// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! ImportActor — ingest GeoJSON (data.gov.sg or a local file) into the GIS
//! store as `Layer` + `Feature` nodes.
//!
//! Layer membership is encoded on each feature node (`layer_id` property +
//! `subtype` = machine layer name); the GIS query/delete paths filter on those,
//! so no per-feature `CONTAINS` edges are created (they doubled the write cost
//! of 15k-feature imports without being consumed).
//!
//! Import is replace-by-name: re-importing the same machine layer name deletes
//! the previous layer + its features first, so it is idempotent and reflects
//! the current file. Writes go through one atomic transaction stream, then a
//! `Sync` snapshot (the debounced snapshot alone is not enough before exit).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use spire_actor::Actor;
use spire_core::actors::MemoryGraphMessage;
use spire_core::models::memory_graph::{
    AttrNode, StreamOp, StreamOpResult, TransactionRequest,
};
use spire_core::spatial::geo::{
    BoundingRect, Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point,
    Polygon,
};
use tokio::sync::{mpsc, oneshot};

use crate::models::geojson::{parse_geojson_stream, DecodedFeature};
use crate::models::{
    default_style, feature_node, geometry_kind, layer_node, node_search_text, sanitize_attributes,
    NODE_FEATURE, NODE_LAYER,
};

/// A small wrapper over a graph transaction stream.
struct Txn {
    tx: mpsc::Sender<TransactionRequest>,
}

impl Txn {
    /// Await an op's reply (used for open/commit/rollback).
    async fn op(&self, operation: StreamOp) -> Result<StreamOpResult, String> {
        let (t, r) = oneshot::channel();
        self.tx
            .send(TransactionRequest {
                operation,
                reply_to: t,
            })
            .await
            .map_err(|e| format!("txn stream gone: {e}"))?;
        r.await
            .map_err(|e| format!("txn reply lost: {e}"))?
            .map_err(|e| format!("txn op failed: {e}"))
    }

    /// Push an op without waiting for its reply (the graph actor processes ops
    /// in order; sending is flow-controlled by the channel). Bulk imports are
    /// ~50x faster this way — the transaction is confirmed by the Commit.
    async fn push(&self, operation: StreamOp) -> Result<(), String> {
        let (_t, _r) = oneshot::channel();
        self.tx
            .send(TransactionRequest {
                operation,
                reply_to: _t,
            })
            .await
            .map_err(|e| format!("txn stream gone: {e}"))
    }
}

/// Messages for [`ImportActor`].
#[derive(Debug)]
pub enum ImportMessage {
    /// Import a GeoJSON file from disk (new layer, or replace by `name`).
    ImportGeoJsonFile {
        path: PathBuf,
        name: Option<String>,
        display_name: Option<String>,
        reply_to: oneshot::Sender<Result<Value, String>>,
    },
    /// Import a data.gov.sg dataset (GeoJSON download) as a layer.
    ImportDataGovSg {
        dataset_id: String,
        name: Option<String>,
        display_name: Option<String>,
        reply_to: oneshot::Sender<Result<Value, String>>,
    },
    /// Import raw GeoJSON text as a layer (replace-by-name). Used by the
    /// DataSource fetch path so every driver reuses the shared import pipeline.
    ImportGeoJsonText {
        text: String,
        name: String,
        display_name: String,
        source: String,
        reply_to: oneshot::Sender<Result<Value, String>>,
    },
    /// Coalesce fragmented named line/polygon features of an existing layer
    /// into one node per (geometry kind, NAME) — used when the source GeoJSON
    /// is no longer available to re-import. Points and unnamed features are
    /// stored unchanged.
    MergeLayer {
        name: String,
        reply_to: oneshot::Sender<Result<Value, String>>,
    },
}

pub struct ImportActor {
    graph: mpsc::Sender<MemoryGraphMessage>,
}

impl ImportActor {
    pub fn new(graph: mpsc::Sender<MemoryGraphMessage>) -> Self {
        Self { graph }
    }

    async fn query_nodes(
        &self,
        node_type: &str,
        subtype: Option<&str>,
    ) -> Result<Vec<AttrNode>, String> {
        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::QueryAttrNodes {
                node_type: Some(node_type.to_string()),
                subtype: subtype.map(|s| s.to_string()),
                name: None,
                limit: Some(1_000_000),
                reply_to: t,
            })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        let nodes = r
            .await
            .map_err(|e| format!("graph reply lost: {e}"))?
            .map_err(|e| format!("query '{node_type}' failed: {e}"))?;
        Ok(nodes)
    }

    async fn delete_node(&self, id: &str) -> Result<(), String> {
        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::DeleteNode {
                id: id.to_string(),
                reply_to: t,
            })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        r.await
            .map_err(|e| format!("delete reply lost: {e}"))?
            .map_err(|e| format!("delete {id} failed: {e}"))
    }

    /// Remove a layer + its features so an import is replace-by-name.
    async fn replace_layer(&self, name: &str) -> Result<(), String> {
        let layers = self.query_nodes(NODE_LAYER, None).await?;
        let existing = layers.into_iter().find(|n| n.name() == name);
        let Some(layer) = existing else { return Ok(()) };
        let layer_id = layer.id().to_string();
        let features = self.query_nodes(NODE_FEATURE, None).await?;
        for f in features {
            let is_mine = f
                .get("layer_id")
                .and_then(|v| v.as_str())
                .map(|id| id == layer_id.as_str())
                .unwrap_or(false);
            if is_mine {
                self.delete_node(f.id()).await?;
            }
        }
        self.delete_node(&layer_id).await
    }
}

// === PART2 ===

impl ImportActor {
    /// Core pipeline: parse GeoJSON text, (re)build the layer, return a report.
    ///
    /// Decoding uses the memory-bounded `parse_geojson_stream` reader (a
    /// whole-document `serde_json::Value` tree for a 350 MB layer balloons to
    /// several GB and previously OOM'd imports). The raw text is dropped right
    /// after decoding, so only the decoded features stay resident.
    async fn import_geojson_text(
        &self,
        machine_name: &str,
        display_name: &str,
        source: &str,
        text: String,
    ) -> Result<Value, String> {
        let features = parse_geojson_stream(&text, None)?;
        drop(text);
        if features.is_empty() {
            return Err("geojson contained no features".to_string());
        }
        eprintln!("decoded {} features", features.len());

        // Geometry classification + schema inference across all features.
        let mut kinds: Vec<&str> = features
            .iter()
            .map(|f| geometry_kind(&f.geometry))
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        let geometry_type = if kinds.len() == 1 { kinds[0] } else { "Mixed" };
        let schema = infer_schema(&features);
        let style = default_style(geometry_type);

        // Replace any existing layer with the same machine name.
        self.replace_layer(machine_name).await?;

        // New layers stack on top of the existing ones.
        let next_z = self
            .query_nodes(NODE_LAYER, None)
            .await
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| n.get("z_order").and_then(|v| v.as_i64()))
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        // Build the layer node (fresh id) and stream everything in atomically.
        let mut layer = layer_node(
            machine_name,
            display_name,
            &format!("Imported from {source}"),
            geometry_type,
            source,
            style,
            schema,
        );
        layer
            .properties
            .insert("z_order".to_string(), json!(next_z));
        let layer_id = layer.id().to_string();

        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::OpenTransactionStream { reply_to: t })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        let op_tx = r.await.map_err(|e| format!("txn open lost: {e}"))?;
        let txn = Txn { tx: op_tx };
        let result = self
            .write_features(&txn, &layer, machine_name, &features)
            .await;
        let (feature_count, bounds) = match result {
            Ok(x) => x,
            Err(e) => {
                // Roll back — dropping the stream sender would AUTO-COMMIT a
                // half-written layer.
                let _ = txn.op(StreamOp::Rollback).await;
                return Err(e);
            }
        };

        // Persist (debounced snapshots are not flushed before process exit).
        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::Sync { reply_to: t })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        r.await
            .map_err(|e| format!("sync reply lost: {e}"))?
            .map_err(|e| format!("sync failed: {e}"))?;

        // This layer changed — drop any cached display GeoJSON so the next
        // `get-layer-geojson` regenerates it from the new features.
        let _ = std::fs::remove_file(
            crate::config::gis_data_dir().join(format!("display-{machine_name}.geojson")),
        );

        // Semantic search: embed the freshly imported features now (place /
        // district / area names etc. become queryable immediately). Best-effort
        // — tests and embedder-less runs skip this quietly.
        self.embed_layer_features(machine_name).await;

        Ok(json!({
            "layer_id": layer_id,
            "name": machine_name,
            "display_name": display_name,
            "geometry_type": geometry_type,
            "feature_count": feature_count,
            "source": source,
            "bounds": bounds,
        }))
    }

    /// Best-effort embed of one layer's feature nodes so semantic search sees
    /// descriptive text right after import. Degrades silently when the graph
    /// has no embedder (tests) or the model is unavailable.
    async fn embed_layer_features(&self, layer_subtype: &str) {
        let nodes = match self.query_nodes(NODE_FEATURE, Some(layer_subtype)).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("embed: query features for '{layer_subtype}' failed: {e}");
                return;
            }
        };
        if nodes.is_empty() {
            return;
        }

        {
            let (t, r) = oneshot::channel();
            if self
                .graph
                .send(MemoryGraphMessage::EnsureEmbeddingVectorIndex { reply_to: t })
                .await
                .is_err()
            {
                return;
            }
            let _ = r.await;
        }

        let mut embedded: u64 = 0;
        let mut batch: Vec<(String, Vec<f32>)> = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            let end = (i + 64).min(nodes.len());
            let texts: Vec<String> = nodes[i..end].iter().map(node_search_text).collect();
            let (t2, r2) = oneshot::channel();
            if self
                .graph
                .send(MemoryGraphMessage::EmbedTexts { texts, reply_to: t2 })
                .await
                .is_err()
            {
                return;
            }
            let vecs = match r2.await {
                Ok(Ok(v)) => v,
                _ => {
                    eprintln!("embed: no embedder available — skipping layer '{layer_subtype}'");
                    return;
                }
            };
            for (node, vec) in nodes[i..end].iter().zip(vecs.iter()) {
                batch.push((node.id().to_string(), vec.clone()));
                embedded += 1;
                if batch.len() >= 256 {
                    let (t3, r3) = oneshot::channel();
                    if self
                        .graph
                        .send(MemoryGraphMessage::SetNodeEmbeddings {
                            items: std::mem::take(&mut batch),
                            reply_to: t3,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let _ = r3.await;
                }
            }
            i = end;
        }
        if !batch.is_empty() {
            let (t3, r3) = oneshot::channel();
            if self
                .graph
                .send(MemoryGraphMessage::SetNodeEmbeddings {
                    items: batch,
                    reply_to: t3,
                })
                .await
                .is_err()
            {
                return;
            }
            let _ = r3.await;
        }

        // Persist the vectors and refresh the search index (best-effort).
        {
            let (t, r) = oneshot::channel();
            let _ = self
                .graph
                .send(MemoryGraphMessage::RebuildVectorIndexes { reply_to: t })
                .await;
            let _ = r.await;
        }
        {
            let (t, r) = oneshot::channel();
            let _ = self
                .graph
                .send(MemoryGraphMessage::Sync { reply_to: t })
                .await;
            let _ = r.await;
        }
        eprintln!("embedded {embedded} feature node(s) for layer '{layer_subtype}'");
    }

    /// Store the layer + every feature inside `txn` and commit. Returns
    /// `(feature_count, bounds)`. Layer membership is carried on each feature
    /// (`layer_id` + `subtype`), so no per-feature edges are written — that is
    /// what keeps 15k-feature imports tractable. Callers roll the stream back
    /// on error (dropping the sender would auto-commit instead).
    async fn write_features(
        &self,
        txn: &Txn,
        layer: &AttrNode,
        machine_name: &str,
        features: &[DecodedFeature],
    ) -> Result<(u64, Option<[f64; 4]>), String> {
        let layer_id = layer.id().to_string();
        txn.push(StreamOp::StoreNode(layer.clone())).await?;

        let mut min_lng = f64::MAX;
        let mut min_lat = f64::MAX;
        let mut max_lng = f64::MIN;
        let mut max_lat = f64::MIN;
        let mut feature_count: u64 = 0;

        // Named line/polygon fragments (roads split into 2-vertex segments,
        // parks split into adjacent polygons) are coalesced into ONE node per
        // (geometry kind, NAME) so queries return whole shapes. Points and
        // unnamed features are never merged.
        let mut groups: BTreeMap<String, MergeGroup> = BTreeMap::new();
        for (i, feat) in features.iter().enumerate() {
            let source_id = feature_source_id(feat, i);
            let name = feature_name(feat, i);
            let kind = geometry_kind(&feat.geometry);
            let trimmed = name.trim();
            let class = first_attr(&feat.properties, &["FOLDERPATH"])
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let mergeable =
                (kind == "LineString" || kind == "Polygon") && !trimmed.is_empty();
            let key = if mergeable {
                format!("{kind}\u{1}{class}\u{1}{trimmed}")
            } else {
                // Unique per feature → never merged with neighbours.
                format!("solo\u{1}{i}")
            };
            let mut node = feature_node(
                &layer_id,
                &source_id,
                &name,
                &feat.geometry,
                &feat.properties,
            );
            // subtype = machine layer name → per-layer tile filters/cache keys.
            node.subtype = Some(machine_name.to_string());
            let geometry = feat.geometry.clone();
            match groups.entry(key) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(MergeGroup {
                        node,
                        geometries: vec![geometry],
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    e.get_mut().geometries.push(geometry);
                }
            }
        }

        for (_key, mut group) in groups {
            let merged = merge_geometries(&group.geometries);
            group.node.set_spatial_geometry(&merged);
            if let Some(rect) = merged.bounding_rect() {
                min_lng = min_lng.min(rect.min().x);
                min_lat = min_lat.min(rect.min().y);
                max_lng = max_lng.max(rect.max().x);
                max_lat = max_lat.max(rect.max().y);
            }
            txn.push(StreamOp::StoreNode(group.node)).await?;
            feature_count += 1;
        }
        txn.op(StreamOp::Commit).await?;

        let bounds = if min_lng <= max_lng && min_lat <= max_lat {
            Some([min_lng, min_lat, max_lng, max_lat])
        } else {
            None
        };
        Ok((feature_count, bounds))
    }

    /// In-place merge: coalesce fragmented named line/polygon features of an
    /// existing layer (roads split into segments, parks split into polygons)
    /// into one node per NAME. Used when the original GeoJSON source is no
    /// longer on disk to re-import.
    async fn merge_layer(&self, layer_name: &str) -> Result<Value, String> {
        let nodes = self.query_nodes(NODE_FEATURE, Some(layer_name)).await?;
        if nodes.is_empty() {
            return Ok(json!({ "name": layer_name, "deleted": 0, "stored": 0 }));
        }
        let layer_id = first_attr_hm(&nodes[0].properties, &["layer_id"])
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // (geometry kind, class, NAME) → (first node, geometries, source ids)
        let mut groups: BTreeMap<String, (AttrNode, Vec<Geometry<f64>>, Vec<String>)> =
            BTreeMap::new();
        for (i, node) in nodes.iter().enumerate() {
            let Some(geom) = node.spatial_geometry() else {
                continue;
            };
            let kind = geometry_kind(&geom);
            let name = first_attr_hm(&node.properties, &["NAME", "Name"])
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let class = first_attr_hm(&node.properties, &["FOLDERPATH"])
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mergeable = (kind == "LineString" || kind == "Polygon") && !name.is_empty();
            let key = if mergeable {
                format!("{kind}\u{1}{class}\u{1}{name}")
            } else {
                // Unique per feature → never merged.
                format!("solo\u{1}{i}")
            };
            let entry = groups
                .entry(key)
                .or_insert_with(|| (node.clone(), Vec::new(), Vec::new()));
            entry.1.push(geom);
            entry.2.push(node.id().to_string());
        }

        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::OpenTransactionStream { reply_to: t })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        let op_tx = r
            .await
            .map_err(|e| format!("txn open lost: {e}"))?;
        let txn = Txn { tx: op_tx };

        let mut deleted: u64 = 0;
        let mut stored: u64 = 0;
        for (_key, (first, geoms, ids)) in groups {
            let merged = merge_geometries(&geoms);
            let name = match first_attr_hm(&first.properties, &["NAME", "Name"])
                .and_then(|v| v.as_str())
            {
                Some(s) => s.to_string(),
                None => first.name().to_string(),
            };
            for id in &ids {
                txn.push(StreamOp::DeleteNode(id.clone())).await?;
                deleted += 1;
            }
            let attributes: Map<String, Value> = first
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut node = feature_node(&layer_id, "", &name, &merged, &attributes);
            node.subtype = Some(layer_name.to_string());
            txn.push(StreamOp::StoreNode(node)).await?;
            stored += 1;
        }
        txn.op(StreamOp::Commit).await?;

        // Persist, and drop the display cache so it regenerates from the
        // merged shapes.
        {
            let (t2, r2) = oneshot::channel();
            self.graph
                .send(MemoryGraphMessage::Sync { reply_to: t2 })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            let _ = r2
                .await
                .map_err(|e| format!("sync reply lost: {e}"))?
                .map_err(|e| format!("sync failed: {e}"))?;
        }
        let _ = std::fs::remove_file(
            crate::config::gis_data_dir().join(format!("display-{layer_name}.geojson")),
        );

        Ok(json!({ "name": layer_name, "deleted": deleted, "stored": stored }))
    }

    async fn import_file(
        &self,
        path: &PathBuf,
        name: Option<String>,
        display: Option<String>,
    ) -> Result<Value, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "layer".to_string());
        let machine = match name {
            Some(n) => n,
            None => slugify(display.as_deref().unwrap_or(&stem)).unwrap_or(stem),
        };
        let display_name = display
            .unwrap_or_else(|| machine.clone())
            .trim()
            .to_string();
        self.import_geojson_text(&machine, &display_name, "local file", text)
            .await
    }

    async fn import_datagov(
        &self,
        dataset_id: &str,
        name: Option<String>,
        display_name: Option<String>,
    ) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let poll_url = format!(
            "https://api-open.data.gov.sg/v1/public/api/datasets/{dataset_id}/poll-download"
        );
        let mut download_url: Option<String> = None;
        for _ in 0..8 {
            let resp = client
                .get(&poll_url)
                .send()
                .await
                .map_err(|e| format!("data.gov.sg poll failed: {e}"))?;
            let body: Value = resp
                .json()
                .await
                .map_err(|e| format!("data.gov.sg poll body: {e}"))?;
            if let Some(u) = body
                .get("data")
                .and_then(|d| d.get("url"))
                .and_then(|u| u.as_str())
            {
                download_url = Some(u.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let url = download_url
            .ok_or_else(|| "data.gov.sg poll-download never became ready".to_string())?;
        let bytes = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("data.gov.sg download failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("data.gov.sg download body: {e}"))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| format!("dataset is not utf-8 text (zip/shapefile?): {e}"))?
            .to_string();
        let machine = name.unwrap_or_else(|| "data-gov-layer".to_string());
        let display = display_name
            .unwrap_or_else(|| machine.clone())
            .trim()
            .to_string();
        self.import_geojson_text(&machine, &display, "data.gov.sg", text)
            .await
    }
}

// === PART3 ===

#[async_trait]
impl Actor for ImportActor {
    type Message = ImportMessage;

    async fn handle(&mut self, msg: Self::Message) {
        match msg {
            ImportMessage::ImportGeoJsonFile {
                path,
                name,
                display_name,
                reply_to,
            } => {
                let r = self.import_file(&path, name, display_name).await;
                let _ = reply_to.send(r);
            }
            ImportMessage::ImportDataGovSg {
                dataset_id,
                name,
                display_name,
                reply_to,
            } => {
                let r = self.import_datagov(&dataset_id, name, display_name).await;
                let _ = reply_to.send(r);
            }
            ImportMessage::ImportGeoJsonText {
                text,
                name,
                display_name,
                source,
                reply_to,
            } => {
                let r = self.import_geojson_text(&name, &display_name, &source, text).await;
                let _ = reply_to.send(r);
            }
            ImportMessage::MergeLayer { name, reply_to } => {
                let r = self.merge_layer(&name).await;
                let _ = reply_to.send(r);
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn scalar_type(v: &Value) -> Option<&'static str> {
    match v {
        Value::String(_) => Some("string"),
        Value::Number(_) => Some("number"),
        Value::Bool(_) => Some("boolean"),
        _ => None,
    }
}

/// Union of attribute key → type across all features (string/number/boolean).
/// Operates on the sanitized keys so the schema matches what is actually
/// stored on the feature nodes.
pub(crate) fn infer_schema(features: &[DecodedFeature]) -> Value {
    let mut schema: Map<String, Value> = Map::new();
    for f in features {
        for (k, v) in sanitize_attributes(&f.properties) {
            if let Some(t) = scalar_type(&v) {
                schema
                    .entry(k)
                    .and_modify(|e| {
                        if e.as_str() != Some(t) {
                            *e = json!("mixed");
                        }
                    })
                    .or_insert_with(|| json!(t));
            }
        }
    }
    Value::Object(schema)
}

fn first_attr<'a>(props: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| props.get(*k))
}

/// Variant of [`first_attr`] for the `HashMap`-backed `AttrNode::properties`.
fn first_attr_hm<'a>(
    props: &'a HashMap<String, Value>,
    keys: &[&str],
) -> Option<&'a Value> {
    keys.iter().find_map(|k| props.get(*k))
}

/// Prefer OBJECTID for the source id (data.gov.sg national map), else index.
fn feature_source_id(feat: &DecodedFeature, index: usize) -> String {
    first_attr(&feat.properties, &["OBJECTID", "objectid", "FID"])
        .and_then(|v| match v {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| format!("f{index}"))
}

fn feature_name(feat: &DecodedFeature, index: usize) -> String {
    first_attr(&feat.properties, &["NAME", "Name"])
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| feature_source_id(feat, index))
}

/// Accumulator for an import-time coalescing pass: attributes come from the
/// first fragment, geometries from every fragment sharing the merge key.
struct MergeGroup {
    node: AttrNode,
    geometries: Vec<Geometry<f64>>,
}

/// Round a coordinate to a ~0.1 m grid so shared road endpoints map to one node.
fn chain_key(c: Coord) -> (i64, i64) {
    ((c.x * 1_000_000.0).round() as i64, (c.y * 1_000_000.0).round() as i64)
}

fn same_coord(a: Coord, b: Coord) -> bool {
    chain_key(a) == chain_key(b)
}

/// Rebuild continuous road polylines by **degree-2 chain tracing**: each
/// segment is an edge between its two endpoints; a run is traced while its
/// current node has exactly two incident edges (one unambiguous continuation).
/// Junctions (degree ≥ 3) and dead-ends (degree 1) terminate a run, so lines
/// never zig-zag through a fork.
fn trace_chains(parts: Vec<LineString>) -> Vec<LineString> {
    struct Seg {
        a: usize,
        b: usize,
        coords: Vec<Coord>,
    }

    let mut key_to_id: HashMap<(i64, i64), usize> = HashMap::new();
    let mut coords_at: Vec<Coord> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();

    let mut node_of = |c: Coord| -> usize {
        let k = chain_key(c);
        *key_to_id.entry(k).or_insert_with(|| {
            coords_at.push(c);
            coords_at.len() - 1
        })
    };

    for ls in parts {
        let c = ls.0;
        if c.len() < 2 {
            continue;
        }
        let a = node_of(c[0]);
        let b = node_of(*c.last().unwrap());
        if a == b {
            continue; // closed loop with no useful continuation
        }
        segs.push(Seg { a, b, coords: c });
    }

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); coords_at.len()];
    for (i, s) in segs.iter().enumerate() {
        adj[s.a].push(i);
        adj[s.b].push(i);
    }

    let mut used = vec![false; segs.len()];
    let mut chains: Vec<Vec<Coord>> = Vec::new();
    for start in 0..segs.len() {
        if used[start] {
            continue;
        }
        used[start] = true;
        let mut acc = segs[start].coords.clone();
        loop {
            let last = *acc.last().unwrap();
            let Some(&nid) = key_to_id.get(&chain_key(last)) else {
                break;
            };
            if adj[nid].len() != 2 {
                break;
            }
            let candidates: Vec<usize> =
                adj[nid].iter().copied().filter(|&s| !used[s]).collect();
            if candidates.len() != 1 {
                break;
            }
            let nxt = candidates[0];
            used[nxt] = true;
            let coords = segs[nxt].coords.clone();
            let append: Vec<Coord> = if same_coord(coords[0], last) {
                coords[1..].to_vec()
            } else if same_coord(*coords.last().unwrap(), last) {
                let mut r = coords;
                r.reverse();
                r[1..].to_vec()
            } else {
                coords
            };
            if append.is_empty() {
                break;
            }
            acc.extend(append);
        }
        chains.push(acc);
    }
    chains.into_iter().map(LineString).collect()
}

/// Merge homogeneous geometry parts into a single geometry. Roads collapse to
/// a `MultiLineString` (contiguous parts stitched first), area fragments to a
/// `MultiPolygon`, and point sets to a `MultiPoint`; a lone part is returned
/// unchanged.
fn merge_geometries(geoms: &[Geometry<f64>]) -> Geometry<f64> {
    let mut lines: Vec<LineString> = Vec::new();
    let mut polys: Vec<Polygon> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    for g in geoms {
        match g {
            Geometry::Line(l) => lines.push(LineString(vec![l.start, l.end])),
            Geometry::LineString(ls) => lines.push(ls.clone()),
            Geometry::MultiLineString(mls) => lines.extend(mls.0.clone()),
            Geometry::Polygon(p) => polys.push(p.clone()),
            Geometry::MultiPolygon(mp) => polys.extend(mp.0.clone()),
            Geometry::Point(p) => points.push(*p),
            Geometry::MultiPoint(mp) => points.extend(mp.0.iter().copied()),
            _ => {}
        }
    }
    let mut lines = trace_chains(lines);
    match (lines.len(), polys.len(), points.len()) {
        (1, 0, 0) => Geometry::LineString(lines.pop().unwrap()),
        (0, 1, 0) => Geometry::Polygon(polys.pop().unwrap()),
        (0, 0, 1) => Geometry::Point(points.pop().unwrap()),
        (n, 0, 0) if n > 1 => Geometry::MultiLineString(MultiLineString(lines)),
        (0, n, 0) if n > 1 => Geometry::MultiPolygon(MultiPolygon(polys)),
        (0, 0, n) if n > 1 => Geometry::MultiPoint(MultiPoint(points)),
        _ => geoms.first().cloned().unwrap_or(Geometry::Point(Point::new(0.0, 0.0))),
    }
}

fn slugify(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    (!out.is_empty()).then_some(out)
}
