// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "SpireUI",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .executable(name: "SpireUI", targets: ["SpireUI"]),
    ],
    targets: [
        .executableTarget(name: "SpireUI", path: "Sources/SpireUI"),
    ]
)
