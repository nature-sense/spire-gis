# SpireGis — Design Document

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

**Status:** Draft · **Target:** macOS (SwiftUI + WebKit + MapLibre GL JS) ·
**Stack:** spire-actor + spire-core

## 1. Overview

SpireGis is a proof-of-concept macOS GIS application built on the `spire-actor`
actor runtime and the `spire-core` graph/vector database. It imports GIS data
(a base map plus feature layers), stores it in the graph database, and renders
it on an interactive map with native controls for layer selection, zoom, and
ad-hoc spatial queries whose results appear in real time.

### Proof-of-concept scope

- Full `Layer` / `Feature` / `Properties` schema.
- Import of one layer from data.gov.sg (the Singapore "National Map Polygon").
- A viewer with base map, the imported layer, and zoom in/out.

## 2. Reused building blocks

| Concern | Reuse | Location |
| --- | --- | --- |
| Actor runtime (mailbox, spawn, registry) | `spire_actor::{Actor, ActorSystem, ServiceRegistry}` | `spire-actor/src/{actor,system,registry}.rs` |
| Graph + geometry store | `MemoryGraphActor` / `MemoryGraphMessage` (`SpatialQuery`, node/edge ops, `Sync`) | `spire-core/src/subsystems/graph/memory_graph.rs` |
| Geometry + slippy-map math | `spire_core::spatial` (`tile_bounds`, `point_to_tile`, `haversine_meters`, predicates, `geo` re-export) | `spire-core/src/spatial.rs` |
| Vector-tile serving | `TileActor` (`GetTile`/`GetTileFeatures`/`ClearCache`, LRU per `(filters,z,x,y)`) + `tiles::encode_tile` (MVT) | `spire-core/src/actors/tile.rs`, `src/tiles.rs` |
| Node envelope + spatial helpers | `AttrNode` (`set_geo_point`, `set_spatial_geometry`, `geo_point`, `spatial_geometry`) | `spire-core/src/models/memory_graph.rs` |
| Edges | `RelationshipType::Custom("…")` + `RelationshipInput { edge_type, from_id, to_id, properties, weight }` | same |
| JSON FFI pattern | `spire_send_json` / `spire_free_string` with `{method, params}` | `spire-code/src/ffi.rs` (mirrored in the `spire-gis` stub) |

`spire-gis` already exists as a scaffold (workspace + `crates/spire-gis` cdylib
+ `ui/swift` shell + `build/assemble-app.sh`); the work is filling in the Rust
core and the SwiftUI/WebKit viewer.

## 3. Architecture

```
SwiftUI (SpireUI)
   │  CoreBridge.send({"method": …, "params": …})      ← JSON FFI
   ▼
spire_send_json ──►  GisCoordinator::route_request(method, params)
                          │
        ┌─────────────────┼──────────────────────────────┐
        ▼                 ▼                               ▼
  ImportActor        LayerActor                    (graph + tile actors)
  (fetch/parse/       (list/get/                     MemoryGraphActor ── store
   ingest)             style layers)                 TileActor ── MVT bytes
        │                 │                               │
        └────────► MemoryGraphMessage  ◄───────────────────┘
                     (StoreAttrNode / MergeAttrNode / CreateRelationship /
                      SpatialQuery / OpenTransactionStream / Sync)
```

- **`GisCoordinator`** — composition root: a `tokio::Runtime`, an `ActorSystem`,
  the shared `ServiceRegistry`, and the JSON dispatcher (mirrors
  `spire-code/src/actors/coordinator.rs::route_request`).
- **`MemoryGraphActor`** — holds the GIS graph (Layer/Feature/Properties +
  edges + bounding boxes).
- **`TileActor`** — answers `GetTile` with MVT bytes for any slippy-map `z/x/y`.

## 4. Data model → graph

The spec node types map onto `AttrNode` (`node_type` discriminator) and the edge
types onto `RelationshipType::Custom`:

| Spec | Implementation |
| --- | --- |
| `Layer` | `AttrNode { node_type: "Layer", name: "roads", … }`; `style` + `schema` as JSON scalars in `properties` |
| `Feature` | `AttrNode { node_type: "Feature", name: source id, … }` with geometry via `set_spatial_geometry(&geo::Geometry)` (auto-derives the `min_*`/`max_*` bbox for the GQL pre-filter) and scalar attributes in `properties` |
| `Properties` | `AttrNode { node_type: "Properties", … }` holding the dynamic queryable fields |
| `CONTAINS` | `Custom("CONTAINS")` Layer → Feature |
| `HAS_PROPERTIES` | `Custom("HAS_PROPERTIES")` Feature → Properties |
| `LOCATED_IN` / `NEARBY` / `WITHIN` | `Custom("…")` Feature → Feature, derived from spatial predicates |

`uuid`/`created_at`/`updated_at`/`version` are already `AttrNode` fields
(`id`, `created_at`, `updated_at`, `version`), so the spec's optimistic-locking
`version` is native.

**Recommendation:** store scalar display attributes inline on `Feature`.
`tiles::encode_tile` already writes every scalar `properties` entry as an MVT
tag, so attributes render in the map with no join. Keep the `Properties`
companion node (linked by `HAS_PROPERTIES`) for relational/ad-hoc querying per
the spec.

### 4.1 `Layer` node (metadata)

Stores information about a named layer and its schema.

| Property | Type | Description |
| :--- | :--- | :--- |
| `uuid` | String | Unique ID |
| `node_type` | String = `"Layer"` | |
| `name` | String | Machine-readable name (e.g., `"roads"`) |
| `display_name` | String | Human-readable name (e.g., `"Road Network"`) |
| `description` | String | Optional description |
| `geometry_type` | String | `"Point"`, `"LineString"`, `"Polygon"`, `"Mixed"` |
| `style` | JSON | MapLibre GL JS style object |
| `source` | String | Data source (e.g., `"data.gov.sg"`, `"OSM"`, `"User"`) |
| `schema` | JSON | Property schema definition |
| `created_at` | DateTime | Creation timestamp |
| `updated_at` | DateTime | Last update timestamp |
| `version` | Integer | Optimistic locking version |

### 4.2 `Feature` node (geometry)

Stores the geometry and links to its layer and properties.

| Property | Type | Description |
| :--- | :--- | :--- |
| `uuid` | String | Unique ID |
| `node_type` | String = `"Feature"` | |
| `layer_id` | String | UUID of the Layer node |
| `name` | String | Feature name or ID |
| `geometry` | GEOMETRY | Point / LineString / Polygon (WGS84) |
| `source_id` | String | External ID from source |
| `created_at` | DateTime | Creation timestamp |
| `updated_at` | DateTime | Last update timestamp |
| `version` | Integer | Optimistic locking version |

### 4.3 `Properties` node (attributes)

Companion node storing all queryable properties for a feature.

| Property | Type | Description |
| :--- | :--- | :--- |
| `uuid` | String | Unique ID |
| `node_type` | String = `"Properties"` | |
| `layer_id` | String | UUID of the Layer node |
| `feature_id` | String | UUID of the Feature node |
| `created_at` | DateTime | Creation timestamp |
| `updated_at` | DateTime | Last update timestamp |
| `version` | Integer | Optimistic locking version |
| *[Dynamic fields]* | Various | Queryable properties for the layer |

### 4.4 Edge types

| Edge | From → To | Purpose |
| :--- | :--- | :--- |
| `CONTAINS` | Layer → Feature | Links a layer to its features |
| `HAS_PROPERTIES` | Feature → Properties | Links a feature to its properties |
| `LOCATED_IN` | Feature → Feature | Spatial containment |
| `NEARBY` | Feature → Feature | Spatial proximity |
| `WITHIN` | Feature → Feature | Hierarchical containment |

## 5. New actors

### 5.1 `ImportActor` (data-source ingestion)

Messages: `ImportGeoJson { layer_id, url/bytes, reply_to }`,
`ImportDataGovSg { dataset_id, reply_to }`, `ImportStatus { layer_id, reply_to }`.

- Fetches + parses GeoJSON (`geo`/`geojson` for geometry, decodes attributes).
- Infers the property schema (string/number/bool → `Layer.schema` JSON).
- Writes nodes/edges through `MemoryGraphMessage::OpenTransactionStream`
  (`StreamOp::StoreNode` + `StreamOp::CreateRelationship` … `Commit`) for one
  atomic bulk load, then `Sync` to snapshot (mandatory — see §10).
- Publishes progress via a broadcast event channel for the UI.

### 5.2 `LayerActor` (catalog + styles)

Messages: `ListLayers`, `GetLayer { id }`, `SetStyle { id, style }`,
`DeleteLayer { id }`. Backed by `QueryAttrNodes` filtering `node_type == "Layer"`.

### 5.3 Spatial queries (no new actor)

Ad-hoc queries go straight to `MemoryGraphActor::SpatialQuery`
(`BoundingBox` / `Radius` / `Contains` / `Intersects` / `Nearest`), returning
`SpatialQueryResult { nodes: Vec<DistanceScoredNode>, … }`; the coordinator maps
results to GeoJSON for the overlay layer.

## 6. FFI / RPC surface

| Method | Params | Returns |
| --- | --- | --- |
| `gis/status` | — | `{ok, core}` |
| `gis/import-datagov` | `{dataset_id}` | `{layer_id, features, bounds, schema}` |
| `gis/import-status` | `{layer_id}` | `{state, done, total}` |
| `gis/list-layers` | — | `[{id, name, display_name, geometry_type, feature_count, bounds}]` |
| `gis/get-layer` | `{id}` | `{…, schema, style}` |
| `gis/set-layer-style` | `{id, style}` | `{ok}` |
| `gis/delete-layer` | `{id}` | `{ok}` |
| `gis/get-tile` | `{layer_ids:[], z, x, y}` | `{tile: "<base64 MVT>"}` |
| `gis/spatial-query` | `{type, …}` | GeoJSON `FeatureCollection` |
| `gis/get-feature` | `{feature_id}` | `{id, properties}` |

## 7. The viewer

- **`MapView: NSViewRepresentable`** wraps a `WKWebView`; a
  `WKUserContentController` handler is the JS→Swift bridge
  (`window.webkit.messageHandlers.spire.postMessage(...)`); Swift→JS uses
  `webView.evaluateJavaScript(...)`.
- **MapLibre GL JS** renders a base map + one `vector` source per layer, fed by a
  custom protocol handler:

  ```js
  maplibregl.addProtocol('spire', (params, cb) => {
    const p = params.url.split('/'); // spire://tiles/<layerIds>/<z>/<x>/<y>
    window.webkit.messageHandlers.spire.postMessage({
      kind: 'tile', z: +p[3], x: +p[4], y: +p[5], layers: p[2]
    });
    pending[params.url] = cb;        // Swift resolves: base64 → cb(null, ArrayBuffer)
  });
  ```

  Swift → `gis/get-tile` → `TileActor::GetTile` → base64 MVT → JS.
- **Native controls**: a `LayerPanel` (SwiftUI `List`) toggles visibility
  (`map.setLayoutProperty(id,'visibility',…)`); zoom buttons call
  `map.zoomIn()/zoomOut()`; MapLibre still handles pan/pinch.
- **Attribute inspector**: click a feature → `gis/get-feature` → native popover.
- **Ad-hoc results**: `gis/spatial-query` → GeoJSON →
  `map.getSource('results').setData(fc)`.

## 8. Data source — data.gov.sg National Map Polygon

`dataset_id = "d_29f066d67df3eae91df8a42f443863c8"`:

1. `GET https://api-open.data.gov.sg/v1/public/api/datasets/{id}/poll-download`
   → poll `{data.url, status}` until ready.
2. Download the artifact (zip) → extract the GeoJSON (attributes:
   `OBJECTID, NAME, FOLDERPATH, SYMBOLID, INC_CRC, FMEL_UPD_D, SHAPE.LEN`).
3. Per feature: build the `Feature` `AttrNode` with `set_spatial_geometry`, map
   attributes → `properties` + `Properties` node, add `CONTAINS` +
   `HAS_PROPERTIES` edges; write the single `Layer` node with inferred schema +
   a default MapLibre style.
4. `Sync` to persist.

## 9. Proposed layout (`crates/spire-gis/src/`)

```
lib.rs            — FFI entry (spire_send_json) + composition root
coordinator.rs    — route_request(method, params) → typed messages
models/mod.rs     — LayerNode/FeatureNode/PropertiesNode builders, GeoJSON⇄AttrNode, schema inference
actors/import.rs  — ImportActor (poll-download + GeoJSON ingestion)
actors/layer.rs   — LayerActor (catalog/style)
sources/datagov.rs— data.gov.sg client
tile.rs           — thin wrapper: FFI tile method → TileActor
```

Reused as-is from spire-core: `MemoryGraphActor`, `TileActor`/`TileMessage`/
`TileFilters`, `spatial`, `tiles::encode_tile`, `AttrNode` helpers,
`MemoryGraphMessage`, `Actor`/`ActorSystem`/`ServiceRegistry`.

## 10. Key decisions & tradeoffs

1. **Tile path = FFI bridge** (not an embedded HTTP server) — matches "all
   interactions through Swift", avoids a new HTTP dependency/port.
   *(Alternative: a tiny `127.0.0.1` MVT endpoint for MapLibre-native caching.)*
2. **Store = dedicated user-level dir** (`~/.spire/gis-data`) rather than the
   repo's `.spire/data`, so datasets persist across app launches.
3. **Denormalized attributes on `Feature`** so `encode_tile` tags them for free;
   `Properties` node kept for the spec's relational querying.
4. **`Sync` after import is mandatory** — the debounced snapshot alone won't
   flush before the app quits (the trap hit during the RAG work).

## 11. Delivery phases

- **Phase 0 (scaffold fill):** `GisCoordinator` + `ActorSystem` + store init
  (`~/.spire/gis-data`), `gis/status`, `gis/list-layers`.
- **Phase 1 (PoC):** full schema; data.gov.sg import; `gis/get-tile` via
  `TileActor`; WKWebView MapLibre viewer (base map + layer + native zoom); a
  `BoundingBox` ad-hoc query.
- **Phase 2 (querying):** `Radius`/`Contains`/`Nearest`/`Intersects` +
  real-time overlay, attribute inspector, multi-layer visibility + styles,
  `LOCATED_IN`/`NEARBY`/`WITHIN` edge inference.
- **Phase 3 (scale):** spatial-index tuning, more source types (Shapefile, OSM),
  hybrid spatial+semantic (RAG) retrieval, low-zoom layer tiling.



