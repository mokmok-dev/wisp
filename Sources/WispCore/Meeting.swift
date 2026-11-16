import Foundation

/// A meeting tool Wisp can detect.
///
/// The catalog is platform data: each native tool lists the macOS bundle
/// identifiers that hold the microphone while a call runs. Browser meetings
/// share one entry because the microphone alone cannot say which site a
/// browser tab is on.
public enum MeetingTool: String, CaseIterable, Hashable, Sendable {
  case zoom
  case microsoftTeams = "microsoft-teams"
  case slack
  case webex
  case faceTime = "face-time"
  case discord
  case browserMeeting = "browser-meeting"

  /// Stable key used in `settings.json` and by the detector. This is the
  /// persisted identity: renaming it resets every user's choice for the
  /// tool to the default.
  public var key: String {
    rawValue
  }

  /// Human-readable name shown in the prompt and settings.
  public var label: String {
    switch self {
    case .zoom: "Zoom"
    case .microsoftTeams: "Microsoft Teams"
    case .slack: "Slack huddles"
    case .webex: "Webex"
    case .faceTime: "FaceTime"
    case .discord: "Discord"
    case .browserMeeting: "Browser meetings"
    }
  }

  /// macOS bundle identifiers that use the microphone during a call.
  public var bundleIDs: [String] {
    switch self {
    case .zoom: ["us.zoom.xos"]
    case .microsoftTeams: ["com.microsoft.teams2", "com.microsoft.teams"]
    case .slack: ["com.tinyspeck.slackmacgap"]
    case .webex: ["com.cisco.webexmeetingsapp"]
    case .faceTime: ["com.apple.FaceTime"]
    case .discord: ["com.hnc.Discord"]
    case .browserMeeting:
      [
        "com.google.Chrome",
        "com.apple.Safari",
        "com.apple.SafariTechnologyPreview",
        "com.microsoft.edgemac",
        "org.mozilla.firefox",
        "com.brave.Browser",
        "company.thebrowser.Browser",
        "com.vivaldi.Vivaldi",
      ]
    }
  }

  /// Whether this entry covers any browser rather than one native app.
  public var isBrowser: Bool {
    self == .browserMeeting
  }

  /// How long the microphone must stay held before this tool counts as a
  /// call. A known app is trusted quickly; a browser also hosts voice
  /// notes and dictation fields, which release the microphone after a few
  /// seconds, so it needs to hold it for a while.
  public var detectionDelay: TimeInterval {
    isBrowser ? 20 : 2
  }

  /// The tool whose bundle list contains `bundleID`.
  ///
  /// Helper processes carry a suffix (`us.zoom.xos.helper`), and they are
  /// the processes that usually open the microphone for a native app, so a
  /// dotted extension of a listed identifier names the same tool.
  public static func fromBundleID(_ bundleID: String) -> Self? {
    allCases.first { tool in
      tool.bundleIDs.contains { known in
        bundleID == known || bundleID.hasPrefix(known + ".")
      }
    }
  }
}

/// What Wisp does when it detects a call from a tool.
public enum MeetingPolicy: String, CaseIterable, Hashable, Sendable, Codable {
  /// Ignore calls from this tool.
  case off
  /// Offer to record when a call starts.
  case ask
  /// Start recording as soon as a call starts.
  case autoRecord = "auto-record"

  /// Stable key used in `settings.json`.
  public var key: String {
    rawValue
  }

  /// Human-readable name shown in settings.
  public var label: String {
    switch self {
    case .off: "Off"
    case .ask: "Ask"
    case .autoRecord: "Auto-record"
    }
  }

  /// The default for any tool the user has not configured.
  public static let `default` = MeetingPolicy.ask

  /// Decode an unknown or absent policy as the default, matching how the
  /// settings file tolerates values written by a newer build.
  public init(from decoder: Decoder) throws {
    let container = try decoder.singleValueContainer()
    let key = try container.decode(String.self)
    self = MeetingPolicy(rawValue: key) ?? .default
  }
}

/// One policy per tool. Tools left out follow `.ask`, and keys this build
/// does not know are dropped so a newer settings file keeps the user's other
/// preferences.
public struct MeetingPolicies: Hashable, Sendable, Codable {
  private var storage: [MeetingTool: MeetingPolicy]

  public init() {
    storage = [:]
  }

  /// The policy for `tool`, or the default when it has none.
  public func policy(for tool: MeetingTool) -> MeetingPolicy {
    storage[tool] ?? .default
  }

  /// Replace one tool's policy. `.ask` is the default, so it leaves no
  /// entry behind.
  public mutating func set(_ tool: MeetingTool, to policy: MeetingPolicy) {
    if policy == .default {
      storage[tool] = nil
    } else {
      storage[tool] = policy
    }
  }

  /// Number of tools that differ from the default.
  public var count: Int {
    storage.count
  }

  /// Whether every tool follows the default.
  public var isEmpty: Bool {
    storage.isEmpty
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.singleValueContainer()
    guard !container.decodeNil() else {
      storage = [:]
      return
    }
    let raw = try container.decode([String: MeetingPolicy].self)
    var policies: [MeetingTool: MeetingPolicy] = [:]
    for (key, policy) in raw where policy != .default {
      if let tool = MeetingTool(rawValue: key) {
        policies[tool] = policy
      }
    }
    storage = policies
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.singleValueContainer()
    // A dictionary keyed by tool, so the document does not depend on the
    // declaration order of `MeetingTool`. `SettingsStore`'s `.sortedKeys`
    // supplies the stable order on disk.
    var raw: [String: MeetingPolicy] = [:]
    for (tool, policy) in storage {
      raw[tool.key] = policy
    }
    try container.encode(raw)
  }
}

/// Persisted meeting-detection preferences.
public struct MeetingDetectionSettings: Hashable, Sendable, Codable {
  /// Whether Wisp watches which apps hold the microphone.
  public var enabled: Bool
  /// The user's choice for each tool.
  public var policies: MeetingPolicies

  public init(enabled: Bool = false, policies: MeetingPolicies = MeetingPolicies()) {
    self.enabled = enabled
    self.policies = policies
  }

  /// The policy for `tool`, or the default when it has none.
  public func policy(for tool: MeetingTool) -> MeetingPolicy {
    policies.policy(for: tool)
  }

  /// Replace one tool's policy. Returns whether anything changed.
  @discardableResult
  public mutating func setPolicy(_ policy: MeetingPolicy, for tool: MeetingTool) -> Bool {
    guard policies.policy(for: tool) != policy else { return false }
    policies.set(tool, to: policy)
    return true
  }

  private enum CodingKeys: String, CodingKey {
    case enabled
    case policies
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    enabled = try container.decodeIfPresent(Bool.self, forKey: .enabled) ?? false
    policies =
      try container.decodeIfPresent(MeetingPolicies.self, forKey: .policies)
      ?? MeetingPolicies()
  }
}
