// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WispAudioKit",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "wispctl", targets: ["wispctl"]),
    ],
    targets: [
        .executableTarget(
            name: "wispctl",
            path: "Sources/wispctl"
        ),
    ]
)
