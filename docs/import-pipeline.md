# Import Pipeline

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

Every ingest path (data.gov.sg fetch, GeoJSON file, raw text) converges on
`ImportActor::import_geojson_text`.

## Steps

1. **Parse** — `parse_geojson_stream` decodes the document into
   `DecodedFeature { geometry, properties }`. The raw text is dropped right
   after decoding so only the decoded features stay resident.
2. **Classify** — geometry kinds are collected to compute the layer's
   `geometry_type` (`Point` / `LineString` / `Polygon` / `Mixed`).
3. **Schema** — `infer_schema` runs over `sanitize_attributes` keys, so the
   reported schema matches exactly what will be stored.
4. **Layer node** — built by `layer_node` (display name, description, source,
   style, schema, `z_order`). The z-order is `max(existing)+1`, so a new import
   lands on top of the stack.
5. **Replace-by-name** — `replace_layer(machine_name)` deletes any previous
   layer with the same machine name (and its features) so re-imports are
   idempotent.
6. **Write** — one `OpenTransactionStream`; the layer node and every feature
   node are pushed as `StoreNode` ops; a final `Commit` confirms the batch.
   Callers roll the stream back on error (dropping the sender would otherwise
   auto-commit a half-written layer).
7. **Sync** — an explicit `Sync` snapshots the store (debounced snapshots alone
   are not guaranteed before exit).
8. **Cache invalidation** — the per-layer `display-<name>.geojson` cache is
   removed so the next `get-layer-geojson` regenerates it.
9. **Embed (best-effort)** — the new layer's feature nodes are embedded
   (`EnsureEmbeddingVectorIndex` → batched `EmbedTexts` → `SetNodeEmbeddings` →
   `RebuildVectorIndexes` → `Sync`). Runs silently skipped when no embedder is
   configured.

## Attribute handling

- `sanitize_attributes` keeps only **scalar** values (string / number / bool)
  with GQL-safe, non-reserved keys. Arrays/objects/null are dropped; strings are
  control-character-stripped and length-capped.
- Keys are sanitised to `[A-Za-z_][A-Za-z0-9_]*` (e.g. `SHAPE.LEN` →
  `SHAPE_LEN`).

## Feature coalescing

Named line/polygon fragments (roads split into 2-vertex segments; parks split
into adjacent polygons) are merged into **one node per (geometry kind, class,
NAME)** so queries return whole shapes. Points and unnamed features are never
merged. Merged nodes keep the first fragment's attributes and a merged geometry.

`MergeLayer` re-runs this coalescing in place for an already-stored layer (used
when the original source file is no longer available).

## Result

The import reply contains:

```json
{ "layer_id": "…", "name": "…", "display_name": "…", "geometry_type": "Polygon",
  "feature_count": 1234, "source": "…", "bounds": [minLng,minLat,maxLng,maxLat] }
```

`bounds` is `null` when no geometry produced a valid extent.
