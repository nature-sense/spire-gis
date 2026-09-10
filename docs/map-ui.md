# Map UI

<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Copyright (c) 2026 NatureSense -->

The host is a SwiftUI app (`ui/swift`, executable `SpireUI`) embedding a
`WKWebView` that runs MapLibre GL JS.

## Swift layer

| File | Role |
| --- | --- |
| `App.swift` | Window + `CoreBridge` environment |
| `Bridge/CoreBridge.swift` | `dlopen`s `libspire_gis.dylib`, sends JSON RPCs, decodes results |
| `Bridge/MapJSBridge.swift` | `WKScriptMessageHandler`; mediates map ↔ core; typed `Gis*` models |
| `MapHtml.swift` | The MapLibre page (HTML/JS) |
| `ContentView.swift` | Shell: icon rail, sidebar, map area, selection panel |
| `AppLog.swift` | File logger (`~/.spire/gis-data/ui.log`) |

`CoreBridge` looks for the dylib in the app bundle's `Contents/Frameworks`, then
`<repo>/target/release/`.

## Window shell

```
┌──────┬───────────────────────────┬───────────────────────────────┐
│ rail │ sidebar (280)             │ map area                      │
│ 48   │  ask field                │   ┌ top-left: selection panel │
│      │  Layers header + order    │   └ top-right: map controls   │
│      │  flat layer/sublayer list │                               │
└──────┴───────────────────────────┴───────────────────────────────┘
```

- **Icon rail** — entry point for the **Data Sources** modal (3 columns:
  provider · layers · config/state).
- **Sidebar** — natural-language "ask" box, a **Layers** header with an
  **order** popover, and the flat list.
- **Selection panel** — overlaid on the map (top-left); shows the picked
  feature/point, its attributes, Clear and Street View.
- **Map controls** — overlaid top-right: selection-mode toggle, zoom, viewport
  query, clear results.

## Layer list

The sidebar shows **one flat list**: a layer with no sublayers appears as one
row; a layer *with* sublayers shows only its sublayers. Every row is a
**checkbox + name**. Two extras:

- **Active click target** — a radio that appears when a row is checked. When
  set, map clicks resolve **only** against that layer/sublayer, so overlapping
  layers cannot steal the pick. Click again to clear (auto mode).
- **Stacking order** — the header's ↕ popover lists layers top → bottom with
  ▲/▼ buttons backed by `gis/reorder-layer`; the map is re-stacked live.

All visibility defaults are **off** at launch.

## Map page (`MapHtml.swift`)

- One GeoJSON source per layer: `spire-<name>` (with `promoteId: 'id'`).
- Style layers: `spire-<name>-layer` for classless layers; one
  `spire-<name>-cls:<class>` per FOLDERPATH class otherwise. All start hidden.
- **Temporary overlays**: `spire-sel-*` (click selection), `spire-hl-*` (query
  matches) and `spire-pointsel-layer` (point marker). These are tracked with
  their source/class and are **removed when the owning layer/sublayer is turned
  off**, so nothing hidden stays on screen.

### Selection

- **Object mode** — clicks pick the topmost feature; repeated clicks at the same
  spot **cycle** through the stack. The selection panel offers Street View only
  for Point features / point picks.
- **Point mode** — toggled from the map controls (cursor becomes an arrow);
  clicks snap to the **nearest vertex** of the top feature (`vertexIndex`), or
  fall back to the raw click coordinate. Street View opens at that exact point.
- **Active target** (sidebar) scopes both modes to one layer/sublayer.

### Messages (JS → Swift)

`ready`, `log` (file-logged), `tile` (MVT request), `bounds` (viewport query),
`select` (feature/point pick), `viewport` (moveend).

## Logging

On-screen status logging has been removed; the host appends map status, query
summaries and import messages to `~/.spire/gis-data/ui.log`.
