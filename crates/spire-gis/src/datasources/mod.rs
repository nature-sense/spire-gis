// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! Data Sources — configurable connectors that fetch external datasets into
//! GIS layers.
//!
//! A **data source** is a persisted, user-editable *definition* (driver `kind`
//! + provider-specific `config`), distinct from the **layers** it produces on
//! import. One driver family (data.gov.sg today; OSM, weather, pm2.5 later)
//! implements discovery (metadata + attribute schema, no import) and fetching
//! (raw GeoJSON text handed to the shared import pipeline).
//!
//! Prototype layout:
//! - [`DataSourceDriver`] — the connector trait (kind/discover/fetch).
//! - [`DriverRegistry`] — `kind → driver` map with a `builtin()` set.
//! - [`DataSource`] — the persisted definition (normalized graph nodes: scalar
//!   properties for identity/config/summary, plus one `DataSourceAttribute`
//!   node per discovered schema attribute — never a JSON blob).
//! - [`GraphDataSourceStore`] — async reads/writes against the memory graph.
//!
//! Wiring note: a future `DataSourceActor` will own the store + dispatch CRUD /
//! discover / fetch to drivers and hand fetched payloads to `ImportActor`'s
//! shared pipeline.

pub mod actor;
pub mod data_gov_sg;

pub use actor::{DataSourceActor, DataSourceMessage};

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use spire_core::actors::MemoryGraphMessage;
use spire_core::models::memory_graph::AttrNode;
use tokio::sync::{mpsc, oneshot};

use crate::models::geojson::parse_geojson;

// ============================================================================
// Definition model
// ============================================================================

/// How often a data source is (re-)fetched. Manual for now; interval refresh
/// is scheduled by a future scheduler actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RefreshPolicy {
    #[default]
    Manual,
    // Future: Interval { seconds: u64 },
}

fn default_true() -> bool {
    true
}

/// A persisted, user-configurable data source definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSource {
    pub id: String,
    /// Driver kind, e.g. `"data-gov-sg"`, `"osm"`.
    pub kind: String,
    /// Human label shown in the UI.
    pub label: String,
    /// Provider-specific configuration (e.g. `{ "dataset_id": "d_…" }`).
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub refresh: RefreshPolicy,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Discovery cache from the last `discover`. Persisted normalized: scalar
    /// properties on this node + one `DataSourceAttribute` node per schema
    /// attribute — never a JSON blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered: Option<Discovery>,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub updated_at: String,
}

impl DataSource {
    /// Build a fresh definition (uuid id + timestamps).
    pub fn new(kind: impl Into<String>, label: impl Into<String>, config: Value) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            label: label.into(),
            config,
            refresh: RefreshPolicy::Manual,
            enabled: true,
            discovered: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Mark a successful discover/fetch: refresh the cached info + timestamp.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

// ============================================================================
// Discovery / fetch contracts
// ============================================================================

/// Metadata discovered about a dataset without importing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetInfo {
    pub name: String,
    pub description: String,
    /// Distinct geometry kinds present, e.g. `["LineString"]`.
    pub geometry_types: Vec<String>,
    pub feature_count: usize,
    /// Attribute schema `{ attr → "string"|"number"|"boolean"|"mixed" }` over
    /// sanitized keys (matches what would actually be stored on feature nodes).
    pub schema: Value,
}

/// Cached discovery summary carried on a [`DataSource`]. Persisted normalized:
/// `feature_count` / `geometry_types` as scalar node properties and `schema`
/// decomposed into one `DataSourceAttribute` node per attribute (reassembled on
/// read).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub feature_count: usize,
    /// Distinct geometry kinds, e.g. `["Point", "LineString"]`.
    pub geometry_types: Vec<String>,
    /// `{ attr → "string"|"number"|"boolean"|"mixed" }`.
    pub schema: Value,
}

impl Discovery {
    pub fn from_info(info: &DatasetInfo) -> Self {
        Self {
            feature_count: info.feature_count,
            geometry_types: info.geometry_types.clone(),
            schema: info.schema.clone(),
        }
    }
}

/// Raw payload a driver's `fetch` returns, ready for the shared import
/// pipeline. Prototype only supports GeoJSON text.
#[derive(Debug, Clone)]
pub enum DatasetPayload {
    /// Complete GeoJSON document as text (FeatureCollection / Feature).
    GeoJsonText(String),
}

/// A connector that discovers + fetches one provider family's datasets.
#[async_trait]
pub trait DataSourceDriver: Send + Sync {
    /// Stable driver id, e.g. `"data-gov-sg"`.
    fn kind(&self) -> &'static str;

    /// Validate `config` and return dataset metadata + inferred attribute
    /// schema (downloads, but never imports/stores anything).
    async fn discover(&self, config: &Value) -> Result<DatasetInfo, String>;

    /// Fetch the raw dataset payload for import.
    async fn fetch(&self, config: &Value) -> Result<DatasetPayload, String>;
}

// ============================================================================
// Driver registry
// ============================================================================

/// `kind → driver` registry.
#[derive(Default)]
pub struct DriverRegistry {
    drivers: HashMap<&'static str, Arc<dyn DataSourceDriver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver, rejecting a duplicate kind.
    pub fn register(&mut self, driver: Arc<dyn DataSourceDriver>) -> Result<(), String> {
        let kind = driver.kind();
        if self.drivers.contains_key(kind) {
            return Err(format!("data source driver '{kind}' already registered"));
        }
        self.drivers.insert(kind, driver);
        Ok(())
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn DataSourceDriver>> {
        self.drivers.get(kind).cloned()
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self.drivers.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    /// The built-in driver set (prototype: data.gov.sg).
    pub fn builtin() -> Self {
        let mut reg = Self::new();
        let _ = reg.register(std::sync::Arc::new(data_gov_sg::DataGovSgDriver));
        reg
    }
}

// ============================================================================
// Analysis helper (shared by drivers' discover + usable offline)
// ============================================================================

/// Parse a GeoJSON document and build a [`DatasetInfo`] (schema via the same
/// inference the import path uses, so discovery previews exactly what will be
/// stored).
pub fn analyze_geojson(
    text: &str,
    name: String,
    description: String,
) -> Result<DatasetInfo, String> {
    let features = parse_geojson(text)?;
    let schema = crate::actors::import::infer_schema(&features);
    let mut geometry_types: Vec<String> = features
        .iter()
        .map(|f| crate::models::geometry_kind(&f.geometry).to_string())
        .collect();
    geometry_types.sort_unstable();
    geometry_types.dedup();
    Ok(DatasetInfo {
        name,
        description,
        geometry_types,
        feature_count: features.len(),
        schema,
    })
}


// ============================================================================
// Normalized node codec + graph store (replaces the JSON-file store)
// ============================================================================


// ============================================================================
// Normalized node codec — no JSON blobs. Every DataSource attribute is either
// a scalar node property (`config_*`, `enabled`, `refresh_mode`,
// `feature_count`, `geometry_types`) or its own `DataSourceAttribute` node
// (the discovered schema), mirroring normalized tables in SQL.
// ============================================================================

/// `node_type` of a DataSource definition node.
pub const SOURCE_NODE_TYPE: &str = "DataSource";
/// `node_type` of one discovered schema attribute (child of a DataSource).
pub const ATTR_NODE_TYPE: &str = "DataSourceAttribute";

const PREFIX_CONFIG: &str = "config_";
const PROP_REFRESH_MODE: &str = "refresh_mode";
const PROP_ENABLED: &str = "enabled";
const PROP_FEATURE_COUNT: &str = "feature_count";
const PROP_GEOMETRY_TYPES: &str = "geometry_types";
const PROP_SOURCE_ID: &str = "source_id";
const PROP_DATA_TYPE: &str = "data_type";

fn ts(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap_or_else(|_| chrono::Utc::now())
}

fn scalar_value(v: &Value) -> Option<Value> {
    match v {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Some(v.clone()),
        _ => None,
    }
}

/// Encode a DataSource as its `DataSource` graph node.
pub fn datasource_to_node(src: &DataSource) -> AttrNode {
    let mut props: HashMap<String, Value> = HashMap::new();
    props.insert(PROP_ENABLED.to_string(), Value::Bool(src.enabled));
    props.insert(
        PROP_REFRESH_MODE.to_string(),
        Value::String(match src.refresh {
            RefreshPolicy::Manual => "manual".to_string(),
        }),
    );
    // Provider config → one scalar property per key (e.g. `config_dataset_id`).
    if let Some(obj) = src.config.as_object() {
        for (k, v) in obj {
            if let Some(v) = scalar_value(v) {
                props.insert(format!("{PREFIX_CONFIG}{k}"), v);
            }
        }
    }
    // Discovery summary scalars (schema itself is stored as attribute nodes).
    if let Some(d) = &src.discovered {
        props.insert(
            PROP_FEATURE_COUNT.to_string(),
            Value::Number(serde_json::Number::from(d.feature_count as u64)),
        );
        props.insert(
            PROP_GEOMETRY_TYPES.to_string(),
            Value::String(d.geometry_types.join(",")),
        );
    }
    AttrNode {
        id: src.id.clone(),
        node_type: SOURCE_NODE_TYPE.to_string(),
        subtype: Some(src.kind.clone()),
        name: src.label.clone(),
        description: None,
        properties: props,
        embedding_id: None,
        created_at: ts(&src.created_at),
        updated_at: ts(&src.updated_at),
        version: 1,
    }
}

/// Encode the discovered schema as one `DataSourceAttribute` node per attribute.
pub fn schema_to_nodes(src: &DataSource) -> Vec<AttrNode> {
    let mut out = Vec::new();
    let Some(d) = &src.discovered else { return out };
    let created = ts(&src.created_at);
    let Some(schema) = d.schema.as_object() else { return out };
    for (key, ty) in schema {
        let mut props: HashMap<String, Value> = HashMap::new();
        props.insert(PROP_SOURCE_ID.to_string(), Value::String(src.id.clone()));
        if let Some(ty) = ty.as_str() {
            props.insert(PROP_DATA_TYPE.to_string(), Value::String(ty.to_string()));
        }
        out.push(AttrNode {
            id: uuid::Uuid::new_v4().to_string(),
            node_type: ATTR_NODE_TYPE.to_string(),
            subtype: Some(src.kind.clone()),
            name: key.clone(),
            description: None,
            properties: props,
            embedding_id: None,
            created_at: created,
            updated_at: created,
            version: 1,
        });
    }
    out
}


fn prop_str(props: &HashMap<String, Value>, key: &str) -> String {
    props
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Reassemble a DataSource from its source node + schema attribute nodes.
pub fn datasource_from_nodes(
    node: &AttrNode,
    schema_nodes: &[AttrNode],
) -> Result<DataSource, String> {
    let mut config = Map::new();
    for (k, v) in &node.properties {
        if let Some(rest) = k.strip_prefix(PREFIX_CONFIG) {
            config.insert(rest.to_string(), v.clone());
        }
    }
    let refresh = match prop_str(&node.properties, PROP_REFRESH_MODE).as_str() {
        "manual" => RefreshPolicy::Manual,
        other => {
            return Err(format!(
                "unknown refresh_mode '{other}' on datasource '{}'",
                node.name()
            ))
        }
    };
    let enabled = node
        .properties
        .get(PROP_ENABLED)
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let feature_count = node
        .properties
        .get(PROP_FEATURE_COUNT)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let geometry_types: Vec<String> = prop_str(&node.properties, PROP_GEOMETRY_TYPES)
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut discovered = None;
    if feature_count.is_some() || !geometry_types.is_empty() || !schema_nodes.is_empty() {
        let mut schema = Map::new();
        for a in schema_nodes {
            let ty = prop_str(&a.properties, PROP_DATA_TYPE);
            schema.insert(a.name().to_string(), Value::String(ty));
        }
        discovered = Some(Discovery {
            feature_count: feature_count.unwrap_or(0),
            geometry_types,
            schema: Value::Object(schema),
        });
    }
    Ok(DataSource {
        id: node.id().to_string(),
        kind: node.subtype().unwrap_or_default().to_string(),
        label: node.name().to_string(),
        config: Value::Object(config),
        refresh,
        enabled,
        discovered,
        created_at: node.created_at.to_rfc3339(),
        updated_at: node.updated_at.to_rfc3339(),
    })
}


// ============================================================================
// Graph-backed store (async; owned by the future DataSourceActor)
// ============================================================================

async fn graph_query_nodes(
    graph: &mpsc::Sender<MemoryGraphMessage>,
    node_type: &str,
) -> Result<Vec<AttrNode>, String> {
    let (t, r) = oneshot::channel();
    graph
        .send(MemoryGraphMessage::QueryAttrNodes {
            node_type: Some(node_type.to_string()),
            subtype: None,
            name: None,
            limit: Some(100_000),
            reply_to: t,
        })
        .await
        .map_err(|e| format!("graph actor gone: {e}"))?;
    r.await
        .map_err(|e| format!("graph reply lost: {e}"))?
        .map_err(|e| format!("query '{node_type}': {e}"))
}

async fn graph_store_node(
    graph: &mpsc::Sender<MemoryGraphMessage>,
    node: AttrNode,
) -> Result<(), String> {
    let (t, r) = oneshot::channel();
    graph
        .send(MemoryGraphMessage::StoreAttrNode { node, reply_to: t })
        .await
        .map_err(|e| format!("graph actor gone: {e}"))?;
    r.await
        .map_err(|e| format!("store reply lost: {e}"))?
        .map_err(|e| format!("store failed: {e}"))?;
    Ok(())
}

async fn graph_delete_node(
    graph: &mpsc::Sender<MemoryGraphMessage>,
    id: String,
) -> Result<(), String> {
    let (t, r) = oneshot::channel();
    graph
        .send(MemoryGraphMessage::DeleteNode { id, reply_to: t })
        .await
        .map_err(|e| format!("graph actor gone: {e}"))?;
    r.await
        .map_err(|e| format!("delete reply lost: {e}"))?
        .map_err(|e| format!("delete failed: {e}"))?;
    Ok(())
}

/// Reads/writes `DataSource` definitions (and their `DataSourceAttribute`
/// schema children) in the memory graph. Single-writer: call from the future
/// DataSourceActor (or tests) so mutations are serialized.
pub struct GraphDataSourceStore {
    graph: mpsc::Sender<MemoryGraphMessage>,
}

impl GraphDataSourceStore {
    pub fn new(graph: mpsc::Sender<MemoryGraphMessage>) -> Self {
        Self { graph }
    }

    /// List all definitions (with their schema attributes reassembled).
    pub async fn list(&self) -> Result<Vec<DataSource>, String> {
        let sources = graph_query_nodes(&self.graph, SOURCE_NODE_TYPE).await?;
        let schema_nodes = graph_query_nodes(&self.graph, ATTR_NODE_TYPE).await?;
        let mut out = Vec::with_capacity(sources.len());
        for s in &sources {
            let mine: Vec<AttrNode> = schema_nodes
                .iter()
                .filter(|a| prop_str(&a.properties, PROP_SOURCE_ID) == s.id())
                .cloned()
                .collect();
            out.push(datasource_from_nodes(s, &mine)?);
        }
        Ok(out)
    }

    /// Fetch a definition by id.
    pub async fn get(&self, id: &str) -> Result<Option<DataSource>, String> {
        let (t, r) = oneshot::channel();
        self.graph
            .send(MemoryGraphMessage::GetAttrNode {
                id: id.to_string(),
                reply_to: t,
            })
            .await
            .map_err(|e| format!("graph actor gone: {e}"))?;
        let node = match r.await.map_err(|e| format!("get reply lost: {e}"))? {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("get '{id}': {e}")),
        };
        let schema_nodes = graph_query_nodes(&self.graph, ATTR_NODE_TYPE).await?;
        let mine: Vec<AttrNode> = schema_nodes
            .iter()
            .filter(|a| prop_str(&a.properties, PROP_SOURCE_ID) == id)
            .cloned()
            .collect();
        Ok(Some(datasource_from_nodes(&node, &mine)?))
    }

    /// Upsert by stable id: replace the existing definition (source node +
    /// schema attribute nodes), then store the source and one node per schema
    /// attribute. Idempotent; edits keep the same uuid.
    pub async fn upsert(&self, source: &DataSource) -> Result<(), String> {
        if self.get(&source.id).await?.is_some() {
            self.delete(&source.id).await?;
        }
        graph_store_node(&self.graph, datasource_to_node(source)).await?;
        for attr in schema_to_nodes(source) {
            graph_store_node(&self.graph, attr).await?;
        }
        Ok(())
    }

    /// Delete a definition and its schema attribute nodes (cascade by FK).
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let schema_nodes = graph_query_nodes(&self.graph, ATTR_NODE_TYPE).await?;
        for a in schema_nodes
            .iter()
            .filter(|a| prop_str(&a.properties, PROP_SOURCE_ID) == id)
        {
            let _ = graph_delete_node(&self.graph, a.id().to_string()).await;
        }
        let _ = graph_delete_node(&self.graph, id.to_string()).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spire_actor::Actor;
    use spire_core::actors::{MemoryGraphActor, MemoryGraphMessage};
    use tokio::sync::oneshot;

    const FIXTURE: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        { "type": "Feature", "properties": { "NAME": "Alpha", "POP": 12 }, "geometry": { "type": "Point", "coordinates": [103.8, 1.3] } },
        { "type": "Feature", "properties": { "NAME": "Beta", "POP": "many" }, "geometry": { "type": "Point", "coordinates": [103.9, 1.4] } }
      ]
    }"#;

    /// Fresh in-memory graph actor store in a temp dir.
    async fn graph_tx(dir: &std::path::Path) -> mpsc::Sender<MemoryGraphMessage> {
        let (tx, rx) = mpsc::channel(64);
        let _join = MemoryGraphActor::new().spawn(rx);
        let (t, r) = oneshot::channel();
        tx.send(MemoryGraphMessage::InitializeInMemory {
            data_dir: dir.to_path_buf(),
            reply_to: t,
        })
        .await
        .unwrap();
        r.await.unwrap().expect("store init");
        tx
    }

    #[test]
    fn analyze_geojson_infers_schema_and_kinds() {
        let info = analyze_geojson(FIXTURE, "demo".to_string(), "fixture".to_string()).unwrap();
        assert_eq!(info.feature_count, 2);
        assert_eq!(info.geometry_types, vec!["Point".to_string()]);
        assert_eq!(info.schema.get("NAME").unwrap(), "string");
        // Same key with a number then a string collapses to "mixed".
        assert_eq!(info.schema.get("POP").unwrap(), "mixed");
    }

    #[test]
    fn codec_normalizes_config_and_schema() {
        let mut src = DataSource::new("data-gov-sg", "Demo", serde_json::json!({}));
        src.config = serde_json::json!({ "dataset_id": "d_x", "zoom": 14 });
        src.discovered = Some(Discovery {
            feature_count: 2,
            geometry_types: vec!["Point".to_string()],
            schema: serde_json::json!({ "NAME": "string", "POP": "mixed" }),
        });
        let node = datasource_to_node(&src);
        let attrs = schema_to_nodes(&src);
        // Config becomes scalar properties, NOT a JSON blob.
        assert_eq!(node.properties.get("config_dataset_id"), Some(&Value::String("d_x".into())));
        assert_eq!(node.properties.get("config_zoom"), Some(&Value::Number(14.into())));
        assert_eq!(node.properties.get("enabled"), Some(&Value::Bool(true)));
        assert_eq!(attrs.len(), 2);

        let back = datasource_from_nodes(&node, &attrs).unwrap();
        assert_eq!(back.id, src.id);
        assert_eq!(back.kind, "data-gov-sg");
        assert_eq!(back.label, "Demo");
        assert_eq!(back.config["dataset_id"], "d_x");
        let d = back.discovered.unwrap();
        assert_eq!(d.feature_count, 2);
        assert_eq!(d.schema, serde_json::json!({ "NAME": "string", "POP": "mixed" }));
    }

    #[tokio::test]
    async fn graph_store_round_trips_definition_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let tx = graph_tx(dir.path()).await;
        let store = GraphDataSourceStore::new(tx.clone());

        let mut src = DataSource::new("data-gov-sg", "National Map Lines", serde_json::json!({}));
        src.config = serde_json::json!({ "dataset_id": "d_x" });
        src.discovered = Some(Discovery {
            feature_count: 42,
            geometry_types: vec!["LineString".to_string()],
            schema: serde_json::json!({ "NAME": "string", "OBJECTID": "number" }),
        });
        store.upsert(&src).await.unwrap();

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        let got = &listed[0];
        assert_eq!(got.id, src.id);
        assert_eq!(got.kind, "data-gov-sg");
        assert_eq!(got.config["dataset_id"], "d_x");
        let d = got.discovered.as_ref().unwrap();
        assert_eq!(d.feature_count, 42);
        assert_eq!(d.geometry_types, vec!["LineString".to_string()]);
        assert_eq!(d.schema, serde_json::json!({ "NAME": "string", "OBJECTID": "number" }));

        // Edit label; upsert keeps the same id and replaces fields.
        let mut edited = got.clone();
        edited.label = "Renamed".to_string();
        store.upsert(&edited).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);
        let got2 = store.get(&src.id).await.unwrap().unwrap();
        assert_eq!(got2.label, "Renamed");
        assert_eq!(got2.config["dataset_id"], "d_x");

        // Delete cascades the schema attribute nodes.
        store.delete(&src.id).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        let (t, r) = oneshot::channel();
        tx.send(MemoryGraphMessage::QueryAttrNodes {
            node_type: Some(ATTR_NODE_TYPE.to_string()),
            subtype: None,
            name: None,
            limit: Some(100),
            reply_to: t,
        })
        .await
        .unwrap();
        let remaining = r.await.unwrap().unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn builtin_registry_contains_data_gov_sg() {
        let reg = DriverRegistry::builtin();
        assert!(reg.get("data-gov-sg").is_some());
        assert_eq!(reg.kinds(), vec!["data-gov-sg"]);
    }
}
