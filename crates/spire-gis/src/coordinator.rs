// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! JSON RPC routing — dispatch `gis/*` methods to the actors.
//!
//! Pure async so it is unit-testable without the FFI; the FFI entry wraps this
//! in a tokio `block_on`.

use base64::Engine as _;
use serde_json::{json, Value};
use spire_core::actors::{MemoryGraphMessage, TileFilters, TileMessage};
use spire_core::models::memory_graph::SpatialQuery;
use spire_core::spatial::geo::{Coord, Rect};
use tokio::sync::{mpsc, oneshot};

use crate::actors::import::ImportMessage;
use crate::actors::layer::LayerMessage;
use crate::models::geojson::node_geometry_geojson;
use crate::models::NODE_FEATURE;

fn param<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params.get(key)
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
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    match method {
        "gis/status" => Ok(json!({
            "core": "spire-gis",
            "version": env!("CARGO_PKG_VERSION"),
        })),

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
                let properties =
                    serde_json::to_value(&hit.node.properties).unwrap_or_else(|_| json!({}));
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

            let mut features = Vec::new();
            for node in &nodes {
                let Some(geometry) = node_geometry_geojson(node) else {
                    continue;
                };
                let properties =
                    serde_json::to_value(&node.properties).unwrap_or_else(|_| json!({}));
                features.push(json!({
                    "type": "Feature",
                    "id": node.id(),
                    "properties": properties,
                    "geometry": geometry,
                }));
            }
            Ok(json!({
                "type": "FeatureCollection",
                "features": features,
                "total": features.len(),
                "truncated": features.len() as u32 >= limit,
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
            .send(MemoryGraphMessage::Initialize {
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

    async fn route(
        g: &mpsc::Sender<MemoryGraphMessage>,
        l: &mpsc::Sender<LayerMessage>,
        i: &mpsc::Sender<ImportMessage>,
        t: &mpsc::Sender<TileMessage>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        route_request(g, l, i, t, method, &params).await
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
