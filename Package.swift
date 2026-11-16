// swift-tools-version: 6.0
import PackageDescription

let package = Package(
  name: "Wisp",
  platforms: [.macOS("26.0")],
  targets: [
    .target(name: "WispCore"),
    .target(
      name: "WispLifecycle",
      dependencies: ["WispCore"]
    ),
    .target(
      name: "WispStorage",
      dependencies: ["WispCore"],
      linkerSettings: [.linkedLibrary("sqlite3")]
    ),
    .target(
      name: "WispAudioKit",
      dependencies: ["WispCore"]
    ),
    .executableTarget(
      name: "WispDesktop",
      dependencies: ["WispAudioKit", "WispCore", "WispLifecycle", "WispStorage"]
    ),
    .executableTarget(
      name: "wispctl",
      dependencies: ["WispAudioKit"]
    ),
    .testTarget(
      name: "WispAudioKitTests",
      dependencies: ["WispAudioKit"]
    ),
    .testTarget(
      name: "WispCoreTests",
      dependencies: ["WispCore"]
    ),
    .testTarget(
      name: "WispLifecycleTests",
      dependencies: ["WispLifecycle"]
    ),
    .testTarget(
      name: "WispStorageTests",
      dependencies: ["WispStorage", "WispCore"]
    ),
    .testTarget(
      name: "WispDesktopTests",
      dependencies: ["WispDesktop", "WispCore", "WispStorage"]
    ),
  ]
)
