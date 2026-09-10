import Foundation

/// App-level diagnostic log written to `~/.spire/gis-data/ui.log`.
/// Status/query/import messages from the map and the Rust core are appended
/// here instead of being shown on screen.
enum AppLog {
    private static let lock = NSLock()

    private static var url: URL = {
        let dir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".spire/gis-data", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("ui.log")
    }()

    /// Append one timestamped line.
    static func write(_ message: String) {
        lock.lock()
        defer { lock.unlock() }
        let line = "[\(Date().formatted(.iso8601))] \(message)\n"
        guard let data = line.data(using: .utf8) else { return }
        if let handle = try? FileHandle(forWritingTo: url) {
            defer { try? handle.close() }
            handle.seekToEndOfFile()
            try? handle.write(contentsOf: data)
        } else {
            try? data.write(to: url)
        }
    }
}
