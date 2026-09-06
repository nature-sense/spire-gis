import SwiftUI

/// spire-gis — Phase 0 shell: round-trips the Rust core over the FFI and shows
/// the layer catalog from the GIS store.
struct ContentView: View {
    @Environment(CoreBridge.self) private var core
    @State private var status: GisStatus?
    @State private var layers: [GisLayer] = []
    @State private var detail = ""

    var body: some View {
        VStack(spacing: 12) {
            Text("spire-gis")
                .font(.largeTitle.weight(.semibold))

            if let status {
                Text("core \(status.core) · v\(status.version)")
                    .foregroundStyle(.secondary)
            } else {
                Text(core.statusText)
                    .foregroundStyle(.secondary)
            }

            HStack(spacing: 12) {
                Button("Reload core") { refresh() }
                Button("Import demo layer") { importDemo() }
                Button("Refresh layers") { refreshLayers() }
            }

            if layers.isEmpty {
                VStack(spacing: 4) {
                    Text("No layers imported yet").font(.callout)
                    Text(detail).font(.caption).foregroundStyle(.secondary)
                }
            } else {
                List(layers) { layer in
                    HStack {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(layer.displayName.isEmpty ? layer.name : layer.displayName)
                            Text(layer.description).font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Text("\(layer.featureCount) · \(layer.geometryType)")
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                        Button(role: .destructive) {
                            if core.gisDeleteLayer(id: layer.id) { refreshLayers() }
                        } label: {
                            Image(systemName: "trash")
                        }
                        .buttonStyle(.plain)
                        .help("Delete layer")
                    }
                }
                .frame(minHeight: 120)
            }

            Text(detail)
                .font(.body.monospaced())
                .textSelection(.enabled)
                .foregroundStyle(.secondary)

            Spacer()
        }
        .padding(24)
        .task { refresh() }
    }

    private func refresh() {
        status = core.gisStatus()
        detail = status == nil ? "status call failed (see Rust core log)" : ""
        refreshLayers()
    }

    private func refreshLayers() {
        layers = core.gisListLayers()
        if detail.isEmpty {
            detail = layers.isEmpty ? "store ready — import a layer to begin" : "\(layers.count) layer(s)"
        }
    }

    /// Import the bundled demo GeoJSON (7 zones around Singapore) as a layer.
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

