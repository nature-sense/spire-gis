import Foundation
import Observation

/// Reply envelope from the Rust core: `{"ok":bool, "result":…, "error":…}`.
private struct GisEnvelope: Codable {
    let ok: Bool
    let result: GisJSON?
    let error: String?
}

/// Minimal JSON passthrough so the envelope (and datasource configs / schema
/// summaries) can hold arbitrary JSON values.
enum GisJSON: Codable, Hashable {
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([GisJSON])
    case object([String: GisJSON])
    case null

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null; return }
        if let b = try? c.decode(Bool.self) { self = .bool(b); return }
        if let n = try? c.decode(Double.self) { self = .number(n); return }
        if let s = try? c.decode(String.self) { self = .string(s); return }
        if let a = try? c.decode([GisJSON].self) { self = .array(a); return }
        if let o = try? c.decode([String: GisJSON].self) { self = .object(o); return }
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

/// One classification (FOLDERPATH) within a layer.
struct GisClassCount: Codable, Hashable {
    let key: String
    let count: Int
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
    let classes: [GisClassCount]
    /// Stacking order (higher = drawn on top).
    let zOrder: Int

    enum CodingKeys: String, CodingKey {
        case id, name, description, source, bounds, classes
        case displayName = "display_name"
        case geometryType = "geometry_type"
        case featureCount = "feature_count"
        case zOrder = "z_order"
    }
}

// MARK: - Data sources

/// Refresh policy tag (`{"mode": "manual"}` today; interval refresh is a
/// future scheduler feature).
struct GisRefreshPolicy: Codable, Hashable {
    let mode: String
}

/// Cached discovery summary carried on a definition (`discovered`).
struct GisDiscovery: Codable, Hashable {
    let featureCount: Int
    let geometryTypes: [String]
    let schema: GisJSON

    enum CodingKeys: String, CodingKey {
        case schema
        case featureCount = "feature_count"
        case geometryTypes = "geometry_types"
    }
}

/// Full discovery result from `gis/datasource/discover`.
struct GisDatasetInfo: Codable, Hashable {
    let name: String
    let description: String
    let geometryTypes: [String]
    let featureCount: Int
    let schema: GisJSON

    enum CodingKeys: String, CodingKey {
        case name, description, schema
        case featureCount = "feature_count"
        case geometryTypes = "geometry_types"
    }
}

/// One data-source definition (`gis/datasource/list`).
struct GisDataSource: Codable, Identifiable, Hashable {
    let id: String
    let kind: String
    let label: String
    let config: GisJSON?
    let refresh: GisRefreshPolicy?
    let enabled: Bool
    let discovered: GisDiscovery?
    let createdAt: String
    let updatedAt: String

    enum CodingKeys: String, CodingKey {
        case id, kind, label, config, refresh, enabled, discovered
        case createdAt = "created_at"
        case updatedAt = "updated_at"
    }
}

/// Full attribute map for one stored feature (`gis/get-feature`).
struct GisFeatureDetail: Codable, Hashable {
    let id: String
    let layer: String
    let name: String
    let attributes: [String: GisJSON]
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

    /// `gis/import-datagov` → import a data.gov.sg GeoJSON dataset as a layer.
    func gisImportDataGovSg(datasetId: String, name: String, displayName: String) -> GisImportReport? {
        sendTyped(["method": "gis/import-datagov",
                   "params": ["dataset_id": datasetId, "name": name, "display_name": displayName]],
                  as: GisImportReport.self)
    }

    /// `gis/get-tile` → base64 MVT bytes for a layer at `z/x/y`.
    func gisGetTile(layer: String, z: Int, x: Int, y: Int) -> String? {
        struct GisTile: Codable { let tile: String; let bytes: Int }
        let params: [String: Any] = ["layer": layer, "z": z, "x": x, "y": y]
        let resp = sendTyped(["method": "gis/get-tile", "params": params], as: GisTile.self)
        return resp?.tile
    }

    /// `gis/query` → structured spatial-query DSL (JSON params). Returns the
    /// raw result JSON string (FeatureCollection + total/by_class), or nil.
    func gisQuery(params: [String: Any]) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "method": "gis/query", "params": params,
        ]), let request = String(data: data, encoding: .utf8),
        let raw = send(request),
        let reply = raw.data(using: .utf8),
        let json = try? JSONSerialization.jsonObject(with: reply) as? [String: Any],
        let ok = json["ok"] as? Bool, ok,
        let result = json["result"],
        let resultData = try? JSONSerialization.data(withJSONObject: result)
        else { return nil }
        return String(data: resultData, encoding: .utf8)
    }

    /// `gis/nl-query` → LLM-translated natural-language query. Returns the raw
    /// result JSON string (gis/query-shaped, plus `summary`/`source`), or nil
    /// when the LLM path failed (caller should fall back).
    func gisNlQuery(text: String, viewport: [String: Double]) -> String? {
        let params: [String: Any] = ["text": text, "viewport": viewport]
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "method": "gis/nl-query", "params": params,
        ]), let request = String(data: data, encoding: .utf8),
        let raw = send(request),
        let reply = raw.data(using: .utf8),
        let json = try? JSONSerialization.jsonObject(with: reply) as? [String: Any],
        let ok = json["ok"] as? Bool, ok,
        let result = json["result"],
        let resultData = try? JSONSerialization.data(withJSONObject: result)
        else { return nil }
        return String(data: resultData, encoding: .utf8)
    }

    /// `gis/semantic-search` → SeleneDB vector search over embedded nodes.
    /// `scope` is "Feature" (GeoJSON FeatureCollection) or "Layer"
    /// (ranked layer list). Returns the raw result JSON string, or nil.
    func gisSemanticSearch(text: String, scope: String = "Feature", limit: Int = 50,
                           layer: String? = nil) -> String? {
        var params: [String: Any] = ["text": text, "node_type": scope, "limit": limit]
        if let layer { params["layer"] = layer }
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "method": "gis/semantic-search", "params": params,
        ]), let request = String(data: data, encoding: .utf8),
        let raw = send(request),
        let reply = raw.data(using: .utf8),
        let json = try? JSONSerialization.jsonObject(with: reply) as? [String: Any],
        let ok = json["ok"] as? Bool, ok,
        let result = json["result"],
        let resultData = try? JSONSerialization.data(withJSONObject: result)
        else { return nil }
        return String(data: resultData, encoding: .utf8)
    }

    /// `gis/spatial-query` → the GeoJSON FeatureCollection (compact JSON string
    /// ready to hand to the map), or nil on error.
    func gisSpatialQuery(minLng: Double, minLat: Double, maxLng: Double, maxLat: Double,
                         layer: String?, limit: Int = 2000) -> String? {
        var params: [String: Any] = [
            "min_lng": minLng, "min_lat": minLat,
            "max_lng": maxLng, "max_lat": maxLat, "limit": limit,
        ]
        if let layer { params["layer"] = layer }
        guard let data = try? JSONSerialization.data(withJSONObject: ["method": "gis/spatial-query", "params": params]),
              let request = String(data: data, encoding: .utf8),
              let raw = send(request),
              let reply = raw.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: reply) as? [String: Any],
              let ok = json["ok"] as? Bool, ok,
              let result = json["result"],
              let resultData = try? JSONSerialization.data(withJSONObject: result)
        else { return nil }
        return String(data: resultData, encoding: .utf8)
    }

    /// `gis/get-layer-geojson` → the whole layer as a compact GeoJSON
    /// FeatureCollection string, or nil on error. `simplify` (degrees) decimates
    /// display geometry server-side — used for very large layers so the main
    /// thread never has to parse a huge payload.
    func gisGetLayerGeoJson(layer: String, limit: Int = 20_000,
                            simplify: Double? = nil, dropProps: Bool = false) -> String? {
        var params: [String: Any] = ["layer": layer, "limit": limit]
        if let simplify { params["simplify"] = simplify }
        if dropProps { params["drop_props"] = true }
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "method": "gis/get-layer-geojson",
            "params": params,
        ]), let request = String(data: data, encoding: .utf8),
        let raw = send(request),
        let reply = raw.data(using: .utf8),
        let json = try? JSONSerialization.jsonObject(with: reply) as? [String: Any],
        let ok = json["ok"] as? Bool, ok,
        let result = json["result"],
        let resultData = try? JSONSerialization.data(withJSONObject: result)
        else { return nil }
        return String(data: resultData, encoding: .utf8)
    }

    /// `gis/delete-layer` → remove a layer + its features.
    @discardableResult
    func gisDeleteLayer(id: String) -> Bool {
        struct Resp: Codable { let deleted: Bool }
        let resp = sendTyped(["method": "gis/delete-layer", "params": ["id": id]], as: Resp.self)
        return resp?.deleted == true
    }

    /// `gis/get-feature` → the full attribute map for one stored feature.
    func gisGetFeature(id: String) -> GisFeatureDetail? {
        sendTyped(["method": "gis/get-feature", "params": ["id": id]], as: GisFeatureDetail.self)
    }

    /// `gis/reorder-layer` → move a layer one step ("up"/"down") and get back
    /// the freshly sorted catalog (bottom-to-top).
    func gisReorderLayer(id: String, direction: String) -> [GisLayer] {
        sendTyped(["method": "gis/reorder-layer",
                   "params": ["id": id, "direction": direction]],
                  as: [GisLayer].self) ?? []
    }

    // MARK: Data-source RPCs

    /// `gis/datasource/kinds` → registered driver kinds, e.g. `["data-gov-sg"]`.
    func gisDatasourceKinds() -> [String] {
        decodeResult(#"{"method":"gis/datasource/kinds","params":{}}"#, as: [String].self) ?? []
    }

    /// `gis/datasource/list` → all persisted data-source definitions.
    func gisDatasourceList() -> [GisDataSource] {
        decodeResult(#"{"method":"gis/datasource/list","params":{}}"#, as: [GisDataSource].self) ?? []
    }

    /// `gis/datasource/get` → one definition by id.
    func gisDatasourceGet(id: String) -> GisDataSource? {
        sendTyped(["method": "gis/datasource/get", "params": ["id": id]], as: GisDataSource.self)
    }

    /// `gis/datasource/add` → create a data.gov.sg definition from a dataset id.
    /// (The provider's whole config is `{"dataset_id": …}`.)
    func gisDatasourceAdd(kind: String, label: String, datasetID: String) -> GisDataSource? {
        sendTyped(["method": "gis/datasource/add",
                   "params": ["kind": kind, "label": label,
                              "config": ["dataset_id": datasetID]]],
                  as: GisDataSource.self)
    }

    /// `gis/datasource/update` → patch label / dataset id / enabled.
    func gisDatasourceUpdate(id: String, label: String? = nil,
                             datasetID: String? = nil, enabled: Bool? = nil) -> GisDataSource? {
        var params: [String: Any] = ["id": id]
        if let label { params["label"] = label }
        if let datasetID { params["config"] = ["dataset_id": datasetID] }
        if let enabled { params["enabled"] = enabled }
        return sendTyped(["method": "gis/datasource/update", "params": params], as: GisDataSource.self)
    }

    /// `gis/datasource/delete` → remove a definition.
    @discardableResult
    func gisDatasourceDelete(id: String) -> Bool {
        struct Resp: Codable { let ok: Bool }
        let resp = sendTyped(["method": "gis/datasource/delete", "params": ["id": id]], as: Resp.self)
        return resp?.ok == true
    }

    /// `gis/datasource/discover` → metadata + attribute schema (no import).
    func gisDatasourceDiscover(id: String) -> GisDatasetInfo? {
        sendTyped(["method": "gis/datasource/discover", "params": ["id": id]], as: GisDatasetInfo.self)
    }

    /// `gis/datasource/fetch` → driver fetch + shared import pipeline.
    func gisDatasourceFetch(id: String) -> GisImportReport? {
        sendTyped(["method": "gis/datasource/fetch", "params": ["id": id]], as: GisImportReport.self)
    }

    // MARK: Config helpers

    /// The data.gov.sg dataset id stored in a definition's config
    /// (`{"dataset_id": "d_…"}`), or "".
    static func datasetID(from config: GisJSON?) -> String {
        guard case .object(let object)? = config,
              case .string(let value)? = object["dataset_id"]
        else { return "" }
        return value
    }

    /// Render a scalar JSON value as display text (numbers drop a trailing
    /// `.0`, booleans read naturally).
    static func scalarText(_ value: GisJSON?) -> String {
        switch value {
        case .string(let s): return s
        case .number(let n):
            if n.isFinite, n == n.rounded(), abs(n) < 1e15 {
                return String(Int64(n))
            }
            return String(n)
        case .bool(let b): return b ? "true" : "false"
        case .object(let o): return "{\(o.count) keys}"
        case .array(let a): return "[\(a.count) items]"
        case .null, .none: return ""
        }
    }

    deinit {
        if let h = handle { dlclose(h) }
    }
}

