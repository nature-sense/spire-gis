import SwiftUI
import AppKit

/// spire-gis — minimal launchable SwiftUI shell. The fill phase replaces
/// this with the real application UI (chat/RAG panels, project views, …).
@main
struct SpireApp: App {
    @State private var core = CoreBridge()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(core)
                .frame(minWidth: 800, minHeight: 500)
        }
        .windowStyle(.titleBar)
    }
}
