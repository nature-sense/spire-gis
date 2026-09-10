# Data Sources

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

A **data source** is a persisted connector definition: a driver `kind` plus
provider-specific `config`. Definitions are distinct from the layers they
produce.

## Model

```rust
pub struct DataSource {
    pub id: String,
    pub kind: String,          // driver id, e.g. "data-gov-sg"
    pub label: String,         // human label; also the produced layer name
    pub config: Value,         // provider config ({"dataset_id": "…"})
    pub refresh: RefreshPolicy,// Manual today (interval is future work)
    pub enabled: bool,
    pub discovered: Option<Discovery>,
    pub created_at: String,
    pub updated_at: String,
}
```

- `Discovery` caches `feature_count`, `geometry_types` and `schema` from the
  last `discover`, persisted as scalar node properties + `DataSourceAttribute`
  child nodes.
- `DatasetInfo` is the full discovery result (adds `name`, `description`) used
  for the RPC reply.
- `DatasetPayload` is what `fetch` returns (currently `GeoJsonText`).

## Drivers

```rust
#[async_trait]
pub trait DataSourceDriver: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn discover(&self, config: &Value) -> Result<DatasetInfo, String>;
    async fn fetch(&self, config: &Value) -> Result<DatasetPayload, String>;
}
```

`DriverRegistry` maps `kind → driver` and owns the built-in set
(`DriverRegistry::builtin()`). Registration rejects duplicate kinds.

### `data-gov-sg`

- Config: `config.dataset_id` (the only required setting).
- `discover` — downloads the dataset and returns metadata + inferred attribute
  schema **without importing anything**.
- `fetch` — downloads the GeoJSON text; the actor then imports it through the
  shared pipeline.

Download flow: poll
`https://api-open.data.gov.sg/v1/public/api/datasets/{id}/poll-download`
(up to 8 attempts, 3 s apart) until a signed URL is ready, then download the
payload as UTF-8 GeoJSON.

## Actor & store

`DataSourceActor` owns a `GraphDataSourceStore` (async CRUD against the memory
graph) and the `DriverRegistry`, and holds a handle to `ImportActor`.

| Message | Behaviour |
| --- | --- |
| `Kinds` | Registered driver kinds |
| `List` / `Get` | Read definitions |
| `Add` | Validate the kind exists, then persist a new definition |
| `Update` | Patch `label` / `config` / `enabled`, bump the timestamp |
| `Delete` | Remove the definition |
| `Discover` | Run the driver's `discover`, cache the summary on the node, return `DatasetInfo` |
| `Fetch` | Run the driver's `fetch`, then hand the payload to `ImportActor` (replace-by-name using the definition label), and record the fetch time |

`Fetch` imports via `ImportMessage::ImportGeoJsonText`, so every driver reuses
the same ingestion path (see [Import pipeline](import-pipeline.md)). A
successful import also embeds the new features for semantic search.

## UI

The Data Sources panel (opened from the left icon rail) is a three-column modal:

1. **Data source** — provider kinds (`gis/datasource/kinds`).
2. **Layers** — definitions under the kind, with **+** to add (label + dataset
   id — no JSON).
3. **Config & state** — label / dataset id, enabled, **Discover**, **Import**,
   produced-layer state incl. sublayer names, and **Delete** (removes the
   definition *and* its imported layer).

## Related RPCs

`gis/datasource/kinds`, `/list`, `/get`, `/add`, `/update`, `/delete`,
`/discover`, `/fetch` — see [RPC API](rpc-api.md).
