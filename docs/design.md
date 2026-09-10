# spire-gis — Design

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

**Status:** active · **Target:** macOS 14+ (SwiftUI + WebKit + MapLibre GL JS) ·
**Core:** Rust cdylib on `spire-actor` + `spire-core`

This document is the entry point. Each area links to a deeper doc.

## 1. Purpose

spire-gis imports external GIS datasets (data.gov.sg GeoJSON), stores them in a
graph database, and renders them on an interactive MapLibre map with native
controls for visibility, stacking, selection, spatial queries and semantic
search.

## 2. Documentation map

| Topic | Doc |
| --- | --- |
| Components, actors, request lifecycle, persistence | [architecture.md](architecture.md) |
| Graph schema (nodes, properties, embeddings, storage) | [data-model.md](data-model.md) |
| Connector definitions, drivers, discover/fetch | [datasources.md](datasources.md) |
| GeoJSON → graph ingestion | [import-pipeline.md](import-pipeline.md) |
| `gis/*` JSON method reference | [rpc-api.md](rpc-api.md) |
| SwiftUI + MapLibre host | [map-ui.md](map-ui.md) |

## 3. Architecture in brief

A SwiftUI host talks JSON over an FFI (`spire_send_json`) to a Rust core.
`coordinator::route_request` dispatches to actors: `ImportActor` (ingest),
`LayerActor` (catalog), `DataSourceActor` (connectors), `TileActor` (MVT),
`LlmActor` (NL queries), all backed by `MemoryGraphActor` (nodes, spatial and
vector indexes, snapshots). See [architecture.md](architecture.md).

## 4. Design principles

1. **Graph-native, normalised storage.** Configuration and schemas are nodes and
   scalar properties — not JSON blobs — so they are queryable.
2. **One ingestion path.** File, raw text and every data-source driver converge
   on `import_geojson_text` (replace-by-name, transactional writes).
3. **Definitions ≠ data.** Data sources are reusable definitions; fetching them
   produces layers.
4. **Thin, generic RPC.** A single JSON envelope; the coordinator is the only
   place that knows method names.
5. **Best-effort extras.** Embedding, display caches and logging never fail a
   request.

## 5. Key flows

- **Import** — parse → infer schema → create layer node (+ next `z_order`) →
  replace-by-name → transactional feature writes → `Sync` → cache invalidation →
  embed. [import-pipeline.md](import-pipeline.md)
- **Data source fetch** — `DataSourceActor::Fetch` runs the driver and hands the
  payload to `ImportActor`. [datasources.md](datasources.md)
- **Selection** — the sidebar can pin an **active** layer/sublayer for clicks;
  object mode picks whole features (cycling the stack), point mode snaps to the
  nearest vertex. [map-ui.md](map-ui.md)
- **Stacking** — `z_order` is persisted per layer and applied with
  `map.moveLayer`. [data-model.md](data-model.md)

## 6. Storage

Graph snapshots plus a display cache live in `~/.spire/gis-data`
(`SPIRE_GIS_DATA_DIR` overrides). The host logs to `ui.log` there.

## 7. Extending

- **Add a data source provider** — implement `DataSourceDriver` (kind /
  discover / fetch), register it in `DriverRegistry::builtin()`, and add a
  config field to the panel. No storage or UI plumbing changes are required: the
  definition model is generic.
- **Add a shader/attribute type** — attribute schemas are inferred from the
  source and surfaced via `gis/datasource/discover`.

## 8. Future work

- Data-source **refresh policies** (interval) and scheduling.
- Driver-provided **config schemas** (currently data.gov.sg's `dataset_id` is a
  fixed field in the panel).
- Provenance: persist `source_id`/`dataset_id` on produced layer nodes for a
  robust definition ↔ layer link (today it is by matching names).
- Automatic embeddings on every path (currently best-effort at import + an
  explicit `gis/reindex-embeddings`).
