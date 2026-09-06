// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! Minimal GeoJSON (RFC 7946) reading/writing for Layer/Feature imports and
//! ad-hoc query overlays. Kept dependency-free: coordinates are decoded with
//! `serde_json` straight into the `geo` types spire-core stores (x = longitude,
//! y = latitude). `GeometryCollection` is not decoded (tile/MVT encoding skips
//! it too).

use serde_json::{json, Map, Value};
use spire_core::spatial::geo::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

/// One decoded feature: geometry + its attribute object.
pub struct DecodedFeature {
    pub geometry: Geometry<f64>,
    pub properties: Map<String, Value>,
}

fn coord_from(v: &Value) -> Option<Coord<f64>> {
    let arr = v.as_array()?;
    let x = arr.first()?.as_f64()?;
    let y = arr.get(1)?.as_f64()?;
    Some(Coord { x, y })
}

fn ring_from(v: &Value) -> Option<LineString<f64>> {
    let coords: Vec<Coord<f64>> = v.as_array()?.iter().filter_map(coord_from).collect();
    if coords.len() < 3 {
        return None;
    }
    Some(LineString(coords))
}

fn polygon_from(v: &Value) -> Option<Polygon<f64>> {
    let arr = v.as_array()?;
    let mut it = arr.iter();
    let exterior = ring_from(it.next()?)?;
    let interiors: Vec<LineString<f64>> = it.filter_map(ring_from).collect();
    Some(Polygon::new(exterior, interiors))
}

fn geometry_from(v: &Value) -> Option<Geometry<f64>> {
    let typ = v.get("type")?.as_str()?;
    let coords = v.get("coordinates")?;
    match typ {
        "Point" => coord_from(coords).map(|c| Geometry::Point(Point::new(c.x, c.y))),
        "MultiPoint" => {
            let pts: Vec<Point<f64>> = coords
                .as_array()?
                .iter()
                .filter_map(coord_from)
                .map(|c| Point::new(c.x, c.y))
                .collect();
            (!pts.is_empty()).then(|| Geometry::MultiPoint(MultiPoint(pts)))
        }
        "LineString" => ring_from(coords).map(Geometry::LineString),
        "MultiLineString" => {
            let lines: Vec<LineString<f64>> =
                coords.as_array()?.iter().filter_map(ring_from).collect();
            (!lines.is_empty()).then(|| Geometry::MultiLineString(MultiLineString(lines)))
        }
        "Polygon" => polygon_from(coords).map(Geometry::Polygon),
        "MultiPolygon" => {
            let polys: Vec<Polygon<f64>> =
                coords.as_array()?.iter().filter_map(polygon_from).collect();
            (!polys.is_empty()).then(|| Geometry::MultiPolygon(MultiPolygon(polys)))
        }
        _ => None, // GeometryCollection etc. — not stored.
    }
}

// === MORE ===

fn feature_from(v: &Value) -> Option<DecodedFeature> {
    let geometry = geometry_from(v.get("geometry")?)?;
    let properties = match v.get("properties") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    Some(DecodedFeature {
        geometry,
        properties,
    })
}

/// Parse a GeoJSON document (FeatureCollection / Feature / bare Geometry).
pub fn parse_geojson(text: &str) -> Result<Vec<DecodedFeature>, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| format!("invalid geojson json: {e}"))?;
    let mut out = Vec::new();
    match root.get("type").and_then(|v| v.as_str()) {
        Some("FeatureCollection") => {
            for f in root
                .get("features")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(feat) = feature_from(f) {
                    out.push(feat);
                }
            }
        }
        Some("Feature") => {
            if let Some(feat) = feature_from(&root) {
                out.push(feat);
            }
        }
        Some(_) => {
            if let Some(g) = geometry_from(&root) {
                out.push(DecodedFeature {
                    geometry: g,
                    properties: Map::new(),
                });
            }
        }
        None => return Err("missing geojson 'type'".to_string()),
    }
    Ok(out)
}

// ============================================================================
// Encoding geo -> GeoJSON (for query result overlays)
// ============================================================================

fn coord_json(c: &Coord<f64>) -> Value {
    json!([c.x, c.y])
}

fn linestring_json(ls: &LineString<f64>) -> Value {
    Value::Array(ls.0.iter().map(coord_json).collect())
}

fn polygon_json(poly: &Polygon<f64>) -> Value {
    let mut rings: Vec<Value> = Vec::new();
    rings.push(linestring_json(poly.exterior()));
    for hole in poly.interiors() {
        rings.push(linestring_json(hole));
    }
    Value::Array(rings)
}

/// Serialize a stored geometry back to a GeoJSON geometry object.
pub fn geometry_to_geojson(g: &Geometry<f64>) -> Value {
    match g {
        Geometry::Point(p) => json!({ "type": "Point", "coordinates": [p.x(), p.y()] }),
        Geometry::MultiPoint(mp) => json!({
            "type": "MultiPoint",
            "coordinates": mp.0.iter().map(|p| json!([p.x(), p.y()])).collect::<Vec<_>>()
        }),
        Geometry::Line(line) => json!({
            "type": "LineString",
            "coordinates": [[line.start.x, line.start.y], [line.end.x, line.end.y]]
        }),
        Geometry::LineString(ls) => {
            json!({ "type": "LineString", "coordinates": linestring_json(ls) })
        }
        Geometry::MultiLineString(mls) => json!({
            "type": "MultiLineString",
            "coordinates": mls.0.iter().map(linestring_json).collect::<Vec<_>>()
        }),
        Geometry::Polygon(poly) => json!({
            "type": "Polygon",
            "coordinates": polygon_json(poly)
        }),
        Geometry::MultiPolygon(mp) => json!({
            "type": "MultiPolygon",
            "coordinates": mp.0.iter().map(polygon_json).collect::<Vec<_>>()
        }),
        _ => json!({ "type": "GeometryCollection", "geometries": [] }),
    }
}

/// The geometry of a stored node (spatial_geometry, else its lat/lon point).
pub fn node_geometry_geojson(node: &spire_core::models::memory_graph::AttrNode) -> Option<Value> {
    if let Some(g) = node.spatial_geometry() {
        Some(geometry_to_geojson(&g))
    } else if let Some(p) = node.geo_point() {
        Some(json!({ "type": "Point", "coordinates": [p.x(), p.y()] }))
    } else {
        None
    }
}
