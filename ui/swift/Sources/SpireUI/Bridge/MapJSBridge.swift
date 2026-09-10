import Foundation
import Observation
import WebKit

/// Bridges the map page (WKWebView) and the Rust core:
///
/// - JS → Swift (`spireBridge` messages): `ready`, `tile` (MapLibre requests
///   a `spire://tiles/...` vector tile → `gis/get-tile` → base64 → JS), and
///   `bounds` (view moved → `gis/spatial-query` → highlight).
/// - Swift → JS: `window.spireSetLayers`, `spireSetVisibility`,
///   `spireHighlightResults`/`spireClearResults`, `spireReportBounds`,
///   `map.zoomIn/zoomOut`.
/// Info about a feature the user clicked in the map.
struct MapSelectionInfo {
    let id: String
    let layer: String
    let name: String
    let objectid: String
    let cls: String
    let lat: Double?
    let lng: Double?
    /// Features stacked under the cursor at click time + which one was picked
    /// (repeated clicks at the same spot cycle through the stack).
    let stacked: Int
    let pickIndex: Int
    /// True for a point pick (snapped to a geometry vertex). Street View is
    /// hidden for point picks; `vertexIndex` is -1 for a free click.
    let isPoint: Bool
    let vertexIndex: Int
    /// MapLibre style-layer type of the picked object: "circle" (Point),
    /// "line" or "fill". Street View only makes sense for circles/points.
    let geometryKind: String
}

@Observable
final class MapJSBridge: NSObject, WKScriptMessageHandler {
    let core: CoreBridge
    weak var webView: WKWebView?

    private(set) var ready = false
    /// Current map viewport (updated on every moveend).
    private(set) var viewport: [String: Double] = [:]
    /// Number of vector tiles served from the graph (0 now that layers render
    /// as GeoJSON — see `loadNote` for the real load state).
    private(set) var tilesServed = 0
    /// Short status shown in the sidebar caption while layers load.
    private(set) var loadNote = "ready"
    var onReady: (() -> Void)?
    var onStatus: ((String) -> Void)?
    var onSelect: ((MapSelectionInfo?) -> Void)?
    /// Layer (by name) narrowed for viewport queries; nil = all layers.
    var queryLayer: String?

    init(core: CoreBridge) {
        self.core = core
        super.init()
    }

    // MARK: WKScriptMessageHandler

    func userContentController(_ userContentController: WKUserContentController,
                               didReceive message: WKScriptMessage) {
        guard message.name == "spireBridge",
              let body = message.body as? [String: Any] else { return }
        switch body["kind"] as? String ?? "" {
        case "ready":
            ready = true
            onReady?()
        case "log":
            if let text = body["text"] as? String { onStatus?(text) }
        case "tile":
            handleTile(body)
        case "bounds":
            handleBounds(body)
        case "select":
            handleSelect(body)
        case "viewport":
            viewport = ["minLng": body["minLng"] as? Double ?? 0,
                        "minLat": body["minLat"] as? Double ?? 0,
                        "maxLng": body["maxLng"] as? Double ?? 0,
                        "maxLat": body["maxLat"] as? Double ?? 0]
        default:
            break
        }
    }

    private func handleTile(_ body: [String: Any]) {
        guard let url = body["url"] as? String,
              let layer = body["layer"] as? String,
              let z = body["z"] as? Int,
              let x = body["x"] as? Int,
              let y = body["y"] as? Int else { return }
        tilesServed += 1
        let core = self.core
        // Tile build + MVT encoding can take tens of ms on a large layer; do it
        // off the main thread so panning the map never stalls the UI.
        Task.detached(priority: .userInitiated) { [weak self] in
            let b64 = core.gisGetTile(layer: layer, z: z, x: x, y: y) ?? ""
            await MainActor.run {
                self?.evaluate("window.spireResolveTile(\(Self.jsString(url)),\(Self.jsString(b64)))")
            }
        }
    }

    private func handleSelect(_ body: [String: Any]) {
        if (body["empty"] as? Bool) == true {
            onSelect?(nil)
            return
        }
        onSelect?(MapSelectionInfo(id: body["id"] as? String ?? "",
                                   layer: body["layer"] as? String ?? "",
                                   name: body["name"] as? String ?? "",
                                   objectid: body["objectid"] as? String ?? "",
                                   cls: body["class"] as? String ?? "",
                                   lat: body["lat"] as? Double,
                                   lng: body["lng"] as? Double,
                                   stacked: body["stacked"] as? Int ?? 1,
                                   pickIndex: body["pick"] as? Int ?? 0,
                                   isPoint: body["point"] as? Bool ?? false,
                                   vertexIndex: body["vertex"] as? Int ?? -1,
                                   geometryKind: body["geom"] as? String ?? ""))
    }

    private func handleBounds(_ body: [String: Any]) {
        guard let minLng = body["minLng"] as? Double,
              let minLat = body["minLat"] as? Double,
              let maxLng = body["maxLng"] as? Double,
              let maxLat = body["maxLat"] as? Double else { return }
        let fc = core.gisSpatialQuery(minLng: minLng, minLat: minLat,
                                      maxLng: maxLng, maxLat: maxLat,
                                      layer: queryLayer)
        if let fc {
            evaluate("window.spireHighlightResults(\(Self.jsString(fc)))", context: "bounds")
        } else {
            clearResults()
        }
    }

    // MARK: Swift → JS

    func evaluate(_ js: String, context: String = "") {
        Task { @MainActor in
            webView?.evaluateJavaScript(js) { [weak self] _, error in
                guard let error, let self else { return }
                // WKWebView puts the real JS exception text in userInfo.
                let nsError = error as NSError
                let jsMessage = nsError.userInfo["WKJavaScriptExceptionMessage"] as? String
                let detail = jsMessage ?? nsError.localizedDescription
                let tag = context.isEmpty ? "js" : context
                self.onStatus?("[\(tag)] \(detail)")
            }
        }
    }

    /// Push the current layer catalog to the map (adds vector-tile sources + layers).
    func syncLayers(_ layers: [GisLayer]) {
        let items: [[String: Any]] = layers.map { l in
            ["name": l.name,
             "geometry_type": l.geometryType,
             "classes": l.classes.map { ["key": $0.key, "count": $0.count] }]
        }
        guard let data = try? JSONSerialization.data(withJSONObject: items),
              let json = String(data: data, encoding: .utf8) else { return }
        evaluate("window.spireSetLayers(\(Self.jsString(json)))", context: "syncLayers")
    }

    /// Load each layer's features from the graph (background) and render it.
    /// Layers load sequentially (one FFI call at a time); very large layers ask
    /// the core for display-decimated, property-free geometry so the payload
    /// handed to the webview stays small. Progress is posted to `loadNote`.
    func refreshLayerData(_ layers: [GisLayer]) {
        guard !layers.isEmpty else { return }
        Task.detached(priority: .userInitiated) { [weak self, core] in
            await MainActor.run { self?.loadNote = "loading \(layers.count) layers…" }
            var done = 0
            for layer in layers {
                let big = layer.featureCount > 1_000
                let simplify = big ? 0.0006 : nil
                // Always use the minimal `{class}` display payload so fill/line
                // styling by FOLDERPATH is consistent across every layer.
                let dropProps = true
                let json = core.gisGetLayerGeoJson(layer: layer.name,
                                                   simplify: simplify,
                                                   dropProps: dropProps)
                guard let json, !json.isEmpty else {
                    await MainActor.run { [weak self] in
                        self?.loadNote = "\(layer.name): failed"
                        self?.onStatus?("\(layer.name): get-layer-geojson returned nothing")
                    }
                    continue
                }
                let kb = json.utf8.count / 1024
                await MainActor.run { [weak self] in
                    guard let self else { return }
                    self.loadNote = "loading \(layer.name)…"
                    self.evaluate("window.spireSetLayerDataB64(\(Self.jsString(layer.name)),\(Self.jsString(Data(json.utf8).base64EncodedString())))", context: "setData:\(layer.name)")
                    self.onStatus?("\(layer.name): sent \(json.utf8.count) chars (~\(kb) KB)")
                }
                done += 1
            }
            await MainActor.run { [weak self] in
                self?.loadNote = "\(done)/\(layers.count) layers"
                self?.onStatus?("layer data ready: \(done)/\(layers.count)")
            }
        }
    }

    /// Clear the map-side highlighted selection.
    func clearSelection() {
        evaluate("window.spireClearSelection && window.spireClearSelection()")
    }

    /// Switch click behaviour: `true` = point mode (snap to nearest vertex,
    /// no Street View); `false` = whole-object selection.
    func setSelectionMode(pointMode: Bool) {
        evaluate("window.spireSetSelectionMode && window.spireSetSelectionMode(\(Self.jsString(pointMode ? "point" : "object")))")
    }

    /// Restrict clicks to one layer (or one sublayer) chosen in the sidebar.
    /// `classKey == nil` means the whole layer; empty `layerName` clears it.
    func setActiveLayer(layerName: String, classKey: String?) {
        let cls = classKey.map { Self.jsString($0) } ?? "null"
        evaluate("window.spireSetActiveLayer && window.spireSetActiveLayer(\(Self.jsString(layerName)),\(cls))")
    }

    /// Show a query-result FeatureCollection by adding temporary companion
    /// layers (same colours as the base layers, matched feature id filters).
    func showQueryResults(_ fcJson: String) {
        evaluate("window.spireHighlightResults(\(Self.jsString(fcJson)))", context: "query")
    }

    func setVisibility(layerName: String, visible: Bool) {
        evaluate("window.spireSetVisibility(\(Self.jsString(layerName)),\(visible))")
    }

    /// Toggle one classification (FOLDERPATH) within a layer.
    func setClassVisibility(layerName: String, classKey: String, visible: Bool) {
        evaluate("window.spireSetClassVisibility(\(Self.jsString(layerName)),\(Self.jsString(classKey)),\(visible))")
    }

    /// Apply a new bottom-to-top stacking order for the data layers (the order
    /// of `gis/list-layers`). Temporary selection/query overlays are re-raised
    /// above everything afterwards.
    func orderLayers(_ names: [String]) {
        guard let data = try? JSONSerialization.data(withJSONObject: names),
              let json = String(data: data, encoding: .utf8) else { return }
        evaluate("window.spireOrderLayers(\(Self.jsString(json)))", context: "orderLayers")
    }

    func clearResults() {
        evaluate("window.spireClearResults && window.spireClearResults()")
    }

    /// Zoom the map to a `[minLng, minLat, maxLng, maxLat]` bounds.
    func fitBounds(_ bounds: [Double]) {
        guard bounds.count == 4 else { return }
        evaluate("window.spireFitBounds && window.spireFitBounds([\(bounds[0]),\(bounds[1]),\(bounds[2]),\(bounds[3])])")
    }

    /// Ask the map for its current viewport; the reply triggers a spatial query.
    func queryViewport() {
        evaluate("window.spireReportBounds()")
    }

    func zoomIn() { evaluate("window.map && window.map.zoomIn()") }
    func zoomOut() { evaluate("window.map && window.map.zoomOut()") }

    /// Escape a Swift string as a double-quoted JS string literal.
    /// (NSJSONSerialization cannot serialize a top-level scalar, so no JSON path.)
    private static func jsString(_ s: String) -> String {
        var out = "\""
        for c in s.unicodeScalars {
            switch c {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            default:
                if c.value < 0x20 {
                    out += String(format: "\\u%04x", c.value)
                } else {
                    out.unicodeScalars.append(c)
                }
            }
        }
        out += "\""
        return out
    }


}
