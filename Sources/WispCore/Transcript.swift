import Foundation

/// One recording session = one meeting. Tracks lifecycle timestamps, the
/// user-editable title, and the on-disk paths to the captured Ogg/Opus files.
public struct Session: Hashable, Sendable {
  public var id: SessionID
  public var startedAt: Date
  /// `nil` while the session is still recording.
  public var endedAt: Date?
  public var title: String
  /// Path relative to the storage root, never absolute. Lets the user
  /// move their library between machines without DB rewrites.
  public var micWavPath: String
  public var systemWavPath: String
  public var notes: String

  public init(
    id: SessionID,
    startedAt: Date,
    endedAt: Date?,
    title: String,
    micWavPath: String,
    systemWavPath: String,
    notes: String
  ) {
    self.id = id
    self.startedAt = startedAt
    self.endedAt = endedAt
    self.title = title
    self.micWavPath = micWavPath
    self.systemWavPath = systemWavPath
    self.notes = notes
  }
}

/// Fields required to create a new session. The storage layer fills in
/// `id`, `endedAt`, and `notes`.
public struct NewSession: Hashable, Sendable {
  public var startedAt: Date
  public var title: String
  public var micWavPath: String
  public var systemWavPath: String

  public init(
    startedAt: Date,
    title: String,
    micWavPath: String,
    systemWavPath: String
  ) {
    self.startedAt = startedAt
    self.title = title
    self.micWavPath = micWavPath
    self.systemWavPath = systemWavPath
  }
}

/// One finalized transcript chunk inside a session.
///
/// Each `SpeechAnalyzer` finalization (mic or system) appends one segment.
/// Partial / volatile updates are not persisted — only the final text is.
public struct Segment: Hashable, Sendable {
  public var id: SegmentID
  public var sessionID: SessionID
  public var source: SourceLabel
  /// 0-based index within (session, source). Stable across reads, lets
  /// the UI re-order or re-render without timestamp comparisons.
  public var segmentIndex: UInt32
  public var startSeconds: Double
  public var endSeconds: Double
  public var text: String
  /// User-editable speaker name. Defaults to the source label string
  /// (`"mic"` / `"system"`) on insert and can be renamed later.
  public var speakerLabel: String?
  public var createdAt: Date

  public init(
    id: SegmentID,
    sessionID: SessionID,
    source: SourceLabel,
    segmentIndex: UInt32,
    startSeconds: Double,
    endSeconds: Double,
    text: String,
    speakerLabel: String?,
    createdAt: Date
  ) {
    self.id = id
    self.sessionID = sessionID
    self.source = source
    self.segmentIndex = segmentIndex
    self.startSeconds = startSeconds
    self.endSeconds = endSeconds
    self.text = text
    self.speakerLabel = speakerLabel
    self.createdAt = createdAt
  }
}

/// Fields required to append a new segment. The storage layer fills in
/// `id` and `createdAt`.
public struct NewSegment: Hashable, Sendable {
  public var sessionID: SessionID
  public var source: SourceLabel
  public var segmentIndex: UInt32
  public var startSeconds: Double
  public var endSeconds: Double
  public var text: String
  public var speakerLabel: String?

  public init(
    sessionID: SessionID,
    source: SourceLabel,
    segmentIndex: UInt32,
    startSeconds: Double,
    endSeconds: Double,
    text: String,
    speakerLabel: String?
  ) {
    self.sessionID = sessionID
    self.source = source
    self.segmentIndex = segmentIndex
    self.startSeconds = startSeconds
    self.endSeconds = endSeconds
    self.text = text
    self.speakerLabel = speakerLabel
  }
}
