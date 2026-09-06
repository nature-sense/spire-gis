// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! ImportActor — ingest GeoJSON (data.gov.sg or a local file) into the GIS
//! store as `Layer` + `Feature` nodes with `CONTAINS` edges.
//!
//! Import is replace-by-name: re-importing the same machine layer name deletes
//! the previous layer + its features first, so it is idempotent and reflects
//! the current file. Writes go through one atomic transaction stream, then a
//! `Sync` snapshot (the debounced snapshot alone is not enough before exit).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use spire_actor::Actor;
use spire_core::actors::MemoryGraphMessage;
use spire_core::models::memory_graph::{
    AttrNode, RelationshipInput, RelationshipType, StreamOp, StreamOpResult, TransactionRequest,
};
use spire_core::spatial::geo::BoundingRect;
use tokio::sync::{mpsc, oneshot};

use crate::models::geojson::{parse_geojson, DecodedFeature};
use crate::models::{
    default_style, feature_node, geometry_kind, layer_node, sanitize_attributes, EDGE_CONTAINS,
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
    async fn import_geojson_text(
        &self,
        machine_name: &str,
        display_name: &str,
        source: &str,
        text: &str,
    ) -> Result<Value, String> {
        let features = parse_geojson(text)?;
        if features.is_empty() {
            return Err("geojson contained no features".to_string());
        }

        // Geometry classification + schema inference across the whole file.
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

        // Build the layer node (fresh id) and stream everything in atomically.
        let layer = layer_node(
            machine_name,
            display_name,
            &format!("Imported from {source}"),
            geometry_type,
            source,
            style,
            schema,
        );
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

    /// Store the layer + every feature inside `txn` and commit. Returns
    /// `(feature_count, bounds)`. Callers roll the stream back on error
    /// (dropping the sender would auto-commit instead).
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

        for (i, feat) in features.iter().enumerate() {
            let source_id = feature_source_id(feat, i);
            let name = feature_name(feat, i);
            let mut node = feature_node(
                &layer_id,
                &source_id,
                &name,
                &feat.geometry,
                &feat.properties,
            );
            // subtype = machine layer name → per-layer tile filters/cache keys.
            node.subtype = Some(machine_name.to_string());
            if let Some(rect) = feat.geometry.bounding_rect() {
                min_lng = min_lng.min(rect.min().x);
                min_lat = min_lat.min(rect.min().y);
                max_lng = max_lng.max(rect.max().x);
                max_lat = max_lat.max(rect.max().y);
            }
            txn.push(StreamOp::StoreNode(node.clone())).await?;
            txn.push(StreamOp::CreateRelationship(RelationshipInput {
                edge_type: RelationshipType::Custom(EDGE_CONTAINS.to_string()),
                from_id: layer_id.clone(),
                to_id: node.id().to_string(),
                properties: None,
                weight: None,
            }))
            .await?;
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
        self.import_geojson_text(&machine, &display_name, "local file", &text)
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
        self.import_geojson_text(&machine, &display, "data.gov.sg", &text)
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
fn infer_schema(features: &[DecodedFeature]) -> Value {
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
