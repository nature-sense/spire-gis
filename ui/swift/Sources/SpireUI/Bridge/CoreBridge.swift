import Foundation
import Observation

/// Reply envelope from the Rust core: `{"ok":bool, "result":…, "error":…}`.
private struct GisEnvelope: Codable {
    let ok: Bool
    let result: JSONValue?
    let error: String?
}

/// Minimal JSON passthrough so the envelope can hold an arbitrary result.
private enum JSONValue: Codable {
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])
    case null

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null; return }
        if let b = try? c.decode(Bool.self) { self = .bool(b); return }
        if let n = try? c.decode(Double.self) { self = .number(n); return }
        if let s = try? c.decode(String.self) { self = .string(s); return }
        if let a = try? c.decode([JSONValue].self) { self = .array(a); return }
        if let o = try? c.decode([String: JSONValue].self) { self = .object(o); return }
        throw DecodingError.dataCorrupted(.init(codingPath: c.codingPath, debugDescription: "unknown json"))
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        case .null: try c.encodeNil()
        }
    }
}

/// The `gis/status` result.
struct GisStatus: Codable {
    let core: String
    let version: String
}

/// Report from a successful import (`gis/import-geojson-file`).
struct GisImportReport: Codable {
    let layerId: String
    let name: String
    let displayName: String
    let geometryType: String
    let featureCount: Int
    let source: String
    let bounds: [Double]?

    enum CodingKeys: String, CodingKey {
        case name, source, bounds
        case layerId = "layer_id"
        case displayName = "display_name"
        case geometryType = "geometry_type"
        case featureCount = "feature_count"
    }
}

/// One entry from `gis/list-layers`.
struct GisLayer: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    let displayName: String
    let description: String
    let geometryType: String
    let source: String
    let featureCount: Int
    let bounds: [Double]?

    enum CodingKeys: String, CodingKey {
        case id, name, description, source, bounds
        case displayName = "display_name"
        case geometryType = "geometry_type"
        case featureCount = "feature_count"
    }
}

/// Loads the Rust core (`libspire_gis.dylib`) and calls it over the JSON FFI.
/// The dylib lives in Contents/Frameworks when bundled, or in
/// <repo-root>/target/release during development — same convention as
/// spire-code's SpireFFI.
@Observable
final class CoreBridge {
    private(set) var statusText = "Rust core: not loaded"
    private var handle: UnsafeMutableRawPointer?

    init() { load() }

    private func load() {
        var candidates: [String] = []
        candidates.append(
            Bundle.main.bundleURL
                .appendingPathComponent("Contents")
                .appendingPathComponent("Frameworks")
                .appendingPathComponent("libspire_gis.dylib")
                .path
        )
        candidates.append(
            URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent()  // Bridge/
                .deletingLastPathComponent()  // SpireUI/
                .deletingLastPathComponent()  // Sources/
                .deletingLastPathComponent()  // swift/
                .deletingLastPathComponent()  // ui/
                .deletingLastPathComponent()  // <repo root>/
                .appendingPathComponent("target")
                .appendingPathComponent("release")
                .appendingPathComponent("libspire_gis.dylib")
                .path
        )
        for p in candidates {
            if let h = dlopen(p, RTLD_NOW | RTLD_LOCAL) {
                handle = h
                statusText = "Rust core: loaded"
                return
            }
        }
    }

    /// Send a JSON request to the Rust core and return the raw JSON reply string.
    func send(_ request: String) -> String? {
        guard let h = handle,
              let sendSym = dlsym(h, "spire_send_json"),
              let freeSym = dlsym(h, "spire_free_string")
        else { return nil }
        let sendFn = unsafeBitCast(
            sendSym,
            to: (@convention(c) (UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>?).self
        )
        let freeFn = unsafeBitCast(
            freeSym,
            to: (@convention(c) (UnsafeMutablePointer<CChar>?) -> Void).self
        )
        let response = request.withCString { sendFn($0) }
        defer { freeFn(response) }
        return response.map { String(cString: $0) }
    }

    /// Decode the `result` half of an `{"ok":…}` envelope as `T`.
    private func decodeResult<T: Decodable>(_ request: String, as _: T.Type) -> T? {
        guard let raw = send(request),
              let data = raw.data(using: .utf8),
              let env = try? JSONDecoder().decode(GisEnvelope.self, from: data),
              env.ok,
              let result = env.result
        else { return nil }
        let encoder = JSONEncoder()
        guard let resultData = try? encoder.encode(result) else { return nil }
        return try? JSONDecoder().decode(T.self, from: resultData)
    }

    /// Send a method with a dictionary of params and decode the result as `T`.
    private func sendTyped<T: Decodable>(_ body: [String: Any], as _: T.Type) -> T? {
        guard let data = try? JSONSerialization.data(withJSONObject: body),
              let request = String(data: data, encoding: .utf8)
        else { return nil }
        return decodeResult(request, as: T.self)
    }

    /// `gis/status` → core id + version.
    func gisStatus() -> GisStatus? {
        decodeResult(#"{"method":"gis/status","params":{}}"#, as: GisStatus.self)
    }

    /// `gis/list-layers` → the imported layers (empty until an import exists).
    func gisListLayers() -> [GisLayer] {
        decodeResult(#"{"method":"gis/list-layers","params":{}}"#, as: [GisLayer].self) ?? []
    }

    /// `gis/import-geojson-file` → import a GeoJSON file as a new/replaced layer.
    func gisImportGeoJsonFile(path: String, name: String? = nil, displayName: String? = nil) -> GisImportReport? {
        var params: [String: Any] = ["path": path]
        if let name { params["name"] = name }
        if let displayName { params["display_name"] = displayName }
        return sendTyped(["method": "gis/import-geojson-file", "params": params], as: GisImportReport.self)
    }

    /// `gis/delete-layer` → remove a layer + its features.
    @discardableResult
    func gisDeleteLayer(id: String) -> Bool {
        struct Resp: Codable { let deleted: Bool }
        let resp = sendTyped(["method": "gis/delete-layer", "params": ["id": id]], as: Resp.self)
        return resp?.deleted == true
    }

    deinit {
        if let h = handle { dlclose(h) }
    }
}

