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
use spire_core::models::memory_graph::AttrNode;
use tokio::sync::{mpsc, oneshot};

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
        let mut out = Vec::with_capacity(layers.len());
        for n in layers {
            let id = n.id().to_string();
            let features = self.query_nodes("Feature", None).await?;
            let feature_count = features
                .iter()
                .filter(|f| f.get("layer_id").and_then(|v| v.as_str()) == Some(id.as_str()))
                .count() as u64;
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
            });
        }
        Ok(out)
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
        }
    }
}
