// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! spire-gis domain models: Layer/Feature node builders + GeoJSON parsing.

pub mod geojson;

use std::collections::HashMap;

use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use spire_core::models::memory_graph::AttrNode;
use spire_core::spatial::geo::Geometry;
use uuid::Uuid;

/// `node_type` for layer catalog records.
pub const NODE_LAYER: &str = "Layer";
/// `node_type` for feature records (each MVT layer = one `node_type`).
pub const NODE_FEATURE: &str = "Feature";
/// Custom edge label Layer → Feature.
pub const EDGE_CONTAINS: &str = "CONTAINS";

/// Reserved property keys kept out of the attribute map (handled as node
/// fields or wire ids instead).
pub const RESERVED_PROPS: [&str; 4] = ["id", "name", "source_id", "layer_id"];

/// Keys the graph store reserves for its own node fields — dropped from the
/// attribute map so an import can never shadow them.
const STORE_BASE_KEYS: [&str; 8] = [
    "uuid",
    "node_type",
    "subtype",
    "description",
    "embedding_id",
    "created_at",
    "updated_at",
    "version",
];

const MAX_ATTR_STRING_LEN: usize = 4096;

/// Rewrite an arbitrary attribute key into a safe GQL property identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`); `None` when nothing usable remains. E.g.
/// `"SHAPE.LEN"` → `"SHAPE_LEN"`.
pub fn sanitize_key(key: &str) -> Option<String> {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    while out.starts_with('_') {
        out.remove(0);
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty()
        || out
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        None
    } else {
        Some(out)
    }
}

/// Make a string safe to embed in a single-quoted GQL literal (raw control
/// characters can break the parser) and cap its length.
pub fn sanitize_string_value(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
    cleaned.chars().take(MAX_ATTR_STRING_LEN).collect()
}

/// Keep only scalars with safe, non-reserved keys. Applied to raw GeoJSON
/// attributes before they are stored or reflected in the layer schema.
pub fn sanitize_attributes(
    attrs: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for (raw_key, value) in attrs {
        let Some(key) = sanitize_key(raw_key) else {
            continue;
        };
        if RESERVED_PROPS.contains(&key.as_str()) || STORE_BASE_KEYS.contains(&key.as_str()) {
            continue;
        }
        let value = match value {
            serde_json::Value::String(s) => serde_json::Value::String(sanitize_string_value(s)),
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.clone(),
            _ => continue, // drop arrays/objects/null — not scalar GQL props
        };
        out.insert(key, value);
    }
    out
}

fn new_node(id: String, node_type: &str, name: &str) -> AttrNode {
    AttrNode {
        id,
        node_type: node_type.to_string(),
        subtype: None,
        name: name.to_string(),
        description: None,
        properties: HashMap::new(),
        embedding_id: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        version: 1,
    }
}

/// A `Layer` catalog node. `style`/`schema` are stored as JSON scalars in
/// `properties` (MapLibre style + inferred attribute schema).
pub fn layer_node(
    name: &str,
    display_name: &str,
    description: &str,
    geometry_type: &str,
    source: &str,
    style: serde_json::Value,
    schema: serde_json::Value,
) -> AttrNode {
    let mut n = new_node(Uuid::new_v4().to_string(), NODE_LAYER, name);
    n.description = Some(description.to_string());
    n.properties = HashMap::from([
        ("display_name".to_string(), json!(display_name)),
        ("geometry_type".to_string(), json!(geometry_type)),
        ("source".to_string(), json!(source)),
        ("style".to_string(), style),
        ("schema".to_string(), schema),
        // Stacking order (higher = drawn on top). Imports bump it so a new
        // layer lands on top; the reorder RPC reassigns the whole sequence.
        ("z_order".to_string(), json!(0)),
    ]);
    n
}

/// A `Feature` node: geometry via `set_spatial_geometry` (derives the scalar
/// bbox columns the spatial pre-filter scans) plus inline scalar attributes
/// (so `tiles::encode_tile` tags them onto the MVT features).
pub fn feature_node(
    layer_id: &str,
    source_id: &str,
    name: &str,
    geometry: &Geometry<f64>,
    attributes: &serde_json::Map<String, serde_json::Value>,
) -> AttrNode {
    let mut n = new_node(Uuid::new_v4().to_string(), NODE_FEATURE, name);
    n.properties.insert("layer_id".to_string(), json!(layer_id));
    n.properties
        .insert("source_id".to_string(), json!(source_id));
    for (k, v) in sanitize_attributes(attributes) {
        n.properties.insert(k, v);
    }
    n.set_spatial_geometry(geometry);
    n
}

/// Default MapLibre paint/layout for a geometry type (viewer merges this into
/// its layer list).
pub fn default_style(geometry_type: &str) -> serde_json::Value {
    let layer = match geometry_type {
        "Point" => json!({
            "type": "circle",
            "paint": { "circle-radius": 5, "circle-color": "#3388ff", "circle-opacity": 0.9 }
        }),
        "LineString" => json!({
            "type": "line",
            "paint": { "line-color": "#3388ff", "line-width": 2, "line-opacity": 0.9 }
        }),
        // Polygon + Mixed default to translucent fills.
        _ => json!({
            "type": "fill",
            "paint": { "fill-color": "#3388ff", "fill-opacity": 0.35 }
        }),
    };
    layer
}

/// Human label for a `geo::Geometry` (point/line/polygon classification).
pub fn geometry_kind(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => "Point",
        Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => "LineString",
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) => "Polygon",
        _ => "Mixed",
    }
}

/// Keys whose values are plumbing, not descriptive text — excluded from the
/// semantic-embedding text (and shown nowhere in the UI).
fn is_plumbing_key(k: &str) -> bool {
    matches!(k, "geometry" | "layer_id" | "source_id" | "embedding")
        || matches!(k, "OBJECTID" | "objectid" | "FID" | "fid" | "ID" | "id")
        || k.starts_with("min_")
        || k.starts_with("max_")
        || k.is_empty()
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Build a compact, searchable text for semantic embedding from a node.
///
/// Deterministic: the feature name + FOLDERPATH + layer name, then every
/// descriptive scalar attribute (all string/number/bool values, skipping
/// plumbing keys) in **sorted key order** — so place/district/area names are
/// always embedded regardless of which columns a dataset uses.
pub fn node_search_text(node: &AttrNode) -> String {
    let mut parts: Vec<String> = vec![node.name().to_string()];
    if let Some(c) = node.get("FOLDERPATH").and_then(|v| v.as_str()) {
        parts.push(c.to_string());
    }
    if let Some(s) = node.subtype() {
        parts.push(s.to_string());
    }

    let mut keys: Vec<&String> = node
        .properties
        .keys()
        .filter(|k| !is_plumbing_key(k))
        .collect();
    keys.sort();

    const MAX_TOTAL: usize = 1600;
    const MAX_VALUE: usize = 200;
    let mut budget: usize = 0;
    for k in keys {
        if budget >= MAX_TOTAL {
            break;
        }
        let Some(s) = node.get(k).and_then(scalar_text) else {
            continue;
        };
        let truncated: String = s.chars().take(MAX_VALUE).collect();
        if budget + truncated.len() > MAX_TOTAL {
            break;
        }
        budget += truncated.len();
        parts.push(truncated);
    }

    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

