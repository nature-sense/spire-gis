# Architecture

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

spire-gis is a SwiftUI macOS app wrapped around a Rust core exposed as a cdylib
over a JSON FFI. The core is an actor system: one coordinator routes JSON
requests to specialised actors, and a memory-graph actor owns all persistence.

## Component overview

```
┌──────────────────────────────── SwiftUI host (ui/swift) ───────────────────────────────┐
│  ContentView ── MapArea/SelectionPanel ── MapView (WKWebView)                            │
│      │                                          │  MapLibre GL JS (MapHtml)              │
│  CoreBridge (dlopen libspire_gis.dylib)     MapJSBridge (script messages)                │
└──────┼──────────────────────────────────────────┼──────────────────────────────────────┘
       │ spire_send_json({"method":…,"params":…}) │ ready/log/tile/bounds/select/viewport
       ▼                                          ▼
┌──────────────────────────── Rust core (crates/spire-gis) ──────────────────────────────┐
│  lib.rs  ─ FFI entry + lazy init(AppState{runtime, senders})                             │
│    └── coordinator::route_request(graph, layers, import, tile, llm, datasources, m, p)   │
│          ├── ImportActor     fetch/parse/ingest GeoJSON → Layer+Feature nodes            │
│          ├── LayerActor      layer catalog (list/delete/reorder)                         │
│          ├── DataSourceActor configure/discover/fetch connectors (+ graph store)         │
│          ├── TileActor*      MVT tiles per z/x/y                                         │
│          ├── LlmActor*       NL → gis/query DSL                                          │
│          └── MemoryGraphActor*  nodes, edges, spatial + vector indexes, snapshots        │
└──────────────────────────────────────────────────────────────────────────────────────────┘
                                   (* from spire-core)
```

## Actors

| Actor | Owns | Messages (summary) |
| --- | --- | --- |
| `MemoryGraphActor` (spire-core) | The graph database (SeleneDB-backed), spatial + embedding indexes | `InitializeInMemory`, `StoreAttrNode`, `UpdateNode`, `DeleteNode`, `QueryAttrNodes`, `GetAttrNode`, `OpenTransactionStream`, `Sync`, `SearchContext`, `EmbedTexts`, `SetNodeEmbeddings` |
| `ImportActor` | GeoJSON → graph ingestion | `ImportGeoJsonFile`, `ImportDataGovSg`, `ImportGeoJsonText`, `MergeLayer` |
| `LayerActor` | Layer catalog queries | `ListLayers`, `DeleteLayer`, `ReorderLayer` |
| `DataSourceActor` | Connector definitions (graph-backed) + driver dispatch | `Kinds`, `List`, `Get`, `Add`, `Update`, `Delete`, `Discover`, `Fetch` |
| `TileActor` (spire-core) | MVT tile cache | `GetTile`, `ClearCache` |
| `LlmActor` (spire-core) | LLM HTTP client | NL query completion |

Actors communicate over `tokio::sync::mpsc` channels; request/reply uses
`oneshot` channels. Writes to the graph that touch many nodes go through an
**open transaction stream** (`StreamOp`s pushed to the graph actor) and are
confirmed by a final `Commit`.

## Request lifecycle

1. Swift calls `spire_send_json` with `{"method": "...", "params": {...}}`.
2. `lib.rs` lazily initializes (once) a tokio runtime, the graph actor, and the
   other actors, and stores their senders in `AppState`.
3. `coordinator::route_request` matches the method and forwards to the right
   actor, awaiting the reply.
4. The result is wrapped as `{"ok": true, "result": …}` (or
   `{"ok": false, "error": "…"}`) and returned as a heap C string the host frees
   with `spire_free_string`.

The FFI clones senders + a runtime handle under a short-lived mutex, then
**drops the lock before `block_on`**, so concurrent RPCs cannot deadlock on the
state lock.

## Persistence & snapshots

`MemoryGraphActor` is initialized in an "in-memory" mode optimized for bulk
imports: fresh stores skip per-operation WAL fsync and rely on explicit `Sync`
calls (issued after imports and embedding backfills) for durability. Stores that
already contain a snapshot recover normally on start. Snapshots live in the data
directory (`~/.spire/gis-data`).

## Error handling

- Actor boundaries carry `Result<T, String>`; the coordinator maps failures to
  the `error` field of the envelope.
- Long-running or optional work (embedding, display-cache writes) is
  **best-effort** and never fails the enclosing request.

## Concurrency notes

- Import/parse work runs inside the `ImportActor` task; the UI calls the FFI
  from background queues (`Task.detached`) and updates SwiftUI state on the main
  actor.
- The map's tile requests are answered off the main thread (`MapJSBridge.handleTile`).
