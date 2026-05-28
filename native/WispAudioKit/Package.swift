// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WispAudioKit",
    platforms: [.macOS("26.0")],
    products: [
        .library(name: "WispAudioKit", targets: ["WispAudioKit"]),
        .executable(name: "wispctl", targets: ["wispctl"]),
    ],
    targets: [
        .target(
            name: "WispAudioKit",
            path: "Sources/WispAudioKit"
        ),
        .executableTarget(
            name: "wispctl",
            dependencies: ["WispAudioKit"],
            path: "Sources/wispctl"
        ),
    ]
)
