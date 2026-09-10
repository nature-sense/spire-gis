import SwiftUI

/// Data Sources panel (3 columns):
///   1. provider kind (e.g. data.gov.sg)   2. that provider's layer definitions
///   (+ to add)   3. selected definition: inline config, Discover/Import, state
///   (incl. produced-layer sublayer names) and Delete.
struct DataSourcesSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(CoreBridge.self) private var core

    /// Called after a fetch/delete so the map + main sidebar re-sync. Non-nil
    /// bounds ask the caller to fit the map to the freshly imported layer.
    let onLayersChanged: (([Double]?) -> Void)?

    // Column 1: provider kinds.
    @State private var kinds: [String] = ["data-gov-sg"]
    @State private var selectedKind: String?

    // Column 2: definitions under the selected kind.
    @State private var sources: [GisDataSource] = []
    @State private var layers: [GisLayer] = []
    @State private var selectedID: String?

    // Column 3: inline-edit drafts.
    @State private var editLabel = ""
    @State private var editDatasetID = ""
    @State private var enabledDraft = true

    // Add-new form.
    @State private var adding = false
    @State private var addLabel = ""
    @State private var addDatasetID = ""

    // Discover / fetch results + busy/status.
    @State private var discoverInfo: GisDatasetInfo?
    @State private var lastImport: GisImportReport?
    @State private var busyAction: String?
    @State private var status = ""
    @State private var errorText: String?
    @State private var confirmDelete = false

    private var selectedSource: GisDataSource? {
        guard let selectedID else { return nil }
        return sources.first { $0.id == selectedID }
    }

    /// True while the column-3 drafts differ from the persisted definition.
    private var isDirty: Bool {
        guard let source = selectedSource else { return false }
        return editLabel != source.label
            || enabledDraft != source.enabled
            || editDatasetID != CoreBridge.datasetID(from: source.config)
    }

    /// The layer this definition produced (fetch imports with name == label).
    private var producedLayer: GisLayer? {
        guard let source = selectedSource else { return nil }
        return layers.first { $0.name == source.label }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HStack(spacing: 0) {
                kindColumn
                    .frame(width: 160)
                Divider()
                sourceColumn
                    .frame(width: 250)
                Divider()
                detailColumn
                    .frame(maxWidth: .infinity)
            }
            Divider()
            footer
        }
        .frame(width: 1000, height: 560)
        .task { reload() }
        .onChange(of: selectedID) { _, _ in
            clearTransients()
            seedDrafts()
        }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "server.rack")
                .foregroundStyle(Color.accentColor)
            Text("Data Sources")
                .font(.headline)
            Spacer()
            if let kind = selectedKind {
                Text(displayKind(kind))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Button {
                dismiss()
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.borderless)
            .help("Close")
        }
        .padding(10)
    }

    // MARK: Column 1 — providers

    private var kindColumn: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Data source")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(kinds, id: \.self) { kind in
                kindRow(kind)
            }
            Spacer()
        }
        .padding(10)
    }

    private func kindRow(_ kind: String) -> some View {
        let selected = selectedKind == kind
        return Button {
            selectedKind = kind
            selectedID = sources.first { $0.kind == kind }?.id
            adding = false
            seedDrafts()
        } label: {
            HStack(spacing: 6) {
                Image(systemName: "cylinder.split.1x2")
                    .font(.caption)
                Text(displayKind(kind))
                    .font(.callout)
                    .lineLimit(1)
                Spacer()
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .background(RoundedRectangle(cornerRadius: 6)
                .fill(selected ? Color.accentColor.opacity(0.14) : Color.clear))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    // MARK: Column 2 — layers (definitions) under the selected provider

    private var sourceColumn: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Layers")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                Button {
                    startAdd()
                } label: {
                    Image(systemName: "plus.circle")
                }
                .buttonStyle(.borderless)
                .disabled(selectedKind == nil || adding)
                .help("Add a layer definition")
            }
            let mine = sources.filter { $0.kind == selectedKind }
            if mine.isEmpty {
                Text(adding ? "Configure the new layer →" : "No layers — tap + to add one.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .padding(.vertical, 6)
            } else {
                List(selection: $selectedID) {
                    ForEach(mine) { source in
                        sourceRow(source)
                            .tag(source.id)
                    }
                }
                .listStyle(.sidebar)
                .scrollContentBackground(.hidden)
            }
            Spacer(minLength: 0)
        }
        .padding(8)
    }

    private func sourceRow(_ source: GisDataSource) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(source.label)
                .font(.callout.weight(source.id == selectedID ? .semibold : .regular))
                .lineLimit(1)
            HStack(spacing: 4) {
                Image(systemName: source.enabled ? "checkmark.circle.fill" : "pause.circle")
                    .font(.system(size: 9))
                    .foregroundStyle(source.enabled ? .green : .secondary)
                Text(displayKind(source.kind))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                if let d = source.discovered {
                    Text("· \(d.featureCount) feat")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .padding(.vertical, 2)
    }

    // MARK: Add-new form (shown in column 3)

    private func startAdd() {
        guard !adding else { return }
        adding = true
        addLabel = ""
        addDatasetID = ""
        discoverInfo = nil
        errorText = nil
    }

    private func cancelAdd() {
        adding = false
        seedDrafts()
    }


    // MARK: Column 3 — config + state

    @ViewBuilder
    private var detailColumn: some View {
        if adding {
            addForm
        } else if let source = selectedSource {
            editForm(source)
        } else {
            VStack(spacing: 8) {
                Spacer()
                Image(systemName: "server.rack")
                    .font(.largeTitle)
                    .foregroundStyle(.tertiary)
                Text("Select a layer on the left\nor tap + to add one.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Spacer()
            }
            .frame(maxWidth: .infinity)
        }
    }

    private var addForm: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("New layer definition")
                .font(.callout.weight(.semibold))
            Text("Provider: \(displayKind(selectedKind ?? "data-gov-sg"))")
                .font(.caption)
                .foregroundStyle(.secondary)

            Text("Label (optional — defaults to the dataset id)")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            TextField("e.g. National Map Lines", text: $addLabel)
                .textFieldStyle(.roundedBorder)

            Text("Dataset ID")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            TextField("d_…", text: $addDatasetID)
                .textFieldStyle(.roundedBorder)
                .font(.system(.body, design: .monospaced))

            HStack {
                Button("Add") { performAdd() }
                    .disabled(addDatasetID.trimmingCharacters(in: .whitespaces).isEmpty)
                Button("Cancel") { cancelAdd() }
                Spacer()
            }
            Spacer()
        }
        .padding(12)
    }

    private func editForm(_ source: GisDataSource) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                Text("Layer config")
                    .font(.callout.weight(.semibold))

                Text("Label")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                TextField(source.label, text: $editLabel)
                    .textFieldStyle(.roundedBorder)

                Text("Dataset ID")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                TextField("d_…", text: $editDatasetID)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(.body, design: .monospaced))

                Toggle("Enabled", isOn: $enabledDraft)
                    .toggleStyle(.switch)
                    .controlSize(.small)

                if isDirty {
                    Text("Unsaved changes — press Save to apply.")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                }

                HStack(spacing: 8) {
                    Button("Save") { performSave(source.id) }
                        .disabled(!isDirty || busyAction != nil)
                    Spacer()
                    actionButton("Discover", icon: "eye", busy: busyAction == "discover") {
                        performDiscover(source.id)
                    }
                    actionButton("Import", icon: "arrow.down.circle", busy: busyAction == "fetch") {
                        performFetch(source.id)
                    }
                }
                .padding(.bottom, 2)

                Divider()

                stateSection(source)
            }
            .padding(12)
        }
    }

    private func actionButton(_ title: String, icon: String, busy: Bool,
                              action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                if busy {
                    ProgressView().controlSize(.mini)
                } else {
                    Image(systemName: icon)
                }
                Text(title)
            }
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .disabled(busyAction != nil || busy)
    }


    // MARK: State (discovery summary + produced layer + sublayers)

    private func stateSection(_ source: GisDataSource) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("State")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)

            // Discovery (fresh from Discover, else the cached summary).
            if let info = discoverInfo {
                Label("Discovered: \(info.name)", systemImage: "doc.text.magnifyingglass")
                    .font(.caption)
                Text("\(info.featureCount) features · \(info.geometryTypes.joined(separator: ", "))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if case .object(let schema) = info.schema {
                    Text(schema.keys.joined(separator: ", "))
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .lineLimit(2)
                }
            } else if let d = source.discovered {
                Label("Last discovery", systemImage: "checkmark.seal")
                    .font(.caption)
                Text("\(d.featureCount) features · \(d.geometryTypes.joined(separator: ", "))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("Not discovered yet — press Discover to validate + inspect the schema.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }

            Divider()

            // Produced layer (fetch imports with name == label).
            if let layer = producedLayer {
                Label("Imported layer", systemImage: "map.fill")
                    .font(.caption)
                    .foregroundStyle(.green)
                Text(layer.displayName.isEmpty ? layer.name : layer.displayName)
                    .font(.callout.weight(.semibold))
                Text("\(layer.featureCount) features · \(layer.geometryType)")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                if !layer.classes.isEmpty {
                    Text("Sublayers")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .padding(.top, 2)
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(layer.classes, id: \.key) { cls in
                            HStack(spacing: 4) {
                                RoundedRectangle(cornerRadius: 2)
                                    .fill(classColor(cls.key))
                                    .frame(width: 12, height: 7)
                                Text(shortLabel(cls.key))
                                    .font(.caption)
                                    .lineLimit(1)
                                Spacer()
                                Text("\(cls.count)")
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                    .padding(.leading, 6)
                }
            } else {
                Text("No imported layer yet — press Import to fetch + import from this definition.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }

            if let report = lastImport {
                Text("Last import: \(report.featureCount) \(report.geometryType) features")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }

            Divider()

            // Danger zone.
            Button(role: .destructive) {
                confirmDelete = true
            } label: {
                Label("Delete layer definition", systemImage: "trash")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .disabled(busyAction != nil)
            .confirmationDialog(
                "Delete “\(source.label)”?",
                isPresented: $confirmDelete,
                titleVisibility: .visible
            ) {
                Button("Delete definition and imported layer", role: .destructive) {
                    performDelete(source)
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Removes the data-source definition and, if present, its imported layer from the map.")
            }
        }
    }

    // MARK: Footer

    private var footer: some View {
        HStack(spacing: 8) {
            if let errorText {
                Label(errorText, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.red)
            } else if !status.isEmpty {
                Text(status)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if busyAction != nil {
                ProgressView().controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .frame(minHeight: 30)
    }


    // MARK: Actions (FFI on a background queue, UI updates on MainActor)

    private func reload() {
        let core = self.core
        Task.detached(priority: .userInitiated) {
            let kinds = core.gisDatasourceKinds()
            let sources = core.gisDatasourceList()
            let layers = core.gisListLayers()
            await MainActor.run {
                self.apply(kinds: kinds, sources: sources, layers: layers)
            }
        }
    }

    @MainActor
    private func apply(kinds: [String], sources: [GisDataSource], layers: [GisLayer]) {
        self.kinds = kinds.isEmpty ? ["data-gov-sg"] : kinds
        self.sources = sources
        self.layers = layers
        if let selectedKind, !self.kinds.contains(selectedKind) {
            self.selectedKind = self.kinds.first
        }
        if let selectedID, !sources.contains(where: { $0.id == selectedID }) {
            self.selectedID = sources.filter { $0.kind == selectedKind }.first?.id
        }
        seedDrafts()
    }

    private func seedDrafts() {
        guard !adding else { return }
        guard let source = selectedSource else {
            editLabel = ""
            editDatasetID = ""
            enabledDraft = true
            return
        }
        editLabel = source.label
        editDatasetID = CoreBridge.datasetID(from: source.config)
        enabledDraft = source.enabled
    }

    /// Reset per-selection Discover/Import results (kept across reloads while
    /// the same definition stays selected).
    private func clearTransients() {
        discoverInfo = nil
        lastImport = nil
    }

    private func performAdd() {
        guard let kind = selectedKind else { return }
        let datasetID = addDatasetID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !datasetID.isEmpty else { return }
        var label = addLabel.trimmingCharacters(in: .whitespacesAndNewlines)
        if label.isEmpty { label = datasetID }
        let core = self.core
        busyAction = "add"
        errorText = nil
        Task.detached(priority: .userInitiated) {
            let added = core.gisDatasourceAdd(kind: kind, label: label, datasetID: datasetID)
            await MainActor.run {
                self.busyAction = nil
                guard let added else {
                    self.errorText = "Add failed — invalid config or unknown provider."
                    return
                }
                self.adding = false
                self.selectedID = added.id
                self.status = "Added “\(added.label)”"
                self.reload()
            }
        }
    }

    private func performSave(_ id: String) {
        let label = editLabel.trimmingCharacters(in: .whitespacesAndNewlines)
        let datasetID = editDatasetID.trimmingCharacters(in: .whitespacesAndNewlines)
        let enabled = enabledDraft
        let core = self.core
        busyAction = "save"
        errorText = nil
        Task.detached(priority: .userInitiated) {
            let updated = core.gisDatasourceUpdate(id: id, label: label,
                                                   datasetID: datasetID, enabled: enabled)
            await MainActor.run {
                self.busyAction = nil
                guard let updated else {
                    self.errorText = "Save failed — check that the config is valid JSON."
                    return
                }
                self.status = "Saved “\(updated.label)”"
                self.reload()
            }
        }
    }


    private func performDiscover(_ id: String) {
        let core = self.core
        busyAction = "discover"
        errorText = nil
        Task.detached(priority: .userInitiated) {
            let info = core.gisDatasourceDiscover(id: id)
            await MainActor.run {
                self.busyAction = nil
                guard let info else {
                    self.errorText = "Discover failed — is the dataset id valid?"
                    return
                }
                self.discoverInfo = info
                self.status = "Discovered “\(info.name)” · \(info.featureCount) features"
                self.reload()
            }
        }
    }

    private func performFetch(_ id: String) {
        let core = self.core
        busyAction = "fetch"
        errorText = nil
        status = "Importing…"
        Task.detached(priority: .userInitiated) {
            let report = core.gisDatasourceFetch(id: id)
            await MainActor.run {
                self.busyAction = nil
                guard let report else {
                    self.errorText = "Import failed — see the Rust core log."
                    return
                }
                self.lastImport = report
                self.discoverInfo = nil
                self.status = "Imported \(report.featureCount) \(report.geometryType) features as “\(report.displayName)”"
                self.onLayersChanged?(report.bounds)
                self.reload()
            }
        }
    }

    private func performDelete(_ source: GisDataSource) {
        let core = self.core
        let produced = layers.first { $0.name == source.label }
        busyAction = "delete"
        errorText = nil
        Task.detached(priority: .userInitiated) {
            if let produced {
                _ = core.gisDeleteLayer(id: produced.id)
            }
            let ok = core.gisDatasourceDelete(id: source.id)
            await MainActor.run {
                self.busyAction = nil
                guard ok else {
                    self.errorText = "Delete failed."
                    return
                }
                self.selectedID = nil
                self.status = "Deleted “\(source.label)”"
                self.onLayersChanged?(nil)
                self.reload()
            }
        }
    }
}

// MARK: Display helpers (mirror ContentView's class color/label maps)

private func displayKind(_ raw: String) -> String {
    switch raw {
    case "data-gov-sg": return "data.gov.sg"
    default: return raw
    }
}

private func shortLabel(_ key: String) -> String {
    let tail = key.split(separator: "/").last.map(String.init) ?? key
    return tail.replacingOccurrences(of: "_", with: " ")
}

private func classColor(_ key: String) -> Color {
    switch key {
    case "Layers/Expressway": return Color(.sRGB, red: 0x25 / 255.0, green: 0x63 / 255.0, blue: 0xeb / 255.0)
    case "Layers/Expressway_Sliproad": return Color(.sRGB, red: 0xf4 / 255.0, green: 0xa2 / 255.0, blue: 0x61 / 255.0)
    case "Layers/Major_Road": return Color(.sRGB, red: 0xe6 / 255.0, green: 0x39 / 255.0, blue: 0x46 / 255.0)
    case "Layers/Contour_250K": return Color(.sRGB, red: 0xb3 / 255.0, green: 0x9b / 255.0, blue: 0x7d / 255.0)
    case "Layers/International_bdy": return Color(.sRGB, red: 0x26 / 255.0, green: 0x46 / 255.0, blue: 0x53 / 255.0)
    case "Layers/Hydrographic": return Color(.sRGB, red: 0x4a / 255.0, green: 0xa3 / 255.0, blue: 0xdf / 255.0)
    case "Layers/Coastal_Outlines": return Color(.sRGB, red: 0xb3 / 255.0, green: 0xa9 / 255.0, blue: 0x8c / 255.0)
    case "Layers/Parks_NaturalReserve": return Color(.sRGB, red: 0x7b / 255.0, green: 0xbf / 255.0, blue: 0x6a / 255.0)
    case "Layers/Airport_Runway": return Color(.sRGB, red: 0x9a / 255.0, green: 0xa0 / 255.0, blue: 0xa6 / 255.0)
    case "Layers/Central_Business_District": return Color(.sRGB, red: 0xf2 / 255.0, green: 0xc9 / 255.0, blue: 0x4c / 255.0)
    default: return .gray
    }
}

