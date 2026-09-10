// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! JSON RPC routing — dispatch `gis/*` methods to the actors.
//!
//! Pure async so it is unit-testable without the FFI; the FFI entry wraps this
//! in a tokio `block_on`.

use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use spire_core::actors::{LlmMessage, MemoryGraphMessage, TileFilters, TileMessage};
use spire_core::models::memory_graph::{
    AttrNode, FeatureSpec, SearchOptions, SpatialQuery,
};
use spire_core::spatial::geo::{Coord, Point, Rect};
use tokio::sync::{mpsc, oneshot};

use crate::actors::import::ImportMessage;
use crate::actors::layer::LayerMessage;
use crate::datasources::DataSourceMessage;
use crate::models::geojson::{
    decimate_geometry, decode_geometry, geometry_to_geojson, node_geometry_geojson,
};
use crate::models::NODE_FEATURE;
use crate::models::node_search_text;

fn param<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params.get(key)
}

/// Render a stored property scalar as a string (NAME etc. are strings,
/// OBJECTID is a number) with a fallback to empty.
fn scalar_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Render any scalar as its text form for comparison.
fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Evaluate `[{key,op,value}, …]` attribute constraints against a node.
/// A missing key fails the constraint; an empty list always matches.
fn attr_list_match(node: &AttrNode, items: &[Value]) -> bool {
    for it in items {
        let Some(key) = it.get("key").and_then(|v| v.as_str()) else { continue };
        let op = it.get("op").and_then(|v| v.as_str()).unwrap_or("=");
        let Some(val) = node.get(key) else { return false };
        let needle = it.get("value").unwrap_or(&Value::Null);
        let a = value_str(val);
        let b = value_str(needle);
        let pass = match op {
            "=" => a == b,
            "!=" => a != b,
            "contains" => a.contains(&b),
            "starts_with" => a.starts_with(&b),
            ">" | ">=" | "<" | "<=" => match (val.as_f64(), needle.as_f64()) {
                (Some(x), Some(y)) => match op {
                    ">" => x > y,
                    ">=" => x >= y,
                    "<" => x < y,
                    _ => x <= y,
                },
                _ => match a.cmp(&b) {
                    std::cmp::Ordering::Greater => op == ">" || op == ">=",
                    std::cmp::Ordering::Equal => op == ">=" || op == "<=",
                    std::cmp::Ordering::Less => op == "<" || op == "<=",
                },
            },
            _ => false,
        };
        if !pass {
            return false;
        }
    }
    true
}

/// Build one feature GeoJSON value from a stored feature node.
fn feature_json(node: &AttrNode, simplify: Option<f64>, drop_props: bool) -> Option<Value> {
    let geometry = match simplify {
        Some(tol) => node
            .spatial_geometry()
            .as_ref()
            .map(|g| geometry_to_geojson(&decimate_geometry(g, tol))),
        None => node_geometry_geojson(node),
    };
    let geometry = geometry?;
    let all_props = serde_json::to_value(&node.properties).unwrap_or_else(|_| json!({}));
    let mut properties = if drop_props {
        // Display payload stays small, but keeps the fields click-to-select
        // needs: class (colour/type), a display name, an object id, and the
        // node id (the MapLibre feature-state id). Different data.gov.sg
        // datasets name their columns differently, so try a few common keys.
        let name = ["NAME", "name", "PARK_NAME", "AREA_NAME", "TREE_NAME",
                    "TREE_ID", "NAME_NEW", "Description"]
            .iter()
            .find_map(|k| {
                let v = scalar_str(all_props.get(k));
                (!v.is_empty()).then_some(v)
            })
            .unwrap_or_default();
        let objectid = ["OBJECTID", "objectid", "FID", "fid", "ID", "id"]
            .iter()
            .find_map(|k| {
                let v = scalar_str(all_props.get(k));
                (!v.is_empty()).then_some(v)
            })
            .unwrap_or_default();
        json!({
            "class": scalar_str(all_props.get("FOLDERPATH")),
            "name": name,
            "objectid": objectid,
            "id": node.id(),
        })
    } else {
        all_props
    };
    // The stored node carries a redundant `geometry` copy inside its attribute
    // map — drop it (the top-level `geometry` carries it).
    if !drop_props {
        if let Some(obj) = properties.as_object_mut() {
            obj.remove("geometry");
        }
    }
    Some(json!({
        "type": "Feature",
        "id": node.id(),
        "properties": properties,
        "geometry": geometry,
    }))
}

/// Serialize a layer's feature nodes into GeoJSON Features. Geometry decode +
/// JSON construction is CPU-bound, so large layers are built across several
/// scoped threads (the reply still arrives in order).
fn build_layer_features(nodes: &[AttrNode], simplify: Option<f64>, drop_props: bool) -> Vec<Value> {
    let build_one = |n: &[AttrNode]| -> Vec<Value> {
        n.iter().filter_map(|node| feature_json(node, simplify, drop_props)).collect()
    };
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .clamp(1, 8);
    if nodes.len() < 4_000 || workers <= 1 {
        return build_one(nodes);
    }
    std::thread::scope(|scope| {
        let chunk = nodes.len().div_ceil(workers);
        let handles: Vec<_> = nodes
            .chunks(chunk)
            .map(|c| scope.spawn(move || build_one(c)))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Pull the first balanced JSON object out of an LLM reply (tolerating
/// ```json fences and trailing prose).
fn extract_json_object(s: &str) -> Option<Value> {
    let t = s.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    let t = t
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    let mut start = 0usize;
    while let Some(rel) = t[start..].find('{') {
        let s0 = start + rel;
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in t[s0..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(s0 + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(end) = end {
            if let Ok(v) = serde_json::from_str::<Value>(&t[s0..end]) {
                return Some(v);
            }
            start = end;
        } else {
            break;
        }
    }
    None
}

/// Dispatch one `{method, params}` request.
///
/// Returns the method result (the "result" half of the reply envelope).
/// Errors are `Err(String)` — the FFI layer wraps them as `{"ok":false,…}`.
pub async fn route_request(
    graph: &mpsc::Sender<MemoryGraphMessage>,
    layers: &mpsc::Sender<LayerMessage>,
    import: &mpsc::Sender<ImportMessage>,
    tile: &mpsc::Sender<TileMessage>,
    llm: &mpsc::Sender<LlmMessage>,
    datasources: &mpsc::Sender<DataSourceMessage>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    match method {
        "gis/status" => Ok(json!({
            "core": "spire-gis",
            "version": env!("CARGO_PKG_VERSION"),
        })),

        "gis/nl-query" => {
            let text = param(params, "text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if text.is_empty() {
                return Err("nl-query requires 'text'".to_string());
            }

            // Build a compact catalog prompt from the live layer list.
            let (lt, lr) = oneshot::channel();
            layers
                .send(LayerMessage::ListLayers { reply_to: lt })
                .await
                .map_err(|e| format!("layer actor gone: {e}"))?;
            let catalog = lr
                .await
                .map_err(|e| format!("layer reply lost: {e}"))?
                .map_err(|e| format!("list layers: {e}"))?;
            let arr = serde_json::to_value(catalog)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();

            let mut prompt = String::from(
                "You translate a natural-language Singapore map query into one gis/query \
                 request. Respond with ONLY a JSON object: {\"params\": {...}, \"summary\": \"...\"}.\n\n\
                 gis/query params schema:\n\
                 - predicate: \"bbox\" | \"radius\" | \"nearest\" | \"near_class\" | \"near_layer\" \
                 (NEVER use contains/intersects — you cannot supply geometries)\n\
                 - region: ALWAYS include region. For bbox use the \"current view bbox\" value below \
                 when it is a 4-number array, otherwise the whole-of-Singapore default \
                 {\"bbox\":[103.6,1.15,104.1,1.5]}. For radius {\"center\":[lng,lat],\"radius_m\":N}; \
                 for nearest {\"center\":[lng,lat],\"k\":N}; for near_class/near_layer \
                 {\"radius_m\":N,\"reference\":{\"classes\":[...] or \"layers\":[...]}} where \
                 \"classes\" selects the reference feature class(es) (e.g. \"Layers/Major_Road\") and the \
                 top-level \"layers\"/\"classes\" select the TARGET features\n\
                 - layers/classes: arrays of layer/class keys from the catalog below. For a vague \
                 concept like \"wooded recreation area\", pick the matching catalog layers \
                 (e.g. \"parks\", \"nparks-nature-reserves\", \"tree-conservation-area\", \"park-connector-loop\") \
                 and use predicate bbox.\n\
                 - attributes: optional [{\"key\",\"op\",\"value\"}]\n\
                 - limit: 200, output: \"features\"\n\n\
                 Rules: \"parks near a main road\" => predicate near_class, top-level layers [\"parks\"], \
                 region.reference.classes [\"Layers/Major_Road\"], radius 500. Prefer layer/class keys from \
                 the catalog; never invent keys. When the user says \"here/this view\", use the current view \
                 bbox below; otherwise default to the whole of Singapore.\n\n",
            );
            prompt.push_str("Available layers/classes:\n");
            for layer in &arr {
                let name = layer.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let mut line = format!("- layer \"{name}\"");
                if let Some(classes) = layer.get("classes").and_then(|v| v.as_array()) {
                    for c in classes {
                        if let Some(k) = c.get("key").and_then(|v| v.as_str()) {
                            line.push_str(&format!(" class \"{k}\""));
                        }
                    }
                }
                prompt.push_str(&line);
                prompt.push('\n');
            }
            let vp = param(params, "viewport").cloned().unwrap_or(Value::Null);
            let vp_str = vp.to_string();
            prompt.push_str(&format!(
                "\nCurrent view bbox: {vp_str}\nUser query: {text}\n"
            ));

            let (lt2, lr2) = oneshot::channel();
            llm.send(LlmMessage::CompleteDefault {
                prompt,
                reply_to: lt2,
            })
            .await
            .map_err(|e| format!("llm actor gone: {e}"))?;
            let llm_out = lr2
                .await
                .map_err(|e| format!("llm reply lost: {e}"))?
                .map_err(|e| format!("llm error: {e}"))?;

            let Some(parsed) = extract_json_object(&llm_out) else {
                return Err("nl-query: LLM returned no parseable JSON".to_string());
            };
            if parsed.get("fallback").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Ok(json!({ "fallback": true }));
            }
            let Some(params_obj) = parsed.get("params").cloned() else {
                return Err("nl-query: LLM JSON missing 'params'".to_string());
            };
            if !params_obj.get("predicate").and_then(|v| v.as_str()).is_some() {
                return Err("nl-query: LLM params missing 'predicate'".to_string());
            }

            // Execute the produced DSL against the same store and merge the
            // LLM's human summary on top of the structured result.
            let mut result = Box::pin(route_request(
                graph,
                layers,
                import,
                tile,
                llm,
                datasources,
                "gis/query",
                &params_obj,
            ))
            .await?;
            if let Some(obj) = result.as_object_mut() {
                if let Some(summary) = parsed.get("summary").and_then(|v| v.as_str()) {
                    obj.insert("summary".to_string(), json!(summary));
                }
                obj.insert("source".to_string(), json!("llm"));
            }
            Ok(result)
        }

        "gis/list-layers" => {
            let (t, r) = oneshot::channel();
            layers
                .send(LayerMessage::ListLayers { reply_to: t })
                .await
                .map_err(|e| format!("layer actor gone: {e}"))?;
            let info = r
                .await
                .map_err(|e| format!("layer reply lost: {e}"))?
                .map_err(|e| format!("list layers failed: {e}"))?;
            serde_json::to_value(info).map_err(|e| format!("serialize: {e}"))
        }

        "gis/delete-layer" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                return Err("delete-layer requires 'id'".to_string());
            }
            let (t, r) = oneshot::channel();
            layers
                .send(LayerMessage::DeleteLayer { id, reply_to: t })
                .await
                .map_err(|e| format!("layer actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("layer reply lost: {e}"))?
                .map_err(|e| format!("delete layer: {e}"))?;
            Ok(json!({ "deleted": true }))
        }

        "gis/reorder-layer" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "reorder-layer requires 'id'".to_string())?
                .to_string();
            let direction = param(params, "direction")
                .and_then(|v| v.as_str())
                .unwrap_or("up")
                .to_string();
            let (t, r) = oneshot::channel();
            layers
                .send(LayerMessage::ReorderLayer {
                    id,
                    direction,
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("layer actor gone: {e}"))?;
            let info = r
                .await
                .map_err(|e| format!("layer reply lost: {e}"))?
                .map_err(|e| format!("reorder layer: {e}"))?;
            serde_json::to_value(info).map_err(|e| format!("serialize: {e}"))
        }

        "gis/merge-layer" => {
            let name = param(params, "name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                return Err("merge-layer requires 'name'".to_string());
            }
            let (t, r) = oneshot::channel();
            import
                .send(ImportMessage::MergeLayer { name, reply_to: t })
                .await
                .map_err(|e| format!("import actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("merge reply lost: {e}"))?
                .map_err(|e| format!("merge layer: {e}"))
        }

        "gis/import-geojson-file" => {
            let path = param(params, "path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .ok_or_else(|| "import-geojson-file requires 'path'".to_string())?;
            let name = param(params, "name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let display_name = param(params, "display_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let (t, r) = oneshot::channel();
            import
                .send(ImportMessage::ImportGeoJsonFile {
                    path,
                    name,
                    display_name,
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("import actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("import reply lost: {e}"))?
                .map_err(|e| format!("import failed: {e}"))
        }

        "gis/import-datagov" => {
            let dataset_id = param(params, "dataset_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if dataset_id.is_empty() {
                return Err("import-datagov requires 'dataset_id'".to_string());
            }
            let name = param(params, "name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let display_name = param(params, "display_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let (t, r) = oneshot::channel();
            import
                .send(ImportMessage::ImportDataGovSg {
                    dataset_id,
                    name,
                    display_name,
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("import actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("import reply lost: {e}"))?
                .map_err(|e| format!("import failed: {e}"))
        }

        "gis/datasource/kinds" => {
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Kinds { reply_to: t })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let kinds = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource kinds: {e}"))?;
            serde_json::to_value(kinds).map_err(|e| format!("serialize kinds: {e}"))
        }

        "gis/datasource/list" => {
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::List { reply_to: t })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let list = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource list: {e}"))?;
            serde_json::to_value(list).map_err(|e| format!("serialize datasources: {e}"))
        }

        "gis/datasource/get" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/get requires 'id'".to_string())?;
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Get {
                    id: id.to_string(),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let got = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource get: {e}"))?;
            serde_json::to_value(got).map_err(|e| format!("serialize datasource: {e}"))
        }

        "gis/datasource/add" => {
            let kind = param(params, "kind")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/add requires 'kind'".to_string())?
                .to_string();
            let label = param(params, "label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/add requires 'label'".to_string())?
                .to_string();
            let config = param(params, "config").cloned().unwrap_or_else(|| json!({}));
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Add {
                    kind,
                    label,
                    config,
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let added = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource add: {e}"))?;
            serde_json::to_value(added).map_err(|e| format!("serialize datasource: {e}"))
        }

        "gis/datasource/update" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/update requires 'id'".to_string())?
                .to_string();
            let label = param(params, "label").and_then(|v| v.as_str()).map(String::from);
            let config = param(params, "config").cloned();
            let enabled = param(params, "enabled").and_then(|v| v.as_bool());
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Update {
                    id,
                    label,
                    config,
                    enabled,
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let updated = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource update: {e}"))?;
            serde_json::to_value(updated).map_err(|e| format!("serialize datasource: {e}"))
        }

        "gis/datasource/delete" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/delete requires 'id'".to_string())?
                .to_string();
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Delete {
                    id: id.to_string(),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource delete: {e}"))?;
            Ok(json!({ "ok": true }))
        }

        "gis/datasource/discover" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/discover requires 'id'".to_string())?
                .to_string();
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Discover {
                    id: id.to_string(),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            let info = r
                .await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource discover: {e}"))?;
            serde_json::to_value(info).map_err(|e| format!("serialize discovery: {e}"))
        }

        "gis/datasource/fetch" => {
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "datasource/fetch requires 'id'".to_string())?
                .to_string();
            let (t, r) = oneshot::channel();
            datasources
                .send(DataSourceMessage::Fetch {
                    id: id.to_string(),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("datasource actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("datasource reply lost: {e}"))?
                .map_err(|e| format!("datasource fetch: {e}"))
        }

        "gis/get-tile" => {
            let layer = param(params, "layer")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let z = param(params, "z")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "get-tile requires 'z'".to_string())? as u8;
            let x = param(params, "x")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "get-tile requires 'x'".to_string())? as u32;
            let y = param(params, "y")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "get-tile requires 'y'".to_string())? as u32;
            let filters = TileFilters {
                node_type: Some(NODE_FEATURE.to_string()),
                subtype: Some(layer),
                limit: None,
            };
            let (t, r) = oneshot::channel();
            tile.send(TileMessage::GetTile {
                filters,
                z,
                x,
                y,
                reply_to: t,
            })
            .await
            .map_err(|e| format!("tile actor gone: {e}"))?;
            let bytes = r
                .await
                .map_err(|e| format!("tile reply lost: {e}"))?
                .map_err(|e| format!("get-tile: {e}"))?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!({ "tile": encoded, "bytes": bytes.len() }))
        }

        "gis/query" => {
            // Structured spatial-query DSL (predicate + region, optional
            // layer/class/attribute filters, output shape). This is the
            // execution layer a natural-language front end will target later.
            let predicate = param(params, "predicate")
                .and_then(|v| v.as_str())
                .unwrap_or("bbox")
                .to_string();
            let region = param(params, "region").cloned().unwrap_or(Value::Null);
            let layers: Vec<String> = param(params, "layers")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let classes: Vec<String> = param(params, "classes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let attrs = param(params, "attributes").cloned().unwrap_or(Value::Null);
            let has_attrs = matches!(&attrs, Value::Array(a) if !a.is_empty());
            let limit = param(params, "limit")
                .and_then(|v| v.as_u64())
                .map(|l| l.min(10_000) as usize)
                .unwrap_or(200);
            let output = param(params, "output")
                .and_then(|v| v.as_str())
                .unwrap_or("features")
                .to_string();

            let rg = |k: &str| region.get(k).cloned();
            let num_pair = |name: &str| -> Result<(f64, f64), String> {
                let a = rg(name)
                    .and_then(|v| v.as_array().map(|x| x.clone()))
                    .ok_or_else(|| format!("gis/query requires region.{name} [lng, lat]"))?;
                let x = a
                    .first()
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| format!("region.{name}[0] must be a number"))?;
                let y = a
                    .get(1)
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| format!("region.{name}[1] must be a number"))?;
                Ok((x, y))
            };

            // For the "near" predicates the query itself is built inside the
            // join branch below; `sq` is `None` there.
            let sq: Option<SpatialQuery> = match predicate.as_str() {
                "bbox" => {
                    let a = rg("bbox")
                        .and_then(|v| v.as_array().map(|x| x.clone()))
                        .ok_or_else(|| {
                            "gis/query bbox requires region.bbox [minLng,minLat,maxLng,maxLat]"
                                .to_string()
                        })?;
                    let f = |i: usize| -> Result<f64, String> {
                        a.get(i)
                            .and_then(|v| v.as_f64())
                            .ok_or_else(|| "region.bbox must be 4 numbers".to_string())
                    };
                    Some(SpatialQuery::BoundingBox {
                        rect: Rect::new(
                            Coord { x: f(0)?, y: f(1)? },
                            Coord { x: f(2)?, y: f(3)? },
                        ),
                    })
                }
                "radius" => {
                    let (x, y) = num_pair("center")?;
                    let radius_m = rg("radius_m")
                        .and_then(|v| v.as_f64())
                        .ok_or_else(|| "gis/query radius requires region.radius_m".to_string())?;
                    Some(SpatialQuery::Radius {
                        center: Point::new(x, y),
                        radius_meters: radius_m,
                    })
                }
                "nearest" => {
                    let (x, y) = num_pair("center")?;
                    let k = rg("k").and_then(|v| v.as_u64()).unwrap_or(limit as u64) as usize;
                    Some(SpatialQuery::Nearest {
                        center: Point::new(x, y),
                        k: k.max(1),
                    })
                }
                "contains" | "intersects" => {
                    let g = rg("geometry").ok_or_else(|| {
                        "gis/query contains/intersects requires region.geometry".to_string()
                    })?;
                    let geom = decode_geometry(&g).ok_or_else(|| {
                        "region.geometry is not a supported GeoJSON geometry".to_string()
                    })?;
                    if predicate == "contains" {
                        Some(SpatialQuery::Contains { geometry: geom })
                    } else {
                        Some(SpatialQuery::Intersects { geometry: geom })
                    }
                }
                "near" | "near_class" | "near_layer" => None,
                other => return Err(format!("gis/query: unknown predicate '{other}'")),
            };

            // With class/attribute filters we over-fetch, then post-filter.
            let filtered = !classes.is_empty() || has_attrs;
            let internal = if filtered { 50_000usize.min(limit.max(2_000)) } else { limit };

            let near_kind = matches!(predicate.as_str(), "near" | "near_class" | "near_layer");
            let mut hits: Vec<(AttrNode, Option<f64>)> = Vec::new();
            if near_kind {
                // Spatial join: target features within `region.radius_m` of any
                // feature matching `region.reference.{layers,classes}`.
                let refv = region.get("reference").cloned().unwrap_or(Value::Null);
                let ref_layers: Vec<String> = refv
                    .get("layers")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let ref_classes: Vec<String> = refv
                    .get("classes")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if ref_layers.is_empty() && ref_classes.is_empty() {
                    return Err(
                        "gis/query near requires region.reference.{layers|classes}".to_string()
                    );
                }
                let radius_m = rg("radius_m")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| "gis/query near requires region.radius_m".to_string())?;
                let target_spec = FeatureSpec {
                    layers: layers.clone(),
                    classes: classes.clone(),
                };
                let reference_spec = FeatureSpec {
                    layers: ref_layers,
                    classes: ref_classes,
                };
                let (t, r) = oneshot::channel();
                graph
                    .send(MemoryGraphMessage::SpatialQuery {
                        query: SpatialQuery::WithinDistanceOf {
                            reference: reference_spec,
                            target: target_spec,
                            radius_meters: radius_m,
                        },
                        node_type: Some(NODE_FEATURE.to_string()),
                        subtype: None,
                        limit: Some(internal.min(20_000)),
                        reply_to: t,
                    })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                let res = r
                    .await
                    .map_err(|e| format!("query reply lost: {e}"))?
                    .map_err(|e| format!("spatial query: {e}"))?;
                for ds in res.nodes {
                    if !layers.is_empty() {
                        let ok = ds
                            .node
                            .subtype()
                            .map(|s| layers.iter().any(|l| l == s))
                            .unwrap_or(false);
                        if !ok {
                            continue;
                        }
                    }
                    if !classes.is_empty() {
                        let ok = ds
                            .node
                            .get("FOLDERPATH")
                            .and_then(|v| v.as_str())
                            .map(|c| classes.iter().any(|k| k == c))
                            .unwrap_or(false);
                        if !ok {
                            continue;
                        }
                    }
                    if has_attrs {
                        if let Value::Array(items) = &attrs {
                            if !attr_list_match(&ds.node, items) {
                                continue;
                            }
                        }
                    }
                    hits.push((ds.node, ds.distance_meters));
                }
            } else {
                let sq = sq.expect("sq is None only for near predicates");
                let subtypes: Vec<Option<String>> = if layers.is_empty() {
                    vec![None]
                } else {
                    layers.iter().map(|l| Some(l.clone())).collect()
                };
                for subtype in subtypes {
                    let (t, r) = oneshot::channel();
                    graph
                        .send(MemoryGraphMessage::SpatialQuery {
                            query: sq.clone(),
                            node_type: Some(NODE_FEATURE.to_string()),
                            subtype,
                            limit: Some(internal),
                            reply_to: t,
                        })
                        .await
                        .map_err(|e| format!("graph actor gone: {e}"))?;
                    let res = r
                        .await
                        .map_err(|e| format!("query reply lost: {e}"))?
                        .map_err(|e| format!("spatial query: {e}"))?;
                    for ds in res.nodes {
                        if !classes.is_empty() {
                            let ok = ds
                                .node
                                .get("FOLDERPATH")
                                .and_then(|v| v.as_str())
                                .map(|c| classes.iter().any(|k| k == c))
                                .unwrap_or(false);
                            if !ok {
                                continue;
                            }
                        }
                        if has_attrs {
                            if let Value::Array(items) = &attrs {
                                if !attr_list_match(&ds.node, items) {
                                    continue;
                                }
                            }
                        }
                        hits.push((ds.node, ds.distance_meters));
                    }
                }
            }

            let total = hits.len();
            let truncated = hits.len() > limit;
            // Summary counts over the full (pre-truncation) match set.
            let mut by_class: BTreeMap<String, u64> = BTreeMap::new();
            for (node, _) in &hits {
                if let Some(c) = node.get("FOLDERPATH").and_then(|v| v.as_str()) {
                    *by_class.entry(c.to_string()).or_insert(0) += 1;
                }
            }
            if hits.len() > limit {
                hits.truncate(limit);
            }

            let mut features = Vec::new();
            if output != "count" {
                for (node, dist) in &hits {
                    let Some(geometry) = node_geometry_geojson(node) else { continue };
                    let mut props =
                        serde_json::to_value(&node.properties).unwrap_or_else(|_| json!({}));
                    if let Some(obj) = props.as_object_mut() {
                        obj.remove("geometry");
                        // Source layer name so the UI can highlight the result
                        // in its own `spire-<layer>` source + feature id.
                        obj.insert("layer".to_string(), json!(node.subtype().unwrap_or_default()));
                        if let Some(d) = dist {
                            obj.insert("distance_m".to_string(), json!(d));
                        }
                    }
                    features.push(json!({
                        "type": "Feature",
                        "id": node.id(),
                        "properties": props,
                        "geometry": geometry,
                    }));
                }
            }

            Ok(json!({
                "query": {
                    "predicate": predicate,
                    "layers": layers,
                    "classes": classes,
                    "limit": limit,
                    "output": output,
                },
                "total": total,
                "truncated": truncated,
                "by_class": by_class,
                "features": json!({ "type": "FeatureCollection", "features": features }),
            }))
        }

        "gis/semantic-search" => {
            // SeleneDB vector search over embedded GIS nodes (cosine, exact).
            let text = param(params, "text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if text.trim().is_empty() {
                return Err("semantic-search requires 'text'".to_string());
            }
            let scope = param(params, "node_type")
                .and_then(|v| v.as_str())
                .unwrap_or("Feature")
                .to_string();
            let layer = param(params, "layer").and_then(|v| v.as_str()).map(String::from);
            let limit = param(params, "limit")
                .and_then(|v| v.as_u64())
                .map(|l| l.min(200) as usize)
                .unwrap_or(20);

            let (t, r) = oneshot::channel();
            graph
                .send(MemoryGraphMessage::SearchContext {
                    query: text.clone(),
                    options: Some(SearchOptions {
                        top_k: Some(limit.max(50)),
                        ..Default::default()
                    }),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            let res = r
                .await
                .map_err(|e| format!("semantic reply lost: {e}"))?
                .map_err(|e| format!("semantic search: {e}"))?;

            // Keep only nodes of the requested kind/layer and rank by score.
            let mut hits: Vec<(String, f64)> = Vec::new(); // uuid → similarity
            let mut hit_nodes: std::collections::HashMap<String, spire_core::models::memory_graph::AttrNode> =
                std::collections::HashMap::new();
            for sn in res.nodes {
                let kind_ok = if scope == "Layer" {
                    sn.node.node_type == "Layer"
                } else {
                    sn.node.node_type == "Feature"
                };
                if !kind_ok {
                    continue;
                }
                if let Some(l) = &layer {
                    if sn.node.subtype().map(|s| s == l).unwrap_or(false) == false {
                        continue;
                    }
                }
                let id = sn.node.id().to_string();
                hit_nodes.entry(id.clone()).or_insert(sn.node);
                hits.push((id, sn.similarity));
            }
            hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let total = hits.len();
            if hits.len() > limit {
                hits.truncate(limit);
            }

            if scope == "Layer" {
                let mut layers_out = Vec::new();
                for (id, sim) in &hits {
                    if let Some(n) = hit_nodes.get(id) {
                        layers_out.push(json!({
                            "id": n.id(),
                            "name": n.name(),
                            "display_name": n.get("display_name").and_then(|v| v.as_str()).unwrap_or_default(),
                            "similarity": sim,
                        }));
                    }
                }
                Ok(json!({
                    "query": text,
                    "total": total,
                    "layers": layers_out,
                }))
            } else {
                let mut features = Vec::new();
                for (id, sim) in &hits {
                    let Some(node) = hit_nodes.get(id) else { continue };
                    let Some(geometry) = node_geometry_geojson(node) else { continue };
                    let mut props = serde_json::to_value(&node.properties).unwrap_or_else(|_| json!({}));
                    if let Some(obj) = props.as_object_mut() {
                        obj.remove("geometry");
                        obj.insert("similarity".to_string(), json!(sim));
                        obj.insert("layer".to_string(), json!(node.subtype().unwrap_or_default()));
                    }
                    features.push(json!({
                        "type": "Feature",
                        "id": node.id(),
                        "properties": props,
                        "geometry": geometry,
                    }));
                }
                Ok(json!({
                    "query": text,
                    "total": total,
                    "features": json!({ "type": "FeatureCollection", "features": features }),
                }))
            }
        }

        "gis/reindex-embeddings" => {
            // One-time backfill: embed Layer + Feature nodes so semantic search
            // works for data imported before embeddings were enabled.
            let mut embedded: u64 = 0;
            // Register the node-embedding vector index first so subsequent
            // numeric-list SETs are stored as typed, searchable vectors.
            {
                let (ti, ri) = oneshot::channel();
                graph
                    .send(MemoryGraphMessage::EnsureEmbeddingVectorIndex { reply_to: ti })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                ri.await
                    .map_err(|e| format!("vector-index reply lost: {e}"))?
                    .map_err(|e| format!("vector-index failed: {e}"))?;
            }
            for kind in ["Feature", "Layer"] {
                let (t, r) = oneshot::channel();
                graph
                    .send(MemoryGraphMessage::QueryAttrNodes {
                        node_type: Some(kind.to_string()),
                        subtype: None,
                        name: None,
                        limit: Some(1_000_000),
                        reply_to: t,
                    })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                let nodes = r
                    .await
                    .map_err(|e| format!("query reply lost: {e}"))?
                    .map_err(|e| format!("query {kind}: {e}"))?;
                let mut i = 0;
                while i < nodes.len() {
                    let end = (i + 64).min(nodes.len());
                    let texts: Vec<String> = nodes[i..end].iter().map(node_search_text).collect();
                    let (t2, r2) = oneshot::channel();
                    graph
                        .send(MemoryGraphMessage::EmbedTexts {
                            texts,
                            reply_to: t2,
                        })
                        .await
                        .map_err(|e| format!("graph actor gone: {e}"))?;
                    let vecs = r2
                        .await
                        .map_err(|e| format!("embed reply lost: {e}"))?
                        .map_err(|e| format!("embed failed: {e}"))?;
                    let mut batch: Vec<(String, Vec<f32>)> = Vec::new();
                    for (node, vec) in nodes[i..end].iter().zip(vecs.iter()) {
                        batch.push((node.id().to_string(), vec.clone()));
                        embedded += 1;
                        if batch.len() >= 256 {
                            let (t3, r3) = oneshot::channel();
                            graph
                                .send(MemoryGraphMessage::SetNodeEmbeddings {
                                    items: std::mem::take(&mut batch),
                                    reply_to: t3,
                                })
                                .await
                                .map_err(|e| format!("graph actor gone: {e}"))?;
                            r3.await
                                .map_err(|e| format!("set-embeddings reply lost: {e}"))?
                                .map_err(|e| format!("set-embeddings failed: {e}"))?;
                        }
                    }
                    if !batch.is_empty() {
                        let (t3, r3) = oneshot::channel();
                        graph
                            .send(MemoryGraphMessage::SetNodeEmbeddings {
                                items: batch,
                                reply_to: t3,
                            })
                            .await
                            .map_err(|e| format!("graph actor gone: {e}"))?;
                        r3.await
                            .map_err(|e| format!("set-embeddings reply lost: {e}"))?
                            .map_err(|e| format!("set-embeddings failed: {e}"))?;
                    }
                    i = end;
                }
            }
            // Persist the embeddings before returning (the write burst above
            // keeps the snapshot debounce from ever firing on its own).
            {
                let (tr, rr) = oneshot::channel();
                graph
                    .send(MemoryGraphMessage::RebuildVectorIndexes { reply_to: tr })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                rr.await
                    .map_err(|e| format!("rebuild reply lost: {e}"))?
                    .map_err(|e| format!("rebuild failed: {e}"))?;
            }
            {
                let (ts, rs) = oneshot::channel();
                graph
                    .send(MemoryGraphMessage::Sync { reply_to: ts })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                let _ = rs
                    .await
                    .map_err(|e| format!("sync reply lost: {e}"))?
                    .map_err(|e| format!("sync failed: {e}"))?;
            }
            Ok(json!({ "embedded": embedded }))
        }

        "gis/spatial-query" => {
            let f = |k: &str| -> Result<f64, String> {
                param(params, k)
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| format!("spatial-query requires '{k}'"))
            };
            let min_lng = f("min_lng")?;
            let min_lat = f("min_lat")?;
            let max_lng = f("max_lng")?;
            let max_lat = f("max_lat")?;
            let layer = param(params, "layer")
                .and_then(|v| v.as_str())
                .map(String::from);
            let limit = param(params, "limit")
                .and_then(|v| v.as_u64())
                .map(|l| l.min(10_000) as usize)
                .unwrap_or(1_000);

            let (t, r) = oneshot::channel();
            graph
                .send(MemoryGraphMessage::SpatialQuery {
                    query: SpatialQuery::BoundingBox {
                        rect: Rect::new(
                            Coord {
                                x: min_lng,
                                y: min_lat,
                            },
                            Coord {
                                x: max_lng,
                                y: max_lat,
                            },
                        ),
                    },
                    node_type: Some(NODE_FEATURE.to_string()),
                    subtype: layer,
                    limit: Some(limit),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            let result = r
                .await
                .map_err(|e| format!("spatial reply lost: {e}"))?
                .map_err(|e| format!("spatial query: {e}"))?;

            let mut features = Vec::new();
            for hit in result.nodes {
                let Some(geometry) = node_geometry_geojson(&hit.node) else {
                    continue;
                };
                let mut properties =
                    serde_json::to_value(&hit.node.properties).unwrap_or_else(|_| json!({}));
                if let Some(obj) = properties.as_object_mut() {
                    obj.insert("layer".to_string(), json!(hit.node.subtype().unwrap_or_default()));
                }
                features.push(json!({
                    "type": "Feature",
                    "id": hit.node.id(),
                    "properties": properties,
                    "geometry": geometry,
                }));
            }
            Ok(json!({
                "type": "FeatureCollection",
                "features": features,
                "total": result.total_results,
                "truncated": result.truncated,
            }))
        }

        "gis/get-layer-geojson" => {
            let layer = param(params, "layer")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if layer.is_empty() {
                return Err("get-layer-geojson requires 'layer'".to_string());
            }
            let limit = param(params, "limit")
                .and_then(|v| v.as_u64())
                .map(|l| l.min(100_000) as u32)
                .unwrap_or(10_000);
            // Optional display decimation (degrees) — see `decimate_geometry`.
            let simplify = param(params, "simplify")
                .and_then(|v| v.as_f64())
                .filter(|t| t.is_finite() && *t > 0.0);
            let drop_props = param(params, "drop_props")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Large display layers are cached on disk (`display-<name>.geojson`
            // in the store dir; invalidated by re-import). Building 16k features
            // takes ~10 s of geometry decode, so a cache hit makes launches
            // instant.
            if simplify.is_some() && drop_props {
                let path = crate::config::gis_data_dir().join(format!("display-{layer}.geojson"));
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(cached) = serde_json::from_slice::<Value>(&bytes) {
                        return Ok(cached);
                    }
                }
            }

            let (t, r) = oneshot::channel();
            graph
                .send(MemoryGraphMessage::QueryAttrNodes {
                    node_type: Some(NODE_FEATURE.to_string()),
                    subtype: Some(layer.clone()),
                    name: None,
                    limit: Some(limit),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            let nodes = r
                .await
                .map_err(|e| format!("query reply lost: {e}"))?
                .map_err(|e| format!("query layer '{layer}': {e}"))?;

            let drop_props = param(params, "drop_props")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let features = build_layer_features(&nodes, simplify, drop_props);
            let fc = json!({
                "type": "FeatureCollection",
                "features": features,
                "total": features.len(),
                "truncated": features.len() as u32 >= limit,
            });
            // Populate the display cache for the next launch (best-effort).
            if simplify.is_some() && drop_props {
                if let Ok(text) = serde_json::to_string(&fc) {
                    let path = crate::config::gis_data_dir().join(format!("display-{layer}.geojson"));
                    let _ = std::fs::write(path, text);
                }
            }
            Ok(fc)
        }

        "gis/get-feature" => {
            // Full attribute map + geometry for a single stored feature (used
            // by the click-to-inspect panel). Selection RPCs stay cheap: the
            // map payload never carries full properties.
            let id = param(params, "id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                return Err("get-feature requires 'id'".to_string());
            }
            let (t, r) = oneshot::channel();
            graph
                .send(MemoryGraphMessage::GetAttrNode {
                    id: id.clone(),
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            let node = r
                .await
                .map_err(|e| format!("get-feature reply lost: {e}"))?
                .map_err(|e| format!("get-feature failed: {e}"))?
                .ok_or_else(|| format!("no feature with id '{id}'"))?;
            if node.node_type != NODE_FEATURE {
                return Err(format!("node '{id}' is not a feature"));
            }
            let mut attrs = serde_json::to_value(&node.properties).unwrap_or_else(|_| json!({}));
            if let Some(obj) = attrs.as_object_mut() {
                // Internal plumbing — the UI filters OBJECTID/FID client-side.
                obj.remove("geometry");
                obj.remove("layer_id");
                obj.remove("source_id");
            }
            Ok(json!({
                "id": node.id(),
                "layer": node.subtype().unwrap_or_default(),
                "name": node.name(),
                "attributes": attrs,
            }))
        }

        other => Err(format!("unknown method: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spire_actor::Actor;
    use spire_core::actors::{MemoryGraphActor, MemoryGraphMessage, TileActor};
    use tokio::sync::oneshot;

    use crate::actors::import::ImportActor;
    use crate::actors::layer::LayerActor;
    const SAMPLE_GEOJSON: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {"type":"Feature","properties":{"NAME":"Zone A","OBJECTID":1},
         "geometry":{"type":"Polygon","coordinates":[[[103.70,1.20],[103.90,1.20],[103.90,1.40],[103.70,1.40],[103.70,1.20]]]}},
        {"type":"Feature","properties":{"NAME":"Zone B","OBJECTID":2},
         "geometry":{"type":"Polygon","coordinates":[[[103.90,1.20],[104.10,1.20],[104.10,1.40],[103.90,1.40],[103.90,1.20]]]}},
        {"type":"Feature","properties":{"NAME":"Zone C","OBJECTID":3},
         "geometry":{"type":"Polygon","coordinates":[[[103.70,1.40],[103.90,1.40],[103.90,1.55],[103.70,1.55],[103.70,1.40]]]}}
      ]
    }"#;

    /// Throwaway store + layer/import/tile actors in a temp dir.
    async fn harness(
        dir: &std::path::Path,
    ) -> (
        mpsc::Sender<MemoryGraphMessage>,
        mpsc::Sender<LayerMessage>,
        mpsc::Sender<ImportMessage>,
        mpsc::Sender<TileMessage>,
    ) {
        let (graph_tx, rx) = mpsc::channel(64);
        let _join = MemoryGraphActor::new().spawn(rx);
        let (t, r) = oneshot::channel();
        graph_tx
            .send(MemoryGraphMessage::InitializeInMemory {
                data_dir: dir.to_path_buf(),
                reply_to: t,
            })
            .await
            .unwrap();
        r.await.unwrap().expect("store init");

        let (layer_tx, lrx) = mpsc::channel(64);
        let _ljoin = LayerActor::new(graph_tx.clone()).spawn(lrx);
        let (import_tx, irx) = mpsc::channel(64);
        let _ijoin = ImportActor::new(graph_tx.clone()).spawn(irx);
        let (tile_tx, trx) = mpsc::channel(64);
        let _tjoin = TileActor::new(graph_tx.clone()).spawn(trx);
        (graph_tx, layer_tx, import_tx, tile_tx)
    }

    fn dummy_datasources() -> mpsc::Sender<DataSourceMessage> {
        let (tx, _rx) = mpsc::channel(8);
        tx
    }

    async fn route(
        g: &mpsc::Sender<MemoryGraphMessage>,
        l: &mpsc::Sender<LayerMessage>,
        i: &mpsc::Sender<ImportMessage>,
        t: &mpsc::Sender<TileMessage>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let (llm_tx, _llm_rx) = mpsc::channel::<LlmMessage>(4);
        route_request(g, l, i, t, &llm_tx, &dummy_datasources(), method, &params).await
    }

    #[tokio::test]
    async fn status_reports_core() {
        let dir = tempfile::tempdir().unwrap();
        let (g, l, i, t) = harness(dir.path()).await;
        let value = route(&g, &l, &i, &t, "gis/status", json!({}))
            .await
            .expect("status");
        assert_eq!(value["core"], "spire-gis");
        assert!(value["version"].as_str().unwrap_or_default().len() > 0);
    }

    #[tokio::test]
    async fn list_layers_empty_on_fresh_store() {
        let dir = tempfile::tempdir().unwrap();
        let (g, l, i, t) = harness(dir.path()).await;
        let value = route(&g, &l, &i, &t, "gis/list-layers", json!({}))
            .await
            .expect("list layers");
        assert!(value.as_array().expect("array").is_empty());
    }

    #[tokio::test]
    async fn unknown_method_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (g, l, i, t) = harness(dir.path()).await;
        let err = route(&g, &l, &i, &t, "gis/nope", json!({}))
            .await
            .expect_err("must fail");
        assert!(err.contains("unknown method"), "got {err}");
    }

    #[tokio::test]
    async fn import_lists_tiles_and_spatial_query() {
        let dir = tempfile::tempdir().unwrap();
        let (g, l, i, t) = harness(dir.path()).await;
        let geojson_path = dir.path().join("sample.geojson");
        std::fs::write(&geojson_path, SAMPLE_GEOJSON).unwrap();

        // 1. Import the sample file as layer "demo".
        let report = route(
            &g,
            &l,
            &i,
            &t,
            "gis/import-geojson-file",
            json!({ "path": geojson_path, "name": "demo", "display_name": "Demo" }),
        )
        .await
        .expect("import");
        assert_eq!(report["name"], "demo");
        assert_eq!(report["feature_count"], 3);
        assert_eq!(report["geometry_type"], "Polygon");

        // 2. list-layers shows one layer with 3 features.
        let layers = route(&g, &l, &i, &t, "gis/list-layers", json!({}))
            .await
            .expect("list");
        let arr = layers.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{arr:?}");
        assert_eq!(arr[0]["name"], "demo");
        assert_eq!(arr[0]["feature_count"], 3);

        // 3. Re-importing the same name replaces, never duplicates.
        let report2 = route(
            &g,
            &l,
            &i,
            &t,
            "gis/import-geojson-file",
            json!({ "path": geojson_path, "name": "demo", "display_name": "Demo" }),
        )
        .await
        .expect("reimport");
        assert_ne!(report2["layer_id"], report["layer_id"], "fresh layer id");
        let layers2 = route(&g, &l, &i, &t, "gis/list-layers", json!({}))
            .await
            .expect("list2");
        let arr2 = layers2.as_array().unwrap();
        assert_eq!(arr2.len(), 1, "reimport must not duplicate: {arr2:?}");
        assert_eq!(arr2[0]["feature_count"], 3);

        // 4. Vector tile covering the world (z0) contains the features.
        let tile = route(
            &g,
            &l,
            &i,
            &t,
            "gis/get-tile",
            json!({ "layer": "demo", "z": 0, "x": 0, "y": 0 }),
        )
        .await
        .expect("tile");
        assert!(tile["bytes"].as_u64().unwrap() > 0, "tile has content");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(tile["tile"].as_str().expect("tile base64"))
            .expect("base64 decode");
        assert!(!decoded.is_empty());

        // 5. Bounding-box spatial query returns all three features.
        let fc = route(
            &g,
            &l,
            &i,
            &t,
            "gis/spatial-query",
            json!({ "min_lng": 103.5, "min_lat": 1.0, "max_lng": 104.3, "max_lat": 1.8, "layer": "demo" }),
        )
        .await
        .expect("bbox");
        assert_eq!(fc["total"], 3, "{fc:?}");
        assert_eq!(fc["features"].as_array().unwrap().len(), 3);

        // 6. Delete the current layer → empty catalog.
        let current_id = arr2[0]["id"].as_str().expect("layer id").to_string();
        route(
            &g,
            &l,
            &i,
            &t,
            "gis/delete-layer",
            json!({ "id": current_id }),
        )
        .await
        .expect("delete");
        let after = route(&g, &l, &i, &t, "gis/list-layers", json!({}))
            .await
            .expect("list-after");
        assert!(after.as_array().unwrap().is_empty());
    }

}
