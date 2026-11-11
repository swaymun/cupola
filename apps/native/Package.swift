// swift-tools-version: 6.0
import PackageDescription
let package = Package(
    name: "WonderPairing",
    platforms: [.macOS(.v14), .iOS("26.0")],
    products: [
        .library(name: "WonderPairing", targets: ["WonderPairing"]),
        .library(name: "WonderComputerView", targets: ["WonderComputerView"]),
    ],
    dependencies: [
        .package(url: "https://github.com/stasel/WebRTC.git", exact: "153.0.0"),
    ],
    targets: [
        .target(name: "WonderPairing", resources: [.copy("Resources/Highlight")]),
        // The app compiles these sources itself instead of linking this
        // target; the target exists for the tests.
        .target(name: "WonderVoice", dependencies: ["WonderPairing"]),
        .target(
            name: "WonderComputerView",
            dependencies: [
                "WonderPairing",
                .product(name: "WebRTC", package: "WebRTC"),
            ]
        ),
        .testTarget(name: "WonderPairingTests", dependencies: ["WonderPairing"]),
        .testTarget(name: "WonderVoiceTests", dependencies: ["WonderVoice", "WonderPairing"]),
        .testTarget(
            name: "WonderComputerViewTests",
            dependencies: [
                "WonderComputerView",
                .product(name: "WebRTC", package: "WebRTC"),
            ]
        ),
    ]
)
