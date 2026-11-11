// swift-tools-version: 6.0
import PackageDescription

// Deliberately independent of the app, transport packages and simulator runner.
let package = Package(
    name: "WonderPresentation",
    platforms: [.macOS(.v14)],
    targets: [
        .target(name: "WonderPresentation", path: "Presentation"),
        .target(name: "WonderNativeReplyUI", path: "Wonder/NativeReplyUI"),
        .testTarget(name: "WonderPresentationSnapshots", dependencies: ["WonderPresentation", "WonderNativeReplyUI"],
                    path: "SnapshotTests", resources: [.copy("References"), .copy("Fixtures")]),
    ]
)
