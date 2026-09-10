import Foundation

/// Result of NL → gis/query translation.
struct NLQueryPlan {
    let params: [String: Any]
    let summary: String
    /// True when the rule tables matched a concrete layer/class/name intent.
    /// When false, callers should fall back to semantic search.
    let hadIntent: Bool
}

/// First-pass, rule-based translator: free text → `gis/query` DSL params.
/// Uses the live layer catalog (names + classes) and the current map viewport /
/// centre as the default region — no geocoder or LLM key required. A later LLM
/// step can emit the same DSL.
enum NLQueryEngine {

    private static let classAliases: [(phrases: [String], key: String)] = [
        (["expressway", "expressways"], "Layers/Expressway"),
        (["sliproad", "sliproads", "expressway sliproad", "expressway sliproads"], "Layers/Expressway_Sliproad"),
        (["major road", "major roads", "main road", "main roads"], "Layers/Major_Road"),
        (["contour", "contours"], "Layers/Contour_250K"),
        (["boundary", "international boundary"], "Layers/International_bdy"),
        (["water", "hydrographic", "reservoir", "river", "rivers"], "Layers/Hydrographic"),
        (["coast", "coastal", "coastline", "shoreline outline"], "Layers/Coastal_Outlines"),
        (["green reserve", "green reserves", "parks reserve", "parks reserves"], "Layers/Parks_NaturalReserve"),
        (["airport", "runway"], "Layers/Airport_Runway"),
        (["central business district", "central business", "cbd"], "Layers/Central_Business_District"),
    ]

    private static let layerAliases: [(phrases: [String], name: String)] = [
        (["heritage trees", "heritage tree"], "heritage-trees"),
        (["park connector", "park connectors", "connector loop", "connectors"], "park-connector-loop"),
        (["nparks tracks", "nparks track", "tracks"], "nparks-tracks"),
        (["community in bloom", "community bloom", "bloom"], "community-in-bloom"),
        (["natureways", "natureway"], "natureways"),
        (["shoreline typology"], "shoreline-typology"),
        (["tree conservation"], "tree-conservation-area"),
        (["heritage road green buffers", "heritage road"], "heritage-road-green-buffers"),
        (["parks", "park"], "parks"),
        (["nature reserves", "nature reserve"], "nparks-nature-reserves"),
    ]

    static func translate(_ text: String,
                          layers: [GisLayer],
                          viewport: [String: Double]) -> NLQueryPlan {
        let lower = text.lowercased()
        let normalized = String(
            lower.unicodeScalars.map { CharacterSet.alphanumerics.contains($0) || $0 == " " ? Character($0) : " " }
        )
        let padded = " \(normalized.split(separator: " ").joined(separator: " ")) "

        let presentLayers = Set(layers.map { $0.name })
        var presentClasses = Set<String>()
        for l in layers { presentClasses.formUnion(l.classes.map { $0.key }) }

        var matchedLayers = Set<String>()
        for a in layerAliases where a.phrases.contains(where: { padded.contains(" \($0) ") }) {
            if presentLayers.contains(a.name) { matchedLayers.insert(a.name) }
        }
        var matchedClasses = Set<String>()
        for a in classAliases where a.phrases.contains(where: { padded.contains(" \($0) ") }) {
            if presentClasses.contains(a.key) { matchedClasses.insert(a.key) }
        }

        var attributes: [[String: Any]] = []
        for marker in ["named ", "called "] {
            if let range = lower.range(of: marker) {
                let name = lower[range.upperBound...].trimmingCharacters(in: .whitespaces)
                if !name.isEmpty {
                    attributes.append(["key": "NAME", "op": "contains", "value": name])
                }
                break
            }
        }

        let distance = parsedDistance(lower) ?? 1000.0
        let hasViewport = (viewport["maxLng"] ?? 0) > (viewport["minLng"] ?? 0)
        let bbox: [Double] = hasViewport
            ? [viewport["minLng"]!, viewport["minLat"]!, viewport["maxLng"]!, viewport["maxLat"]!]
            : [103.6, 1.15, 104.1, 1.5]
        let center: [Double] = hasViewport
            ? [(viewport["minLng"]! + viewport["maxLng"]!) / 2,
               (viewport["minLat"]! + viewport["maxLat"]!) / 2]
            : [103.85, 1.35]

        var predicate = "bbox"
        var region: [String: Any] = ["bbox": bbox]
        if padded.contains("nearest") || padded.contains("closest") {
            predicate = "nearest"
            region = ["center": center, "k": 20]
        } else if [" near ", " around ", " within ", " close to ", " next to ", " nearby "]
            .contains(where: { padded.contains($0) }) {
            predicate = "radius"
            region = ["center": center, "radius_m": distance]
        }

        let what = describe(classes: matchedClasses, layers: matchedLayers, attributes: attributes, fallback: "features")
        let wherePhrase: String
        if predicate == "nearest" {
            wherePhrase = String(format: "nearest to the map centre (%.3f, %.3f)", center[0], center[1])
        } else if predicate == "radius" {
            wherePhrase = String(format: "within %.0f m of the map centre", distance)
        } else {
            wherePhrase = hasViewport ? "in the current view" : "across Singapore"
        }
        let summary = "\(what) \(wherePhrase)"

        var params: [String: Any] = [
            "predicate": predicate,
            "region": region,
            "limit": 200,
            "output": "features",
        ]
        if !matchedLayers.isEmpty { params["layers"] = Array(matchedLayers).sorted() }
        if !matchedClasses.isEmpty { params["classes"] = Array(matchedClasses).sorted() }
        if !attributes.isEmpty { params["attributes"] = attributes }

        let hadIntent = !matchedLayers.isEmpty || !matchedClasses.isEmpty || !attributes.isEmpty
        return NLQueryPlan(params: params, summary: summary, hadIntent: hadIntent)
    }

    private static func parsedDistance(_ s: String) -> Double? {
        let patterns: [(String, Double)] = [
            (##"(\d+(?:\.\d+)?)\s*km"##, 1000),
            (##"(\d+(?:\.\d+)?)\s*m(?:et(?:er|re)s?)?"##, 1),
        ]
        for (pat, mult) in patterns {
            guard let rx = try? NSRegularExpression(pattern: pat),
                  let m = rx.firstMatch(in: s, range: NSRange(s.startIndex..., in: s)),
                  let r = Range(m.range(at: 1), in: s),
                  let v = Double(s[r]) else { continue }
            return v * mult
        }
        return nil
    }

    private static func describe(classes: Set<String>,
                                 layers: Set<String>,
                                 attributes: [[String: Any]],
                                 fallback: String) -> String {
        var parts: [String] = []
        for c in classes.sorted() {
            let tail = c.split(separator: "/").last.map(String.init) ?? c
            parts.append(tail.replacingOccurrences(of: "_", with: " "))
        }
        for l in layers.sorted() {
            parts.append(l.replacingOccurrences(of: "-", with: " "))
        }
        for a in attributes {
            if let v = a["value"] as? String, !v.isEmpty {
                parts.append("named \"\(v)\"")
            }
        }
        return parts.isEmpty ? fallback : parts.joined(separator: ", ")
    }
}
