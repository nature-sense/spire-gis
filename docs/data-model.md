# Data Model

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

All GIS state lives in the memory graph as `AttrNode` envelopes distinguished by
their `node_type` discriminator. Configuration is **normalised into scalar
properties / child nodes** — no JSON blobs are used for storage (JSON appears
only as the wire format and for a few opaque display values such as MapLibre
`style`).

## Node types

### `Layer`
One imported dataset. Properties:

| Property | Meaning |
| --- | --- |
| `display_name` | Human label |
| `geometry_type` | `Point` / `LineString` / `Polygon` / `Mixed` |
| `source` | Provenance string (e.g. `data.gov.sg` or the driver kind) |
| `style` | MapLibre style template (JSON scalar) |
| `schema` | Inferred attribute schema `{ attr → type }` (JSON scalar) |
| `z_order` | Integer stacking order; higher is drawn on top |

`name` is the machine layer name (used for replace-by-name imports and as the
feature `subtype`). `description` records the provenance note.

### `Feature`
One geometry + its attributes.

| Property | Meaning |
| --- | --- |
| `layer_id` | Owning layer node id |
| `source_id` | Source identifier (`OBJECTID`/`FID`, else index) |
| *(attributes)* | Every **scalar** attribute from the source, keys sanitised (`SHAPE.LEN` → `SHAPE_LEN`) |
| `geometry` / bbox | Geometry stored via `set_spatial_geometry`, which derives the `min_*`/`max_*` bbox columns used by the spatial pre-filter |

`subtype` = machine layer name (per-layer tile filters / cache keys). Feature
geometry may be a coalesced multi-geometry (see the import pipeline). Reserved
keys (`id`, `name`, `source_id`, `layer_id`, store base keys) are never taken
from source attributes.

### `DataSource`
An external connector definition.

| Property | Meaning |
| --- | --- |
| `kind` | Driver id, e.g. `data-gov-sg` (subtype) |
| `label` | Human label |
| `config_*` | Normalised provider config scalars (e.g. `config_dataset_id`) |
| `enabled` | Whether the definition is active |
| `discovered_feature_count`, `discovered_geometry_types` | Cached discovery summary |
| timestamps | Native node fields |

### `DataSourceAttribute`
One discovered schema attribute (`source_id` FK → DataSource, `attribute`, `type`).
This mirrors a child table rather than storing the schema as JSON.

## Relationships

Relationships are optional; layer membership is currently carried on the
feature itself (`layer_id` + `subtype`) rather than per-feature edges — that
choice keeps large imports tractable. The import path writes no `CONTAINS`
edges.

## Embeddings

Feature and layer nodes are embedded for semantic search. The embedder is the
neural `sentence-transformers/all-MiniLM-L6-v2` (via Candle) when available,
falling back to a deterministic `HashEmbedder`. Embedding text is built
deterministically by `node_search_text`: name + `FOLDERPATH` + layer name, then
every descriptive scalar attribute in sorted key order (plumbing keys excluded).
Vectors are attached via `SetNodeEmbeddings` and made searchable with
`RebuildVectorIndexes`.

## Storage layout

| Path | Contents |
| --- | --- |
| `~/.spire/gis-data/` | Graph snapshots |
| `~/.spire/gis-data/display-<layer>.geojson` | Display-decimated FeatureCollection cache (invalidated on re-import) |
| `~/.spire/gis-data/ui.log` | Host-side log |

`SPIRE_GIS_DATA_DIR` overrides the directory.
