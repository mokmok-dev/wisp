import Foundation
import WispCore
import WispStorage

/// Glue between `Storage` and `AppModel`: the helpers that list the library,
/// create a session row at recording start, commit the finished transcript,
/// load a history transcript, and remove a session's recordings.
enum Library {
  /// Default session title: `2026-05-29 14:30` in the user's local timezone.
  static func defaultTitle(startedAt: Date) -> String {
    Format.startedAt(startedAt)
  }

  /// Per-session directory name under `recordings/`.
  ///
  /// This name is part of the on-disk layout: it is the `mic_wav_path` /
  /// `system_wav_path` prefix in `sessions.db` and the name of the directory
  /// holding the Ogg/Opus files. Changing the shape orphans existing
  /// recordings. The random suffix keeps rapid or concurrent starts apart.
  static func sessionDirectoryName(
    startedAt: Date = Date(),
    id: UUID = UUID()
  ) -> String {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.calendar = Calendar(identifier: .gregorian)
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "yyyy-MM-dd'T'HHmmss'Z'"
    return "\(formatter.string(from: startedAt))-\(id.uuidString.lowercased())"
  }

  /// Ogg paths for one recording, relative to the storage root.
  static func relativeOggPaths(dirName: String) -> (mic: String, system: String) {
    (
      mic: "recordings/\(dirName)/mic.ogg",
      system: "recordings/\(dirName)/system.ogg"
    )
  }

  /// Create a session row for a recording that is about to start.
  static func createSession(
    _ storage: Storage,
    startedAt: Date,
    dirName: String,
    title: String
  ) throws -> SessionID {
    let paths = relativeOggPaths(dirName: dirName)
    let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
    return try storage.sessions.create(
      NewSession(
        startedAt: startedAt,
        title: trimmed.isEmpty ? defaultTitle(startedAt: startedAt) : trimmed,
        micWavPath: paths.mic,
        systemWavPath: paths.system
      )
    )
  }

  /// Commit the finished transcript and stamp `ended_at` in one transaction.
  ///
  /// Per-source `segment_index` follows the order the rows are walked, so the
  /// history reads back in the same order the live view showed.
  static func finaliseSession(
    _ storage: Storage,
    sessionID: SessionID,
    segments: [LiveSegment],
    endedAt: Date
  ) throws {
    var stored: [NewSegment] = []
    stored.reserveCapacity(segments.count)
    var micIndex: UInt32 = 0
    var systemIndex: UInt32 = 0
    for segment in segments {
      let index: UInt32
      switch segment.source {
      case .mic:
        index = micIndex
        micIndex = micIndex == .max ? .max : micIndex + 1
      case .system:
        index = systemIndex
        systemIndex = systemIndex == .max ? .max : systemIndex + 1
      }
      // SpeechAnalyzer emits zero-length results during silence;
      // persisting them only clutters the history.
      guard !segment.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
        continue
      }
      stored.append(
        NewSegment(
          sessionID: sessionID,
          source: segment.source,
          segmentIndex: index,
          startSeconds: segment.startSeconds,
          endSeconds: segment.endSeconds,
          text: segment.text,
          speakerLabel: nil
        )
      )
    }
    try storage.finaliseSession(sessionID, segments: stored, endedAt: endedAt)
  }

  /// Load a saved transcript as finalised live segments.
  static func loadHistory(_ storage: Storage, sessionID: SessionID) throws -> [LiveSegment] {
    try storage.segments.listBySession(sessionID).map { segment in
      LiveSegment(
        source: segment.source,
        id: UInt64(segment.segmentIndex),
        text: segment.text,
        startSeconds: segment.startSeconds,
        endSeconds: segment.endSeconds,
        isFinal: true
      )
    }
  }

  /// Remove one session's recording directory.
  ///
  /// The path comes from the database, so it cannot be assumed to still point
  /// inside `recordingsRoot`. Both tracks must name the same directory, and
  /// that directory must canonicalise to a strict descendant of the root. A
  /// missing directory returns `false` because the row can still go.
  static func removeSessionRecordings(
    storageRoot: URL,
    recordingsRoot: URL,
    session: WispCore.Session
  ) throws -> Bool {
    let relativeDirectory = (session.micWavPath as NSString).deletingLastPathComponent
    guard !relativeDirectory.isEmpty,
      relativeDirectory == (session.systemWavPath as NSString).deletingLastPathComponent,
      let canonicalRoot = canonicalPath(recordingsRoot),
      let canonicalDirectory = canonicalPath(
        storageRoot.appendingPathComponent(relativeDirectory, isDirectory: true)
      ),
      canonicalDirectory != canonicalRoot,
      canonicalDirectory.hasPrefix(canonicalRoot + "/")
    else {
      return false
    }
    try FileManager.default.removeItem(atPath: canonicalDirectory)
    return true
  }

  /// Absolute, symlink-resolved path, or `nil` when the entry is absent.
  private static func canonicalPath(_ url: URL) -> String? {
    let path = url.standardizedFileURL.resolvingSymlinksInPath().path
    return FileManager.default.fileExists(atPath: path) ? path : nil
  }
}
