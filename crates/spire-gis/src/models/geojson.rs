// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! Minimal GeoJSON (RFC 7946) reading/writing for Layer/Feature imports and
//! ad-hoc query overlays. Kept dependency-free: coordinates are decoded with
//! `serde_json` straight into the `geo` types spire-core stores (x = longitude,
//! y = latitude). `GeometryCollection` is not decoded (tile/MVT encoding skips
//! it too).

use serde::Deserialize;
use serde_json::value::RawValue;
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

/// A GeoJSON line needs at least two vertices — data.gov.sg roads are often
/// stored as straight 2-vertex segments, which `ring_from` (≥3, for polygon
/// rings) would wrongly reject.
fn line_from(v: &Value) -> Option<LineString<f64>> {
    let coords: Vec<Coord<f64>> = v.as_array()?.iter().filter_map(coord_from).collect();
    if coords.len() < 2 {
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
        "LineString" => line_from(coords).map(Geometry::LineString),
        "MultiLineString" => {
            let lines: Vec<LineString<f64>> =
                coords.as_array()?.iter().filter_map(line_from).collect();
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

/// Public helper: decode a GeoJSON geometry object (e.g. from a `gis/query`
/// `contains`/`intersects` region) into a `geo` geometry.
pub fn decode_geometry(v: &Value) -> Option<Geometry<f64>> {
    geometry_from(v)
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

/// Radial-distance decimation for *display* geometry: drops any vertex closer
/// than `tol_deg` (degrees) to the last kept vertex, always keeping the first
/// and last. Lines that would collapse below 2 vertices are returned unchanged.
///
/// Full-precision geometry stays in the store for spatial queries; this is only
/// applied when serving a very large layer to the map, so the GeoJSON payload
/// handed to the webview (and parsed on its main thread) stays small.
pub fn decimate_geometry(g: &Geometry<f64>, tol_deg: f64) -> Geometry<f64> {
    fn keep(pts: &[Coord<f64>], tol: f64) -> Vec<Coord<f64>> {
        let mut out: Vec<Coord<f64>> = Vec::with_capacity(pts.len());
        let n = pts.len();
        for (i, p) in pts.iter().enumerate() {
            let last = match out.last() {
                Some(c) => *c,
                None => {
                    out.push(*p);
                    continue;
                }
            };
            let d = ((p.x - last.x).powi(2) + (p.y - last.y).powi(2)).sqrt();
            if d >= tol || i == n - 1 {
                out.push(*p);
            }
        }
        out
    }
    fn dec_line(ls: &LineString<f64>, tol: f64) -> LineString<f64> {
        let v = keep(&ls.0, tol);
        if v.len() >= 2 {
            LineString(v)
        } else {
            ls.clone()
        }
    }
    match g {
        Geometry::Line(_) | Geometry::Point(_) | Geometry::MultiPoint(_) => g.clone(),
        Geometry::LineString(ls) => Geometry::LineString(dec_line(ls, tol_deg)),
        Geometry::MultiLineString(mls) => Geometry::MultiLineString(
            MultiLineString(mls.0.iter().map(|l| dec_line(l, tol_deg)).collect()),
        ),
        _ => g.clone(),
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

// ============================================================================
// Streaming (memory-bounded) reader for large FeatureCollections
// ============================================================================
//
// `parse_geojson` builds a whole-document `serde_json::Value` tree — fine for
// small files, but a 350 MB layer expands to several GB of `Value` (that is
// what OOM'd the national-map line import). Here the top level is parsed into
// zero-copy `RawValue` slices (no deep tree) and each feature is decoded
// independently, so peak memory stays proportional to the decoded features,
// not to the whole JSON document.

#[derive(Deserialize)]
struct RawFeature<'a> {
    #[serde(borrow, default)]
    geometry: Option<&'a RawValue>,
    #[serde(borrow, default)]
    properties: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct RawCollection<'a> {
    #[serde(borrow, default)]
    features: Vec<RawFeature<'a>>,
}

/// Decode a FeatureCollection into owned features without materialising a
/// whole-document `Value` tree. Features with `null`/unsupported geometry are
/// skipped. `max_features` (optional) caps the decoded count.
pub fn parse_geojson_stream(
    text: &str,
    max_features: Option<usize>,
) -> Result<Vec<DecodedFeature>, String> {
    let coll: RawCollection =
        serde_json::from_str(text).map_err(|e| format!("invalid geojson json: {e}"))?;
    let mut out: Vec<DecodedFeature> = Vec::with_capacity(coll.features.len().min(65_536));
    for raw in coll.features {
        if let Some(mx) = max_features {
            if out.len() >= mx {
                break;
            }
        }
        let Some(geom_raw) = raw.geometry else { continue };
        let geom_val: Value =
            serde_json::from_str(geom_raw.get()).map_err(|e| format!("bad geometry: {e}"))?;
        let Some(geometry) = geometry_from(&geom_val) else { continue };
        let properties = match raw.properties {
            Some(r) => serde_json::from_str::<Value>(r.get())
                .map(|v| match v {
                    Value::Object(m) => m,
                    _ => Map::new(),
                })
                .unwrap_or_default(),
            None => Map::new(),
        };
        out.push(DecodedFeature {
            geometry,
            properties,
        });
    }
    Ok(out)
}

