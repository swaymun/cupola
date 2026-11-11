// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WonderComputerUse",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "WonderComputerUseCore", targets: ["WonderComputerUseCore"]),
        .executable(name: "WonderComputerUse", targets: ["WonderComputerUse"]),
    ],
    dependencies: [
        .package(url: "https://github.com/stasel/WebRTC.git", exact: "153.0.0"),
    ],
    targets: [
        .target(name: "WonderSimulatorCapture", linkerSettings: [.linkedFramework("IOSurface"), .linkedFramework("QuartzCore")]),
        .target(
            name: "WonderComputerUseCore",
            dependencies: ["WonderSimulatorCapture"],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
            ]
        ),
        .executableTarget(
            name: "WonderComputerUse",
            dependencies: [
                "WonderComputerUseCore",
                .product(name: "WebRTC", package: "WebRTC"),
            ]
        ),
        .testTarget(
            name: "WonderComputerUseCoreTests",
            dependencies: ["WonderComputerUseCore"]
        ),
        .testTarget(
            name: "WonderComputerUseWebRTCTests",
            dependencies: [
                "WonderComputerUse",
                .product(name: "WebRTC", package: "WebRTC"),
            ]
        ),
    ]
)
