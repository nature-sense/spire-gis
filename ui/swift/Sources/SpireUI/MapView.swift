import SwiftUI
import WebKit

/// A WKWebView hosting the MapLibre map page, wired to a `MapJSBridge`.
struct MapView: NSViewRepresentable {
    let controller: MapJSBridge

    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.userContentController = WKUserContentController()
        config.userContentController.add(controller, name: "spireBridge")
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        let web = WKWebView(frame: .zero, configuration: config)
        web.setValue(false, forKey: "drawsBackground")
        controller.webView = web
        web.loadHTMLString(spireMapHtml, baseURL: nil)
        return web
    }

    func updateNSView(_ nsView: WKWebView, context: Context) {}
}
