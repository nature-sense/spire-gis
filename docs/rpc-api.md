# RPC API

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

The Rust core is driven entirely through one JSON entry point.

## FFI

```c
char *spire_send_json(const char *request); // caller must free
void  spire_free_string(char *ptr);
```

Request: `{"method": "gis/…", "params": { … }}`.
Reply envelope:

```json
{ "ok": true,  "result": … }
{ "ok": false, "error": "message" }
```

Every method below returns the `result` value. Actor-level failures surface as
`error`.

## Core

| Method | Params | Result |
| --- | --- | --- |
| `gis/status` | — | `{ "core": "spire-gis", "version": "…" }` |

## Layers

| Method | Params | Result |
| --- | --- | --- |
| `gis/list-layers` | — | `[ LayerInfo ]` sorted **bottom → top** by `z_order` |
| `gis/delete-layer` | `id` | `{ "deleted": true }` (also deletes the layer's features) |
| `gis/reorder-layer` | `id`, `direction` (`up`\|`down`) | updated `[ LayerInfo ]` |
| `gis/merge-layer` | `name` | `{ name, deleted, stored }` — coalesce named fragments in place |
| `gis/get-layer-geojson` | `layer`, `limit?`, `simplify?`, `drop_props?` | GeoJSON `FeatureCollection` with `total` + `truncated` |

`LayerInfo`:

```json
{ "id": "…", "name": "…", "display_name": "…", "description": "…",
  "geometry_type": "LineString", "source": "data.gov.sg", "feature_count": 15425,
  "bounds": [minLng,minLat,maxLng,maxLat], "classes": [{"key":"…","count":0}],
  "z_order": 3 }
```

## Features

| Method | Params | Result |
| --- | --- | --- |
| `gis/get-feature` | `id` (feature node uuid) | `{ id, layer, name, attributes }` — full scalar attribute map (geometry/layer_id/source_id removed) |

## Imports

| Method | Params | Result |
| --- | --- | --- |
| `gis/import-geojson-file` | `path`, `name?`, `display_name?` | import report |
| `gis/import-datagov` | `dataset_id`, `name?`, `display_name?` | import report |

Import report: `{ layer_id, name, display_name, geometry_type, feature_count, source, bounds }`.

## Queries

| Method | Params | Result |
| --- | --- | --- |
| `gis/spatial-query` | `min_lng,min_lat,max_lng,max_lat`, `layer?`, `limit?` | GeoJSON `FeatureCollection` |
| `gis/query` | structured DSL `{ predicate, region?, limit? }` | `{ total, features: FeatureCollection }` |
| `gis/nl-query` | `text`, `viewport` | `gis/query` result plus `summary`/`source`, or `{ fallback: true }` |
| `gis/semantic-search` | `text`, `node_type?` (`Feature`\|`Layer`), `layer?`, `limit?` | `{ query, total, features \| layers }` |
| `gis/reindex-embeddings` | — | `{ "embedded": N }` — backfill embeddings for all nodes |

## Tiles

| Method | Params | Result |
| --- | --- | --- |
| `gis/get-tile` | `layer`, `z`, `x`, `y` | `{ tile: "<base64 MVT>", bytes }` |

## Data sources

| Method | Params | Result |
| --- | --- | --- |
| `gis/datasource/kinds` | — | `["data-gov-sg", …]` |
| `gis/datasource/list` | — | `[ DataSource ]` |
| `gis/datasource/get` | `id` | `DataSource` \| `null` |
| `gis/datasource/add` | `kind`, `label`, `config` | `DataSource` |
| `gis/datasource/update` | `id`, `label?`, `config?`, `enabled?` | `DataSource` |
| `gis/datasource/delete` | `id` | `{ "ok": true }` |
| `gis/datasource/discover` | `id` | `DatasetInfo` |
| `gis/datasource/fetch` | `id` | import report |

`DatasetInfo`: `{ name, description, geometry_types, feature_count, schema }`.

## Error examples

- `unknown method: gis/…`
- `datasource/add requires 'kind'`
- `unknown data source kind 'x'; available: data-gov-sg`
- `no data source with id '…'`
