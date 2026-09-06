import SwiftUI

/// spire-gis — map viewer + layer catalog. The right side hosts the MapLibre
/// map (WKWebView); the left sidebar manages imported layers and spatial queries.
struct ContentView: View {
    @Environment(CoreBridge.self) private var core

    @State private var map: MapJSBridge?
    @State private var layers: [GisLayer] = []
    @State private var visible: [String: Bool] = [:]
    @State private var detail = ""
    @State private var queryLayer: String?
    @State private var importing = false
    @State private var pendingFit: [Double]?

    var body: some View {
        HStack(spacing: 0) {
            sidebar
                .frame(width: 280)
            Divider()
            mapArea
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .task { startMap() }
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

            Button {
                importDataGov(datasetId: "d_29f066d67df3eae91df8a42f443863c8",
                              name: "national-map-polygon", displayName: "National Map Polygon")
            } label: {
                if importing { ProgressView().controlSize(.small) }
                Label("Import National Map Polygon", systemImage: "arrow.down.circle")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .disabled(importing)

            Button {
                importDataGov(datasetId: "d_10480c0b59e65663dfae1028ff4aa8bb",
                              name: "national-map-line", displayName: "National Map Lines")
            } label: {
                if importing { ProgressView().controlSize(.small) }
                Label("Import National Map Lines", systemImage: "arrow.down.circle")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .disabled(importing)

            Button { importDemo() } label: {
                Label("Import demo layer", systemImage: "plus.circle")
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.small)

            if let map {
                Text(map.ready ? "map ready · served \(map.tilesServed) tiles"
                              : "loading map…")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            if layers.isEmpty {
                Text("No layers yet — import the demo to see the map.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                List {
                    ForEach(layers) { layer in
                        layerRow(layer)
                    }
                }
                .scrollContentBackground(.hidden)
            }

            Text(detail)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .textSelection(.enabled)

            Spacer()
        }
        .padding(10)
        .background(.background)
    }

    private func layerRow(_ layer: GisLayer) -> some View {
        HStack(spacing: 6) {
            Toggle("", isOn: Binding(
                get: { visible[layer.name] ?? true },
                set: { on in
                    visible[layer.name] = on
                    map?.setVisibility(layerName: layer.name, visible: on)
                }
            ))
            .labelsHidden()
            .toggleStyle(.switch)
            .controlSize(.small)

            VStack(alignment: .leading, spacing: 1) {
                Text(layer.displayName.isEmpty ? layer.name : layer.displayName)
                    .font(.callout)
                Text("\(layer.featureCount) · \(layer.geometryType)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }

            Spacer()

            Button { runViewportQuery(for: layer) } label: {
                Image(systemName: "scope")
            }
            .buttonStyle(.plain)
            .help("Query features in the current viewport for this layer")

            Button(role: .destructive) {
                if core.gisDeleteLayer(id: layer.id) { refreshLayers() }
            } label: {
                Image(systemName: "trash")
            }
            .buttonStyle(.plain)
            .help("Delete layer")
        }
        .padding(.vertical, 2)
    }

    // MARK: Map

    private var mapArea: some View {
        ZStack(alignment: .topTrailing) {
            if let map {
                MapView(controller: map)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                ProgressView("Loading map…")
            }

            VStack(spacing: 8) {
                Button { map?.zoomIn() } label: { Image(systemName: "plus") }
                    .buttonStyle(.bordered).help("Zoom in")
                Button { map?.zoomOut() } label: { Image(systemName: "minus") }
                    .buttonStyle(.bordered).help("Zoom out")
                Divider().frame(width: 24)
                Button {
                    map?.queryLayer = nil
                    map?.queryViewport()
                    detail = "querying current viewport…"
                } label: { Image(systemName: "scope") }
                .buttonStyle(.bordered)
                .help("Query all features in the current viewport")
                Button {
                    map?.clearResults()
                    detail = "results cleared"
                } label: { Image(systemName: "xmark.circle") }
                .buttonStyle(.bordered)
                .help("Clear query results")
            }
            .padding(8)
            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 10))
            .padding(10)
        }
        .padding(10)
    }

    // MARK: Actions

    private func startMap() {
        let controller = MapJSBridge(core: core)
        controller.onReady = { refreshLayers() }
        controller.onStatus = { msg in
            if !msg.isEmpty { detail = msg }
        }
        controller.queryLayer = nil
        map = controller
    }

    private func refreshLayers() {
        layers = core.gisListLayers()
        for l in layers where visible[l.name] == nil {
            visible[l.name] = true
        }
        map?.syncLayers(layers)
        map?.refreshLayerData(layers)
        if let bounds = pendingFit {
            pendingFit = nil
            map?.fitBounds(bounds)
        }
    }

    /// Import a data.gov.sg GeoJSON dataset on a background thread (network +
    /// store writes can take a while), then show + fit it on the map.
    private func importDataGov(datasetId: String, name: String, displayName: String) {
        guard !importing else { return }
        importing = true
        detail = "importing \(displayName) from data.gov.sg…"
        Task.detached(priority: .userInitiated) {
            let report = self.core.gisImportDataGovSg(datasetId: datasetId,
                                                      name: name,
                                                      displayName: displayName)
            await MainActor.run {
                self.importing = false
                guard let report else {
                    self.detail = "\(displayName) import failed (see Rust core log)"
                    return
                }
                self.pendingFit = report.bounds
                self.detail = "\(displayName): \(report.featureCount) \(report.geometryType) features imported"
                self.refreshLayers()
            }
        }
    }

    private func runViewportQuery(for layer: GisLayer) {
        map?.queryLayer = layer.name
        map?.queryViewport()
        detail = "querying '\(layer.name)' in viewport…"
    }

    private func importDemo() {
        guard let url = Bundle.main.url(forResource: "singapore-zones",
                                        withExtension: "geojson",
                                        subdirectory: "fixtures") else {
            detail = "demo fixture not bundled — run `make app`"
            return
        }
        detail = "importing…"
        let report = core.gisImportGeoJsonFile(path: url.path,
                                               name: "singapore-zones",
                                               displayName: "Singapore Demo Zones")
        if let report {
            detail = "imported \(report.featureCount) features as '\(report.name)' (\(report.geometryType))"
        } else {
            detail = "import failed (see Rust core log)"
        }
        refreshLayers()
    }
}


