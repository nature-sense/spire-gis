import SwiftUI
import AppKit

/// spire-gis — map viewer + layer catalog. The right side hosts the MapLibre
/// map (WKWebView); the left sidebar manages imported layers and spatial queries.
struct ContentView: View {
    @Environment(CoreBridge.self) private var core

    @State private var map: MapJSBridge?
    @State private var layers: [GisLayer] = []
    @State private var visible: [String: Bool] = [:]
    @State private var classVisible: [String: Bool] = [:]
    @State private var selected: MapSelectionInfo?
    @State private var featureDetail: GisFeatureDetail?
    @State private var askText = ""
    @State private var askBusy = false
    @State private var hasQueryResults = false
    @State private var showLayerOrder = false
    @State private var pointSelectMode = false
    @State private var activeEntryID: String?
    @State private var queryLayer: String?
    @State private var showDataSources = false
    @State private var pendingFit: [Double]?

    var body: some View {
        HStack(spacing: 0) {
            iconRail
            Divider()
            sidebar
                .frame(width: 280)
            Divider()
            mapArea
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .task { startMap() }
        .sheet(isPresented: $showDataSources) {
            DataSourcesSheet(onLayersChanged: dataSourcesChanged)
                .environment(core)
        }
    }

    // MARK: Icon rail (constant shell, like spire-code)

    private var iconRail: some View {
        VStack(spacing: 6) {
            railButton(icon: "server.rack", title: "Data Sources", active: showDataSources) {
                showDataSources = true
            }
            Spacer()
        }
        .padding(.vertical, 8)
        .frame(width: 48)
        .background(.background)
    }

    private func railButton(icon: String, title: String, active: Bool,
                            action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: icon)
                .font(.system(size: 16))
                .foregroundStyle(active ? Color.accentColor : Color.secondary)
                .frame(width: 32, height: 32)
                .background(RoundedRectangle(cornerRadius: 6)
                    .fill(active ? Color.accentColor.opacity(0.15) : Color.clear))
        }
        .buttonStyle(.plain)
        .help(title)
    }

    // MARK: Sidebar

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("spire-gis")
                .font(.headline)
            if let status = core.gisStatus() {
                Text("core \(status.core) · v\(status.version)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 6) {
                TextField("Ask the map…  e.g. “parks near me”", text: $askText)
                    .textFieldStyle(.roundedBorder)
                    .font(.caption)
                    .onSubmit { runAsk() }
                Button {
                    runAsk()
                } label: {
                    Image(systemName: "magnifyingglass")
                }
                .buttonStyle(.bordered)
                .disabled(askBusy || askText.trimmingCharacters(in: .whitespaces).isEmpty)
                Button {
                    clearQuery()
                } label: {
                    Image(systemName: "xmark.circle")
                }
                .buttonStyle(.bordered)
                .disabled(!hasQueryResults)
                .help("Clear the last query results")
            }
            if askBusy {
                ProgressView().controlSize(.small)
            }

            if let map {
                Text(map.ready
                     ? (map.tilesServed > 0
                        ? "map ready · served \(map.tilesServed) tiles"
                        : "map ready · \(map.loadNote.isEmpty ? "ready" : map.loadNote)")
                     : "loading map…")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            if layers.isEmpty {
                Text("No layers yet — add one via Data Sources (left rail).")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                HStack {
                    Text("Layers")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button {
                        showLayerOrder.toggle()
                    } label: {
                        Image(systemName: "arrow.up.arrow.down")
                    }
                    .buttonStyle(.borderless)
                    .help("Layer order")
                    .popover(isPresented: $showLayerOrder, arrowEdge: .bottom) {
                        layerOrderPanel
                    }
                }
                List(layerEntries) { entry in
                    layerEntryRow(entry)
                }
                .listStyle(.inset)
                .scrollContentBackground(.hidden)
            }

            Spacer()
        }
        .padding(10)
        .background(.background)
    }

    /// Selection inspector, overlaid on the map (top-left).
    private func selectionPanel(_ sel: MapSelectionInfo) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Selected")
                .font(.caption2.weight(.bold))
                .foregroundStyle(.secondary)
            Text(sel.titleText)
                .font(.callout.weight(.semibold))
                .textSelection(.enabled)
            Text(sel.detailText)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
            if sel.stacked > 1 {
                Text("Pick \(sel.pickIndex + 1) of \(sel.stacked) stacked — click again to cycle")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            if let featureDetail {
                let rows = attributeRows(featureDetail.attributes)
                if rows.isEmpty {
                    Text("No descriptive attributes")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                } else {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(rows, id: \.0) { row in
                            HStack(alignment: .top, spacing: 6) {
                                Text(row.0)
                                    .font(.caption2.weight(.medium))
                                    .foregroundStyle(.secondary)
                                    .frame(width: 104, alignment: .leading)
                                    .lineLimit(2)
                                Text(row.1)
                                    .font(.caption)
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .lineLimit(4)
                            }
                        }
                    }
                    .padding(.top, 2)
                }
            } else if !sel.id.isEmpty {
                HStack(spacing: 4) {
                    ProgressView().controlSize(.mini)
                    Text("Loading attributes…")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }

            HStack(spacing: 10) {
                Button {
                    selected = nil
                    featureDetail = nil
                    map?.clearSelection()
                } label: {
                    Label("Clear", systemImage: "xmark.circle")
                }
                Button {
                    guard let lat = sel.lat, let lng = sel.lng else { return }
                    openStreetView(lat: lat, lng: lng)
                } label: {
                    Label("Street View", systemImage: "viewfinder")
                }
                .disabled(sel.lat == nil || sel.lng == nil
                          || !(sel.isPoint || sel.geometryKind == "circle"))
                .help(sel.isPoint || sel.geometryKind == "circle"
                      ? "Open Street View at this point"
                      : "Street View is only available for points / point picks")
            }
            .buttonStyle(.plain)
            .font(.caption)
        }
        .padding(10)
        .frame(width: 320, alignment: .leading)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Color.secondary.opacity(0.25), lineWidth: 0.5))
    }

    /// Popover: change the stacking order of whole layers (top first).
    private var layerOrderPanel: some View {
        let ordered = layers.sorted { $0.zOrder > $1.zOrder }
        let maxZ = layers.map(\.zOrder).max() ?? 0
        let minZ = layers.map(\.zOrder).min() ?? 0
        return VStack(alignment: .leading, spacing: 4) {
            Text("Layer order (top → bottom)")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(ordered) { layer in
                HStack(spacing: 6) {
                    Text(layer.displayName.isEmpty ? layer.name : layer.displayName)
                        .font(.callout)
                        .lineLimit(1)
                    Spacer()
                    Button {
                        reorderLayer(layer, direction: "up")
                    } label: {
                        Image(systemName: "arrow.up")
                    }
                    .buttonStyle(.borderless)
                    .disabled(layer.zOrder >= maxZ)
                    Button {
                        reorderLayer(layer, direction: "down")
                    } label: {
                        Image(systemName: "arrow.down")
                    }
                    .buttonStyle(.borderless)
                    .disabled(layer.zOrder <= minZ)
                }
                .padding(.vertical, 1)
            }
        }
        .padding(10)
        .frame(width: 280)
    }

    /// One entry in the sidebar layer list. Layers that have sublayers are
    /// omitted and only their sublayers are listed — everything renders the
    /// same way (checkbox + name, nothing else).
    private var layerEntries: [LayerListEntry] {
        var entries: [LayerListEntry] = []
        for layer in layers {
            if layer.classes.isEmpty {
                entries.append(LayerListEntry(
                    id: "layer::\(layer.name)",
                    kind: .layer(layer.name),
                    name: layer.displayName.isEmpty ? layer.name : layer.displayName))
            } else {
                for cls in layer.classes {
                    entries.append(LayerListEntry(
                        id: "sublayer::\(layer.name)::\(cls.key)",
                        kind: .sublayer(layer: layer.name, cls: cls.key),
                        name: shortLabel(cls.key)))
                }
            }
        }
        return entries
    }

    private func layerEntryRow(_ entry: LayerListEntry) -> some View {
        let binding = layerEntryBinding(entry)
        let on = binding.wrappedValue
        let active = activeEntryID == entry.id
        return HStack(spacing: 6) {
            Toggle(isOn: binding) {
                Text(entry.name)
                    .font(.callout)
                    .lineLimit(1)
            }
            .toggleStyle(.checkbox)
            .controlSize(.small)
            Spacer()
            if on {
                Button {
                    setActiveEntry(active ? nil : entry)
                } label: {
                    Image(systemName: active ? "largecircle.fill.circle" : "circle")
                        .font(.caption)
                        .foregroundStyle(active ? Color.accentColor : Color.secondary)
                }
                .buttonStyle(.borderless)
                .help(active
                      ? "This layer receives clicks — click to clear (back to auto)"
                      : "Restrict clicks to this layer / sublayer")
            }
        }
    }

    /// Set (or clear) the layer whose features receive map clicks.
    private func setActiveEntry(_ entry: LayerListEntry?) {
        activeEntryID = entry?.id
        switch entry?.kind {
        case .layer(let name):
            map?.setActiveLayer(layerName: name, classKey: nil)
        case .sublayer(let layerName, let cls):
            map?.setActiveLayer(layerName: layerName, classKey: cls)
        case .none:
            map?.setActiveLayer(layerName: "", classKey: nil)
        }
    }

    /// Drop the active target if its row disappeared (layer deleted/renamed).
    private func revalidateActiveEntry() {
        guard let id = activeEntryID else { return }
        if !layerEntries.contains(where: { $0.id == id }) {
            setActiveEntry(nil)
        }
    }

    private func layerEntryBinding(_ entry: LayerListEntry) -> Binding<Bool> {
        switch entry.kind {
        case .layer(let name):
            return Binding(
                get: { visible[name] ?? false },
                set: { on in
                    visible[name] = on
                    if !on {
                        clearSelectionIfAffected(layerName: name, classKey: nil)
                        if activeEntryID == entry.id { setActiveEntry(nil) }
                    }
                    map?.setVisibility(layerName: name, visible: on)
                })
        case .sublayer(let layerName, let cls):
            let key = classKey(layerName: layerName, cls: cls)
            return Binding(
                get: { classVisible[key] ?? false },
                set: { on in
                    classVisible[key] = on
                    if !on {
                        clearSelectionIfAffected(layerName: layerName, classKey: cls)
                        if activeEntryID == entry.id { setActiveEntry(nil) }
                    }
                    map?.setClassVisibility(layerName: layerName, classKey: cls, visible: on)
                })
        }
    }

    /// If the checkbox being switched OFF belongs to the currently selected
    /// feature's layer (optionally its class), drop the selection panel — the
    /// map-side selection overlay is removed by the JS visibility handler.
    private func clearSelectionIfAffected(layerName: String, classKey: String?) {
        guard let sel = selected, sel.layer == layerName else { return }
        if let classKey, !classKey.isEmpty, sel.cls != classKey { return }
        selected = nil
        featureDetail = nil
        map?.clearSelection()
    }

    // MARK: Map

    private var mapArea: some View {
        ZStack {
            if let map {
                MapView(controller: map)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                ProgressView("Loading map…")
            }
        }
        .overlay(alignment: .topTrailing) {
            zoomControls
        }
        .overlay(alignment: .topLeading) {
            if let sel = selected {
                selectionPanel(sel)
                    .padding(10)
            }
        }
        .padding(10)
    }

    private var zoomControls: some View {
        VStack(spacing: 8) {
            Button {
                pointSelectMode.toggle()
                map?.setSelectionMode(pointMode: pointSelectMode)
            } label: {
                Image(systemName: pointSelectMode ? "smallcircle.filled.circle" : "cursorarrow")
            }
            .buttonStyle(.bordered)
            .help(pointSelectMode
                  ? "Point select (snaps to the nearest vertex) — click for object select"
                  : "Object select — click for point select")
            Divider().frame(width: 24)
            Button { map?.zoomIn() } label: { Image(systemName: "plus") }
                .buttonStyle(.bordered).help("Zoom in")
            Button { map?.zoomOut() } label: { Image(systemName: "minus") }
                .buttonStyle(.bordered).help("Zoom out")
            Divider().frame(width: 24)
            Button {
                map?.queryLayer = nil
                map?.queryViewport()
                AppLog.write("viewport query: all layers")
            } label: { Image(systemName: "scope") }
            .buttonStyle(.bordered)
            .help("Query all features in the current viewport")
            Button {
                clearQuery()
            } label: { Image(systemName: "xmark.circle") }
            .buttonStyle(.bordered)
            .help("Clear query results")
        }
        .padding(8)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }

    // MARK: Actions

    private func startMap() {
        let controller = MapJSBridge(core: core)
        controller.onReady = { refreshLayers() }
        controller.onStatus = { msg in
            if !msg.isEmpty {
                AppLog.write("map: \(msg)")
            }
        }
        controller.onSelect = { info in handleFeatureSelect(info) }
        controller.queryLayer = nil
        map = controller
    }

    private func refreshLayers() {
        Task.detached(priority: .userInitiated) { [core] in
            let loaded = core.gisListLayers()
            await MainActor.run {
                self.applyLayers(loaded)
            }
        }
    }

    /// Called by the Data Sources sheet after a fetch (bounds = fit target) or
    /// a delete (nil) so the map + sidebar re-sync with the store.
    private func dataSourcesChanged(bounds: [Double]?) {
        if let bounds { pendingFit = bounds }
        refreshLayers()
    }

    @MainActor
    private func applyLayers(_ loaded: [GisLayer]) {
        layers = loaded
        for l in loaded where visible[l.name] == nil {
            visible[l.name] = false
        }
        map?.syncLayers(loaded)
        map?.orderLayers(loaded.map(\.name))
        map?.refreshLayerData(loaded)
        revalidateActiveEntry()
        if let bounds = pendingFit {
            pendingFit = nil
            map?.fitBounds(bounds)
        }
    }

    /// Adopt a new catalog order (from `gis/reorder-layer`) without reloading
    /// any feature data — only the map's style-layer stacking changes.
    @MainActor
    private func applyOrder(_ loaded: [GisLayer]) {
        layers = loaded
        map?.orderLayers(loaded.map(\.name))
        revalidateActiveEntry()
    }

    /// Move one layer a step in the z-order, then re-stack the map.
    private func reorderLayer(_ layer: GisLayer, direction: String) {
        let id = layer.id
        Task.detached(priority: .userInitiated) { [core] in
            let updated = core.gisReorderLayer(id: id, direction: direction)
            await MainActor.run {
                if updated.isEmpty {
                    self.refreshLayers()
                } else {
                    self.applyOrder(updated)
                }
            }
        }
    }

    /// Natural-language → `gis/query` (rule-based translator), then draw results.
    /// Open Google Street View at a point, reusing the SAME browser window on
    /// every call. Chromium-family and Safari browsers are driven through
    /// AppleScript (navigate the front window's active tab); anything else
    /// falls back to the default `open`.
    private func openStreetView(lat: Double, lng: Double) {
        let urlStr = "https://www.google.com/maps/@?api=1&map_action=pano&viewpoint=\(lat),\(lng)"
        guard let url = URL(string: urlStr) else { return }
        let appURL = NSWorkspace.shared.urlForApplication(toOpen: url)
        let appName = (appURL?.lastPathComponent as NSString?)?.deletingPathExtension ?? ""
        let lower = appName.lowercased()

        var script: String?
        let chromium = lower.contains("chrome") || lower.contains("chromium")
            || lower.contains("edge") || lower.contains("brave")
            || lower.contains("opera") || lower.contains("arc")
            || lower.contains("vivaldi") || lower.contains("browser")
        if chromium && !appName.isEmpty {
            script = """
            tell application "\(appName)"
              activate
              if (count of windows) is 0 then
                open location "\(urlStr)"
              else
                set URL of active tab of front window to "\(urlStr)"
              end if
            end tell
            """
        } else if lower.contains("safari") && !appName.isEmpty {
            script = """
            tell application "Safari"
              activate
              if (count of documents) is 0 then
                open location "\(urlStr)"
              else
                set URL of front document to "\(urlStr)"
              end if
            end tell
            """
        }

        if let script {
            Task.detached(priority: .userInitiated) {
                let proc = Process()
                proc.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
                proc.arguments = ["-e", script]
                do {
                    try proc.run()
                    proc.waitUntilExit()
                } catch { /* fall through to default open */ }
                if proc.terminationStatus != 0 {
                    NSWorkspace.shared.open(url)
                }
            }
        } else {
            NSWorkspace.shared.open(url)
        }
    }

    /// Remove the last query's map highlights.
    private func clearQuery() {
        map?.clearResults()
        hasQueryResults = false
    }

    /// Map click → remember the feature and fetch its full attribute map
    /// (`gis/get-feature`) so the selection panel can show every attribute.
    private func handleFeatureSelect(_ info: MapSelectionInfo?) {
        guard let info else {
            selected = nil
            featureDetail = nil
            return
        }
        selected = info
        featureDetail = nil
        let id = info.id
        guard !id.isEmpty else { return }
        let core = self.core
        Task.detached(priority: .userInitiated) {
            let detail = core.gisGetFeature(id: id)
            await MainActor.run {
                // Only apply if the selection hasn't moved on meanwhile.
                if self.selected?.id == id {
                    self.featureDetail = detail
                }
            }
        }
    }

    /// Attribute keys that are plumbing / ids, never shown to the user.
    private func isPlumbingAttribute(_ key: String) -> Bool {
        ["OBJECTID", "objectid", "FID", "fid", "ID", "id"].contains(key)
            || key.hasPrefix("min_")
            || key.hasPrefix("max_")
    }

    /// Descriptive attributes as sorted (key, value) rows.
    private func attributeRows(_ attrs: [String: GisJSON]) -> [(String, String)] {
        attrs
            .compactMap { key, value in
                guard !isPlumbingAttribute(key) else { return nil }
                let text = CoreBridge.scalarText(value)
                guard !text.isEmpty else { return nil }
                return (key, text)
            }
            .sorted { $0.0.lowercased() < $1.0.lowercased() }
    }

    private func runAsk() {
        guard let map else { return }
        let q = askText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty, !askBusy else { return }
        askBusy = true
        let vp = map.viewport
        let snapshot = layers
        Task.detached(priority: .userInitiated) { [core] in
            let plan = NLQueryEngine.translate(q, layers: snapshot, viewport: vp)
            var fc: String?
            var total: Int?
            var headline = plan.summary

            // 1) LLM translator (primary): free text → gis/query DSL.
            if let llmRaw = core.gisNlQuery(text: q, viewport: vp),
               let data = llmRaw.data(using: .utf8),
               let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                let isFallback = obj["fallback"] as? Bool ?? false
                if !isFallback,
                   let f = obj["features"],
                   let fd = try? JSONSerialization.data(withJSONObject: f) {
                    fc = String(data: fd, encoding: .utf8)
                    total = (obj["total"] as? NSNumber)?.intValue
                    if let s = obj["summary"] as? String, !s.isEmpty {
                        headline = s
                    }
                }
            }

            // 2) Offline fallbacks: rule tables carry a concrete intent →
            //    structured gis/query; otherwise SeleneDB semantic search.
            if fc == nil {
                let semantic = !plan.hadIntent
                let raw = semantic
                    ? core.gisSemanticSearch(text: q, scope: "Feature", limit: 50)
                    : core.gisQuery(params: plan.params)
                if let raw,
                   let data = raw.data(using: .utf8),
                   let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                    total = (obj["total"] as? NSNumber)?.intValue
                    if let f = obj["features"],
                       let fd = try? JSONSerialization.data(withJSONObject: f) {
                        fc = String(data: fd, encoding: .utf8)
                    }
                }
                if semantic {
                    headline = "Semantic search — no alias rule matched"
                }
            }

            await MainActor.run {
                self.askBusy = false
                var log = headline
                if let fc {
                    self.map?.showQueryResults(fc)
                    self.hasQueryResults = true
                } else {
                    self.hasQueryResults = false
                }
                if let total {
                    log += "\n→ \(total) \(total == 1 ? "result" : "results")"
                } else {
                    log += "\n→ query failed (no result)"
                }
                AppLog.write(log)
            }
        }
    }
}

// MARK: Classification helpers (mirror MapHtml LINE_STYLES / POLY_STYLES)

private func classKey(layerName: String, cls: String) -> String {
    "\(layerName)::\(cls)"
}

// MARK: Flat layer-list model

/// One row in the sidebar layer list: either a whole layer (when it has no
/// sublayers) or a single sublayer. Both render identically.
private enum LayerListKind {
    case layer(String)
    case sublayer(layer: String, cls: String)
}

private struct LayerListEntry: Identifiable {
    let id: String
    let kind: LayerListKind
    let name: String
}

private func colorHex(_ rgb: UInt32) -> Color {
    Color(.sRGB,
          red: Double((rgb >> 16) & 0xFF) / 255.0,
          green: Double((rgb >> 8) & 0xFF) / 255.0,
          blue: Double(rgb & 0xFF) / 255.0)
}

private func classColor(_ key: String) -> Color {
    switch key {
    case "Layers/Expressway": return colorHex(0x2563eb)          // medium blue
    case "Layers/Expressway_Sliproad": return colorHex(0xf4a261) // amber
    case "Layers/Major_Road": return colorHex(0xe63946)          // red
    case "Layers/Contour_250K": return colorHex(0xb39b7d)        // light brown
    case "Layers/International_bdy": return colorHex(0x264653)   // dark navy
    case "Layers/Hydrographic": return colorHex(0x4aa3df)        // water blue
    case "Layers/Coastal_Outlines": return colorHex(0xb3a98c)    // taupe
    case "Layers/Parks_NaturalReserve": return colorHex(0x7bbf6a) // green
    case "Layers/Airport_Runway": return colorHex(0x9aa0a6)      // gray
    case "Layers/Central_Business_District": return colorHex(0xf2c94c) // amber
    default: return .gray
    }
}

/// Colour for classless layers — mirrors MapHtml `LAYER_COLORS`.
private func layerColorHex(_ name: String) -> Color {
    switch name {
    case "nparks-nature-reserves": return colorHex(0x2d6a4f)     // deep forest green
    case "parks": return colorHex(0x52b788)                       // park green
    case "heritage-trees": return colorHex(0x40916c)              // tree green
    case "park-connector-loop": return colorHex(0x95d5b2)         // light connector green
    case "tree-conservation-area": return colorHex(0x1b4332)      // very dark green
    case "heritage-road-green-buffers": return colorHex(0xa7c957) // yellow-green
    case "nparks-tracks": return colorHex(0x4d908e)               // teal
    case "community-in-bloom": return colorHex(0xd81b60)          // blossom pink
    case "natureways": return colorHex(0x80b918)                  // vivid lime green
    case "shoreline-typology": return colorHex(0x0077b6)          // coastal blue
    default: return .gray
    }
}

private func shortLabel(_ key: String) -> String {
    let tail = key.split(separator: "/").last.map(String.init) ?? key
    return tail.replacingOccurrences(of: "_", with: " ")
}

extension MapSelectionInfo {
    var titleText: String {
        if isPoint { return "Point" }
        if !name.isEmpty { return name }
        return "Unnamed feature"
    }
    var detailText: String {
        var parts: [String] = []
        if isPoint {
            if let lat, let lng { parts.append(String(format: "%.5f, %.5f", lat, lng)) }
            parts.append(vertexIndex >= 0 ? "vertex \(vertexIndex)" : "point")
            if !layer.isEmpty { parts.append(layer) }
            if !cls.isEmpty { parts.append(shortLabel(cls)) }
            return parts.joined(separator: " · ")
        }
        if !cls.isEmpty { parts.append(shortLabel(cls)) }
        if !layer.isEmpty {
            parts.append(layer == "results" ? "search result" : layer)
        }
        return parts.joined(separator: " · ")
    }
}


