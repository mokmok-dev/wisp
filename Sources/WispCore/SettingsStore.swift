import Foundation

/// Failure to resolve the on-disk root Wisp keeps its library in.
public enum WispPathsError: Error, CustomStringConvertible, Equatable {
  /// Neither `WISP_DATA_DIR` nor `HOME` was usable. An ephemeral temp
  /// directory is not a safe persistence fallback.
  case homeUnavailable

  public var description: String {
    switch self {
    case .homeUnavailable:
      "HOME is unavailable; set WISP_DATA_DIR to durable storage"
    }
  }
}

/// The directories Wisp reads and writes.
public enum WispPaths {
  /// `$WISP_DATA_DIR`, else `$HOME/Library/Application Support/dev.mokmok.wisp`.
  public static func dataDirectory(
    environment: [String: String] = ProcessInfo.processInfo.environment
  ) throws -> URL {
    if let override = environment["WISP_DATA_DIR"], !override.isEmpty {
      return URL(fileURLWithPath: override, isDirectory: true)
    }
    guard let home = environment["HOME"], !home.isEmpty else {
      throw WispPathsError.homeUnavailable
    }
    return URL(fileURLWithPath: home, isDirectory: true)
      .appendingPathComponent("Library", isDirectory: true)
      .appendingPathComponent("Application Support", isDirectory: true)
      .appendingPathComponent("dev.mokmok.wisp", isDirectory: true)
  }

  /// Root for every session's Ogg/Opus files.
  public static func recordingsDirectory(in dataDirectory: URL) -> URL {
    dataDirectory.appendingPathComponent("recordings", isDirectory: true)
  }

  /// `sessions.db` inside the data directory.
  public static func databaseFile(in dataDirectory: URL) -> URL {
    dataDirectory.appendingPathComponent("sessions.db", isDirectory: false)
  }
}

/// Reads and writes `settings.json` in the Wisp data directory.
public struct SettingsStore: Sendable {
  /// Name of the settings file inside the Wisp data directory.
  static let fileName = "settings.json"

  public let directory: URL

  public init(directory: URL) {
    self.directory = directory
  }

  public var fileURL: URL {
    directory.appendingPathComponent(Self.fileName, isDirectory: false)
  }

  /// Read `settings.json`, falling back to defaults when the file is missing
  /// or unreadable.
  public func load() -> Settings {
    guard let data = try? Data(contentsOf: fileURL),
      let settings = try? JSONDecoder().decode(Settings.self, from: data)
    else {
      return Settings()
    }
    return settings
  }

  /// Write the settings back. `Data.write(options: .atomic)` stages a
  /// temporary file and renames it into place, so a crash mid-write keeps
  /// the previous preferences.
  public func save(_ settings: Settings) throws {
    try FileManager.default.createDirectory(
      at: directory,
      withIntermediateDirectories: true
    )
    let encoder = JSONEncoder()
    // `.sortedKeys` because `MeetingPolicies` encodes as a dictionary, whose
    // iteration order is not stable; without it every save is a diff.
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    var data = try encoder.encode(settings)
    data.append(0x0A)
    try data.write(to: fileURL, options: .atomic)
  }
}
