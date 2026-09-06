import Foundation
import Observation
import WebKit

/// Bridges the map page (WKWebView) and the Rust core:
///
/// - JS → Swift (`spireBridge` messages): `ready`, `tile` (MapLibre requests
///   a `spire://tiles/...` vector tile → `gis/get-tile` → base64 → JS), and
///   `bounds` (view moved → `gis/spatial-query` → overlay).
/// - Swift → JS: `window.spireSetLayers`, `spireSetVisibility`,
///   `spireSetResults`, `spireReportBounds`, `map.zoomIn/zoomOut`.
@Observable
final class MapJSBridge: NSObject, WKScriptMessageHandler {
    let core: CoreBridge
    weak var webView: WKWebView?

    private(set) var ready = false
    /// Number of vector tiles served from the graph (proof the map is live).
    private(set) var tilesServed = 0
    var onReady: (() -> Void)?
    var onStatus: ((String) -> Void)?
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
        let b64 = core.gisGetTile(layer: layer, z: z, x: x, y: y) ?? ""
        evaluate("window.spireResolveTile(\(Self.jsString(url)),\(Self.jsString(b64)))")
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
            evaluate("window.spireSetResults(\(Self.jsString(fc)))")
        } else {
            clearResults()
        }
    }

    // MARK: Swift → JS

    func evaluate(_ js: String) {
        Task { @MainActor in
            webView?.evaluateJavaScript(js, completionHandler: nil)
        }
    }

    /// Push the current layer catalog to the map (adds GeoJSON sources/layers).
    func syncLayers(_ layers: [GisLayer]) {
        let items: [[String: Any]] = layers.map { l in
            ["name": l.name, "geometry_type": l.geometryType]
        }
        guard let data = try? JSONSerialization.data(withJSONObject: items),
              let json = String(data: data, encoding: .utf8) else { return }
        evaluate("window.spireSetLayers(\(Self.jsString(json)))")
    }

    /// Load each layer's features from the graph (background) and render it.
    func refreshLayerData(_ layers: [GisLayer]) {
        for layer in layers {
            Task.detached(priority: .userInitiated) { [core] in
                let json = core.gisGetLayerGeoJson(layer: layer.name)
                if let json {
                    await MainActor.run { [weak self] in
                        self?.evaluate("window.spireSetLayerData(\(Self.jsString(layer.name)),\(Self.jsString(json)))")
                    }
                }
            }
        }
    }

    func setVisibility(layerName: String, visible: Bool) {
        evaluate("window.spireSetVisibility(\(Self.jsString(layerName)),\(visible))")
    }

    func clearResults() {
        evaluate("window.spireSetResults(null)")
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
