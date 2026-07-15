// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WispAudioKit",
    platforms: [.macOS("26.0")],
    products: [
        // Static library so the Rust `wisp-audiokit-sys` crate can statically
        // link the resulting .a into the desktop binary.
        .library(name: "WispAudioKit", type: .static, targets: ["WispAudioKit"]),
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
        .testTarget(
            name: "WispAudioKitTests",
            dependencies: ["WispAudioKit"],
            path: "Tests/WispAudioKitTests"
        ),
    ]
)
