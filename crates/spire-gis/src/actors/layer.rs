// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! LayerActor — catalog of imported GIS layers, backed by the memory graph.
//!
//! Layers are `AttrNode { node_type: "Layer" }` records in the GIS store.
//! This actor answers catalog queries from the UI (`gis/list-layers`). Feature
//! enumeration / import live in `ImportActor` (Phase 1).

use async_trait::async_trait;
use serde::Serialize;
use spire_actor::Actor;
use spire_core::actors::MemoryGraphMessage;
use spire_core::models::memory_graph::{AttrNode, NodeUpdate};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

/// One classification (FOLDERPATH) within a layer, with its feature count.
#[derive(Debug, Clone, Serialize)]
pub struct ClassCount {
    pub key: String,
    pub count: u64,
}

/// Public summary of one layer (mirrors the `gis/list-layers` RPC shape).
#[derive(Debug, Clone, Serialize)]
pub struct LayerInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub geometry_type: String,
    pub source: String,
    pub feature_count: u64,
    /// `[min_lng, min_lat, max_lng, max_lat]` once computed (Phase 1).
    pub bounds: Option<[f64; 4]>,
    /// Distinct `FOLDERPATH` values in this layer + counts (for per-class UI).
    pub classes: Vec<ClassCount>,
    /// Stacking order (higher = drawn on top). `list-layers` sorts by this.
    pub z_order: i64,
}

/// Messages for [`LayerActor`].
#[derive(Debug)]
pub enum LayerMessage {
    /// Every `Layer` node in the store, with feature counts.
    ListLayers {
        reply_to: oneshot::Sender<Result<Vec<LayerInfo>, String>>,
    },
    /// Delete a layer (by id) and every feature it `CONTAINS`.
    DeleteLayer {
        id: String,
        reply_to: oneshot::Sender<Result<(), String>>,
    },
    /// Move a layer one step in the z-order (`"up"` = towards the top) and
    /// reply with the freshly sorted catalog.
    ReorderLayer {
        id: String,
        direction: String,
        reply_to: oneshot::Sender<Result<Vec<LayerInfo>, String>>,
    },
}

/// Actor serving the layer catalog.
pub struct LayerActor {
    graph: mpsc::Sender<MemoryGraphMessage>,
}

impl LayerActor {
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

    async fn list_layers(&self) -> Result<Vec<LayerInfo>, String> {
        let layers = self.query_nodes("Layer", None).await?;
        // Fetch the full feature set once and bucket it by layer_id in a single
        // pass — with many layers this is O(features), not O(layers × features).
        let all_features = self.query_nodes("Feature", None).await?;
        let mut out = Vec::with_capacity(layers.len());
        for n in &layers {
            let id = n.id().to_string();
            let mut feature_count: u64 = 0;
            let mut class_counts: HashMap<String, u64> = HashMap::new();
            for f in &all_features {
                let mine = f.get("layer_id").and_then(|v| v.as_str()) == Some(id.as_str());
                if mine {
                    feature_count += 1;
                    if let Some(cls) = f.get("FOLDERPATH").and_then(|v| v.as_str()) {
                        *class_counts.entry(cls.to_string()).or_insert(0) += 1;
                    }
                }
            }
            let mut classes: Vec<ClassCount> = class_counts
                .into_iter()
                .map(|(key, count)| ClassCount { key, count })
                .collect();
            classes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
            let description = n
                .description
                .clone()
                .or_else(|| {
                    n.get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .unwrap_or_default();
            out.push(LayerInfo {
                id,
                name: n.name().to_string(),
                display_name: n
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                description,
                geometry_type: n
                    .get("geometry_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Mixed")
                    .to_string(),
                source: n
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                feature_count,
                bounds: None,
                classes,
                z_order: n.get("z_order").and_then(|v| v.as_i64()).unwrap_or(0),
            });
        }
        // Bottom-to-top stacking order (higher z drawn on top), ties by name so
        // legacy layers that all share the default 0 are still deterministic.
        out.sort_by(|a, b| {
            a.z_order
                .cmp(&b.z_order)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    /// Move a layer one step in the z-order (`"up"` = towards the top). The
    /// whole sequence is rewritten to a clean `0..n-1` so legacy layers that
    /// all share the default 0 become individually reorderable.
    async fn reorder_layer(&self, id: &str, direction: &str) -> Result<Vec<LayerInfo>, String> {
        let mut nodes = self.query_nodes("Layer", None).await?;
        nodes.sort_by(|a, b| {
            let az = a.get("z_order").and_then(|v| v.as_i64()).unwrap_or(0);
            let bz = b.get("z_order").and_then(|v| v.as_i64()).unwrap_or(0);
            az.cmp(&bz).then_with(|| a.name().cmp(b.name()))
        });
        let Some(pos) = nodes.iter().position(|n| n.id() == id) else {
            return Err(format!("no layer with id '{id}'"));
        };
        let target = match direction {
            "up" => pos + 1,
            "down" => pos.saturating_sub(1),
            other => {
                return Err(format!("direction must be 'up' or 'down', got '{other}'"));
            }
        };
        if target != pos && target < nodes.len() {
            let node = nodes.remove(pos);
            nodes.insert(target, node);
        }
        for (i, node) in nodes.iter().enumerate() {
            let current = node.get("z_order").and_then(|v| v.as_i64()).unwrap_or(0);
            if current == i as i64 {
                continue;
            }
            let mut props: HashMap<String, serde_json::Value> = HashMap::new();
            props.insert("z_order".to_string(), serde_json::json!(i as i64));
            let (t, r) = oneshot::channel();
            self.graph
                .send(MemoryGraphMessage::UpdateNode {
                    id: node.id().to_string(),
                    updates: NodeUpdate {
                        node_type: None,
                        subtype: None,
                        name: None,
                        description: None,
                        properties: Some(props),
                        embedding_id: None,
                    },
                    reply_to: t,
                })
                .await
                .map_err(|e| format!("graph actor gone: {e}"))?;
            r.await
                .map_err(|e| format!("reorder reply lost: {e}"))?
                .map_err(|e| format!("reorder failed: {e}"))?;
        }
        self.list_layers().await
    }

    /// Delete a layer + all features that reference it (by the `layer_id`
    /// property). Relationships are removed with their endpoints.
    async fn delete_layer(&self, id: &str) -> Result<(), String> {
        let features = self.query_nodes("Feature", None).await?;
        for f in features {
            let is_mine = f
                .get("layer_id")
                .and_then(|v| v.as_str())
                .map(|lid| lid == id)
                .unwrap_or(false);
            if is_mine {
                let (t, r) = oneshot::channel();
                self.graph
                    .send(MemoryGraphMessage::DeleteNode {
                        id: f.id().to_string(),
                        reply_to: t,
                    })
                    .await
                    .map_err(|e| format!("graph actor gone: {e}"))?;
                r.await
                    .map_err(|e| format!("delete reply lost: {e}"))?
                    .map_err(|e| format!("delete feature failed: {e}"))?;
            }
        }
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
            .map_err(|e| format!("delete layer failed: {e}"))
    }
}

#[async_trait]
impl Actor for LayerActor {
    type Message = LayerMessage;

    async fn handle(&mut self, msg: Self::Message) {
        match msg {
            LayerMessage::ListLayers { reply_to } => {
                let result = self.list_layers().await;
                let _ = reply_to.send(result);
            }
            LayerMessage::DeleteLayer { id, reply_to } => {
                let result = self.delete_layer(&id).await;
                let _ = reply_to.send(result);
            }
            LayerMessage::ReorderLayer {
                id,
                direction,
                reply_to,
            } => {
                let result = self.reorder_layer(&id, &direction).await;
                let _ = reply_to.send(result);
            }
        }
    }
}
