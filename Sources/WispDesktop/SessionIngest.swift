import Foundation
import WispAudioKit
import WispCore
import WispStorage

/// Applies `SessionRunner` updates to `AppModel` and persists at the session
/// boundaries.
///
/// Every write goes through `Storage` on the main actor, so the model and the
/// database the UI reads can never disagree within one update.
@MainActor
final class SessionIngest {
  /// Name of the durable transcript snapshot written beside the Ogg files.
  static let recoveryFileName = "transcript-recovery.json"

  let model: AppModel
  private let storage: Storage
  private let recordingsRoot: URL

  init(model: AppModel, storage: Storage, recordingsRoot: URL) {
    self.model = model
    self.storage = storage
    self.recordingsRoot = recordingsRoot
  }

  // MARK: - Updates

  func applyUpdate(_ update: Update) {
    switch update {
    case .started(let start):
      applyStarted(start)
    case .event(let sessionID, let event):
      applyEvent(sessionID, event)
    case .stopped(let sessionID):
      applyStopped(sessionID)
    case .startFailed(let sessionID, let error):
      applyStartFailed(sessionID, error)
    case .runtimeFailed(let sessionID, let error):
      applyRuntimeFailed(sessionID, error)
    case .error(let sessionID, let error):
      applyError(sessionID, error)
    }
  }

  private func applyStarted(_ start: SessionStart) {
    guard let nextPhase = model.workerUpdatePhase(start.sessionID, update: .started) else {
      return
    }
    model.linkedSessionID = start.sessionID
    model.currentSessionStartedAt = start.startedAt
    model.currentSessionDirName = start.dirName
    // A quit request can move Starting → Stopping before the worker reports
    // Started; the queued Stop still owns the transition.
    model.setState(nextPhase)
  }

  private func applyEvent(_ sessionID: SessionID, _ event: WispSession.Result) {
    guard model.acceptsWorkerUpdate(sessionID) else { return }
    model.ingest(event)
  }

  private func applyStopped(_ sessionID: SessionID) {
    guard model.acceptsWorkerUpdate(sessionID) else { return }
    model.finalizeAllSegments()
    model.pendingSessionWrite = .finalise(endedAt: Date())
    _ = retryPendingPersistence()
  }

  private func applyStartFailed(_ sessionID: SessionID, _ error: SessionError) {
    guard model.acceptsWorkerUpdate(sessionID) else { return }
    model.finalizeAllSegments()
    settleFailure(
      pendingWrite: .finalise(endedAt: Date()),
      suffix: "; audio startup also failed: ",
      error: error
    )
  }

  private func applyRuntimeFailed(_ sessionID: SessionID, _ error: SessionError) {
    guard model.acceptsWorkerUpdate(sessionID) else { return }
    model.finalizeAllSegments()
    settleFailure(
      pendingWrite: .finalise(endedAt: Date()),
      suffix: "; audio runtime also failed: ",
      error: error
    )
  }

  private func applyError(_ sessionID: SessionID, _ error: SessionError) {
    guard model.acceptsWorkerUpdate(sessionID) else { return }
    // Construction failed before any capture, so there is no transcript to
    // finalise; only the pre-allocated row must be removed.
    settleFailure(
      pendingWrite: .delete,
      suffix: "; audio construction also failed: ",
      error: error
    )
  }

  private func settleFailure(
    pendingWrite: PendingSessionWrite,
    suffix: String,
    error: SessionError
  ) {
    model.pendingSessionWrite = pendingWrite
    if retryPendingPersistence() {
      model.fail(.audio(error))
    } else if case .persistence(let message) = model.lastError {
      model.lastError = .persistence(message + suffix + error.description)
    }
  }

  // MARK: - Persistence

  /// Retry a transaction that previously rolled back. Returns true once no
  /// database handle remains and a new session may safely start.
  ///
  /// `target` defaults to this ingest's model. Recovery passes the model it
  /// decoded from a snapshot instead, so it reuses this storage and recordings
  /// root without constructing a second ingest.
  @discardableResult
  func retryPendingPersistence(for target: AppModel? = nil) -> Bool {
    let model = target ?? self.model
    guard let pendingWrite = model.pendingSessionWrite else {
      return !model.hasUnsettledSession
    }
    switch pendingWrite {
    case .finalise: return persistFinishedSession(for: model)
    case .delete: return deleteUnstartedSession(for: model)
    }
  }

  private func persistFinishedSession(for model: AppModel) -> Bool {
    guard case .finalise(let endedAt)? = model.pendingSessionWrite else {
      return failPersistence("the session is not awaiting finalisation", for: model)
    }
    let sessionID: SessionID
    do {
      sessionID = try resolveSessionID(for: model)
    } catch {
      return failPersistence("\(error)", for: model)
    }
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    do {
      try Library.finaliseSession(
        storage,
        sessionID: sessionID,
        segments: model.segments,
        endedAt: endedAt
      )
    } catch {
      // The transaction rolled back, so keep the handle: the navigation
      // and start guards stop this transcript from being replaced.
      return failPersistence("\(error)", for: model)
    }
    releaseHandle(for: model)
    model.setState(.idle)
    model.lastError = nil
    refreshLibrary(for: model)
    return true
  }

  /// Resolve a database handle before every retry.
  ///
  /// A sidecar can outlive a rolled-back or externally removed row, and a
  /// transient lookup failure leaves it unclear whether the row exists.
  /// Rechecking makes retry safe in both directions: reuse the matching row,
  /// recreate a missing one, and refuse to write through an id owned by a
  /// different recording.
  private func resolveSessionID(for model: AppModel) throws -> SessionID {
    guard let startedAt = model.currentSessionStartedAt,
      let dirName = model.currentSessionDirName
    else {
      throw ResolutionFailure(message: "session launch metadata is unavailable")
    }
    if let sessionID = model.currentSessionID {
      let existing: WispCore.Session?
      do {
        existing = try storage.sessions.get(sessionID)
      } catch {
        throw ResolutionFailure(
          message: "could not validate the session row: \(error)"
        )
      }
      if let existing {
        let expected = Library.relativeOggPaths(dirName: dirName)
        guard existing.micWavPath == expected.mic,
          existing.systemWavPath == expected.system
        else {
          throw ResolutionFailure(
            message: "the retained session id belongs to a different recording"
          )
        }
        return sessionID
      }
    }
    do {
      return try Library.createSession(
        storage,
        startedAt: startedAt,
        dirName: dirName,
        title: model.liveTitle
      )
    } catch {
      throw ResolutionFailure(message: "could not create the session row: \(error)")
    }
  }

  private func deleteUnstartedSession(for model: AppModel) -> Bool {
    guard let sessionID = model.currentSessionID else {
      return failPersistence("the pending cleanup has no database id", for: model)
    }
    do {
      try storage.sessions.delete(sessionID)
    } catch {
      return failPersistence("could not remove failed session row: \(error)", for: model)
    }
    model.currentSessionID = nil
    model.linkedSessionID = nil
    releaseHandle(for: model)
    model.setState(.idle)
    model.lastError = nil
    refreshLibrary(for: model)
    return true
  }

  /// Drop the retained handle, its sidecar, and the launch metadata.
  private func releaseHandle(for model: AppModel) {
    model.currentSessionID = nil
    model.pendingSessionWrite = nil
    model.currentSessionStartedAt = nil
    model.currentSessionDirName = nil
    if let outputDir = model.currentOutputDir {
      try? FileManager.default.removeItem(
        at: outputDir.appendingPathComponent(Self.recoveryFileName)
      )
    }
    model.currentOutputDir = nil
  }

  @discardableResult
  private func failPersistence(_ error: String, for model: AppModel) -> Bool {
    let message: String
    do {
      let path = try writeRecoverySnapshot(for: model)
      message = "\(error); recovery transcript saved at \(path.path)"
    } catch {
      message = "\(error); recovery transcript could not be saved: \(error)"
    }
    model.fail(.persistence(message))
    return false
  }

  /// Replace the cached library list from storage.
  func refreshLibrary(for target: AppModel? = nil) {
    let model = target ?? self.model
    guard let sessions = try? storage.sessions.list() else { return }
    model.library = sessions
    let query = model.libraryQuery
    if !query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
      searchLibrary(query, for: model)
    }
  }

  /// Run a transcript search and replace the library filter.
  ///
  /// The query is stored verbatim so the field and the filter agree; only the
  /// storage call sees the trimmed form.
  func searchLibrary(_ query: String, for target: AppModel? = nil) {
    let model = target ?? self.model
    let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty else {
      model.libraryQuery = query
      model.libraryHits = nil
      return
    }
    do {
      let segments = try storage.segments.searchText(trimmed, limit: 200)
      var hits: [SessionID] = []
      for segment in segments where !hits.contains(segment.sessionID) {
        hits.append(segment.sessionID)
      }
      model.libraryQuery = query
      model.libraryHits = hits
    } catch {
      model.libraryQuery = query
      model.libraryHits = []
      model.lastError = .persistence("transcript search failed: \(error)")
    }
  }

  /// Load a saved session into the history screen.
  func openHistory(_ sessionID: SessionID) {
    guard let session = try? storage.sessions.get(sessionID) else { return }
    let segments = (try? Library.loadHistory(storage, sessionID: sessionID)) ?? []
    model.showHistory(session, segments: segments)
  }

  /// Delete a saved session and its recording directory.
  ///
  /// The row goes first: a file-removal failure leaves an orphaned directory,
  /// which costs disk but cannot leave a row pointing at missing audio.
  func deleteSession(_ sessionID: SessionID) {
    let session: WispCore.Session
    do {
      guard let found = try storage.sessions.get(sessionID) else {
        refreshLibrary()
        model.pendingDelete = nil
        return
      }
      session = found
    } catch {
      model.pendingDelete = nil
      model.lastError = .persistence(
        "could not read the session before deleting it: \(error)"
      )
      return
    }
    do {
      try storage.sessions.delete(sessionID)
    } catch {
      model.pendingDelete = nil
      model.lastError = .persistence("could not delete the session: \(error)")
      return
    }
    let removalError: Error?
    do {
      _ = try Library.removeSessionRecordings(
        storageRoot: storage.root,
        recordingsRoot: recordingsRoot,
        session: session
      )
      removalError = nil
    } catch {
      removalError = error
    }
    refreshLibrary()
    if model.view == .history(sessionID) {
      model.showLibrary()
    }
    model.pendingDelete = nil
    if let removalError {
      model.lastError = .persistence(
        "session deleted, but its recordings could not be removed: \(removalError)"
      )
    }
  }

  /// Persist a live-session title change onto the open row.
  func updateLiveTitle(_ title: String) {
    model.liveTitle = title
    guard let sessionID = model.currentSessionID ?? model.linkedSessionID else { return }
    let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
    let stored =
      trimmed.isEmpty
      ? model.currentSessionStartedAt.map(Library.defaultTitle(startedAt:)) ?? ""
      : trimmed
    try? storage.sessions.updateTitle(sessionID, title: stored)
  }

  /// Apply an inline rename from the history header. A blank title keeps the
  /// previous value, so clearing the field acts as a cancel.
  func renameSession(_ sessionID: SessionID, title: String) {
    let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
    if !trimmed.isEmpty {
      try? storage.sessions.updateTitle(sessionID, title: trimmed)
      if model.viewedSession?.id == sessionID {
        model.viewedSession?.title = trimmed
      }
    }
    refreshLibrary()
  }

  // MARK: - Recovery

  /// Write the in-memory transcript beside its Ogg files, atomically.
  ///
  /// This is the durable fallback used before quit and whenever the database
  /// transaction rolls back.
  @discardableResult
  func writeRecoverySnapshot(for target: AppModel? = nil) throws -> URL {
    let model = target ?? self.model
    guard let outputDir = model.currentOutputDir else {
      throw RecoveryError.missingOutputDirectory
    }
    guard let pendingWrite = model.pendingSessionWrite else {
      throw RecoveryError.noPendingWrite
    }
    try FileManager.default.createDirectory(
      at: outputDir,
      withIntermediateDirectories: true
    )
    let snapshot = RecoverySnapshot(
      version: 1,
      sessionID: model.currentSessionID,
      startedAt: model.currentSessionStartedAt,
      directoryName: model.currentSessionDirName,
      pendingWrite: RecoveryPendingWrite(pendingWrite),
      segments: model.segments.map(RecoverySegment.init)
    )
    let target = outputDir.appendingPathComponent(Self.recoveryFileName)
    let temporary = outputDir.appendingPathComponent(".\(Self.recoveryFileName).tmp")
    try snapshot.encoded().write(to: temporary)
    // Ordered for crash safety: the bytes, then the rename, then the
    // directory entry itself.
    try syncFile(at: temporary)
    guard rename(temporary.path, target.path) == 0 else {
      throw RecoveryError.syncFailed(target.path)
    }
    try syncFile(at: outputDir)
    return target
  }

  /// Reconcile durable snapshots left by a previous process before a new
  /// recording can start.
  ///
  /// A valid snapshot is retried against `SQLite` and its sidecar removed on
  /// success. The first unrecoverable one is installed into the guarded
  /// Failed state so the Retry Save action stays available, and its sidecar
  /// stays beside the Ogg files.
  @discardableResult
  func recoverPendingSessions() -> Int {
    let entries =
      (try? FileManager.default.contentsOfDirectory(
        at: recordingsRoot,
        includingPropertiesForKeys: [.isDirectoryKey]
      )) ?? []
    let recoveryPaths =
      entries
      .filter { url in
        (try? url.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true
      }
      .map { $0.appendingPathComponent(Self.recoveryFileName) }
      .filter { FileManager.default.fileExists(atPath: $0.path) }
      .sorted { $0.path < $1.path }

    var recoveredCount = 0
    for path in recoveryPaths {
      switch loadRecoveryModel(from: path) {
      case .success(let recovered):
        recovered.finalizeAllSegments()
        if retryPendingPersistence(for: recovered) {
          recoveredCount += 1
          continue
        }
        installRecoveredSession(recovered)
        return recoveredCount
      case .failure(.alreadyPersisted):
        // The transaction committed and only the sidecar unlink was
        // interrupted. Never reinstall an ended row as a live handle.
        do {
          try FileManager.default.removeItem(at: path)
          recoveredCount += 1
        } catch {
          FileHandle.standardError.write(
            Data(
              "wisp: cannot remove already-persisted recovery snapshot \(path.path): \(error)\n"
                .utf8
            )
          )
        }
      case .failure(.invalid(let message)):
        FileHandle.standardError.write(
          Data("wisp: cannot load recovery snapshot \(path.path): \(message)\n".utf8)
        )
      case .failure(.retryable(let message, let recovered)):
        recovered.lastError = .persistence(
          "cannot reconcile pending recovery \(path.path): \(message)"
        )
        installRecoveredSession(recovered)
        return recoveredCount
      }
    }
    return recoveredCount
  }

  /// Keep setup state owned by the running process and replace only the
  /// transcript ownership fields. The Failed state blocks navigation and new
  /// recordings while exposing the ordinary Retry Save action.
  private func installRecoveredSession(_ recovered: AppModel) {
    model.state = .failed
    model.view = .live
    model.segments = recovered.segments
    model.currentSessionID = recovered.currentSessionID
    model.linkedSessionID = recovered.linkedSessionID
    model.pendingSessionWrite = recovered.pendingSessionWrite
    model.currentSessionStartedAt = recovered.currentSessionStartedAt
    model.currentSessionDirName = recovered.currentSessionDirName
    model.currentOutputDir = recovered.currentOutputDir
    model.viewedSession = nil
    model.lastError = recovered.lastError
  }

  private func loadRecoveryModel(from path: URL) -> Result<AppModel, RecoveryLoadError> {
    switch parseRecoverySnapshot(at: path) {
    case .success(let parsed):
      reconcileLiveSession(parsed.model, pendingWrite: parsed.pendingWrite)
    case .failure(let error):
      .failure(error)
    }
  }

  private func parseRecoverySnapshot(
    at path: URL
  ) -> Result<(model: AppModel, pendingWrite: PendingSessionWrite), RecoveryLoadError> {
    let data: Data
    do {
      data = try Data(contentsOf: path)
    } catch {
      return .failure(.invalid("\(error)"))
    }
    let snapshot: RecoverySnapshot
    do {
      snapshot = try RecoverySnapshot.decode(data)
    } catch {
      return .failure(.invalid("\(error)"))
    }
    guard snapshot.version == 1 else {
      return .failure(.invalid("unsupported recovery version \(snapshot.version)"))
    }
    let outputDir = path.deletingLastPathComponent()
    let directoryName = outputDir.lastPathComponent
    guard !directoryName.isEmpty else {
      return .failure(.invalid("recovery path has no parent directory"))
    }
    guard snapshot.directoryName == directoryName else {
      return .failure(
        .invalid("snapshot directory metadata does not match its location")
      )
    }
    guard let startedAt = snapshot.startedAt else {
      return .failure(.invalid("snapshot has no launch timestamp"))
    }
    let pendingWrite = snapshot.pendingWrite.pendingWrite
    var segments: [LiveSegment] = []
    for recovered in snapshot.segments {
      guard recovered.startSeconds.isFinite,
        recovered.endSeconds.isFinite,
        recovered.startSeconds >= 0,
        recovered.endSeconds >= recovered.startSeconds
      else {
        return .failure(.invalid("snapshot contains invalid segment timing"))
      }
      segments.append(
        LiveSegment(
          source: recovered.source,
          id: recovered.id,
          text: recovered.text,
          startSeconds: recovered.startSeconds,
          endSeconds: recovered.endSeconds,
          // A previous process has stopped; no engine can revise this.
          isFinal: true
        )
      )
    }
    let model = AppModel()
    model.state = .failed
    model.view = .live
    model.segments = segments
    model.currentSessionID = snapshot.sessionID
    model.linkedSessionID = snapshot.sessionID
    model.pendingSessionWrite = pendingWrite
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = directoryName
    model.currentOutputDir = outputDir
    model.lastError = .persistence("pending recovery loaded from \(path.path)")
    return .success((model, pendingWrite))
  }

  private func reconcileLiveSession(
    _ model: AppModel,
    pendingWrite: PendingSessionWrite
  ) -> Result<AppModel, RecoveryLoadError> {
    guard let sessionID = model.currentSessionID else {
      return .success(model)
    }
    let session: WispCore.Session?
    do {
      session = try storage.sessions.get(sessionID)
    } catch {
      return .failure(.retryable("\(error)", model))
    }
    guard let session else {
      // A finalisation row may have been lost while the sidecar stayed;
      // launch metadata lets retry recreate it safely.
      guard pendingWrite != .delete else {
        return .failure(.alreadyPersisted)
      }
      model.currentSessionID = nil
      model.linkedSessionID = nil
      return .success(model)
    }
    let dirName = model.currentSessionDirName ?? ""
    let expected = Library.relativeOggPaths(dirName: dirName)
    guard session.micWavPath == expected.mic, session.systemWavPath == expected.system else {
      return .failure(.invalid("snapshot session id points at a different recording"))
    }
    switch pendingWrite {
    case .finalise:
      return session.endedAt == nil
        ? .success(model)
        : .failure(.alreadyPersisted)
    case .delete:
      return session.endedAt == nil
        ? .success(model)
        : .failure(.invalid("cleanup snapshot points at an already-finalised session"))
    }
  }

  /// Flush a file or directory so the write survives a crash. Flushing the
  /// snapshot alone is not enough: the rename needs the directory synced too.
  private func syncFile(at url: URL) throws {
    let descriptor = open(url.path, O_RDONLY)
    guard descriptor >= 0 else { throw RecoveryError.syncFailed(url.path) }
    defer { close(descriptor) }
    guard fsync(descriptor) == 0 else { throw RecoveryError.syncFailed(url.path) }
  }
}

/// A persistence retry that cannot proceed, with the reason to show the user.
private struct ResolutionFailure: Error, CustomStringConvertible {
  let message: String

  var description: String {
    message
  }
}

/// Failure to write or reconcile a recovery snapshot.
enum RecoveryError: Error, Equatable, CustomStringConvertible {
  case missingOutputDirectory
  case noPendingWrite
  case syncFailed(String)

  var description: String {
    switch self {
    case .missingOutputDirectory: "session output directory is unavailable"
    case .noPendingWrite: "session has no pending persistence operation"
    case .syncFailed(let path): "could not flush \(path) to disk"
    }
  }
}

/// One attempt to load a durable snapshot.
enum RecoveryLoadError: Error {
  /// The database already holds the finished row; only cleanup remains.
  case alreadyPersisted
  /// The snapshot cannot be interpreted and is left in place.
  case invalid(String)
  /// Storage could not be consulted, so the transcript stays guarded.
  case retryable(String, AppModel)
}

// MARK: - Snapshot wire format

/// `transcript-recovery.json` beside a session's Ogg files.
struct RecoverySnapshot: Equatable {
  var version: Int
  var sessionID: SessionID?
  var startedAt: Date?
  var directoryName: String?
  var pendingWrite: RecoveryPendingWrite
  var segments: [RecoverySegment]

  func encoded() throws -> Data {
    let encoder = JSONEncoder()
    // `.sortedKeys` keeps the key order stable: `pending_write` is not the only
    // dictionary in the tree, and an unstable order would make every save a
    // diff. The trailing newline matches the text shape this file has always
    // had on disk.
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    var data = try encoder.encode(self)
    data.append(0x0A)
    return data
  }

  static func decode(_ data: Data) throws -> RecoverySnapshot {
    try JSONDecoder().decode(RecoverySnapshot.self, from: data)
  }
}

/// Keys are the on-disk contract; a reader that expects a fixed key set keeps
/// working. Encoding uses `container.encode` rather than `encodeIfPresent` so
/// an absent optional is still written as `null`.
extension RecoverySnapshot: Codable {
  private enum CodingKeys: String, CodingKey {
    case version
    case sessionID = "session_id"
    case startedAt = "started_at"
    case directoryName = "directory_name"
    case pendingWrite = "pending_write"
    case segments
  }

  init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    version = try container.decode(Int.self, forKey: .version)
    sessionID = try container.decodeIfPresent(Int64.self, forKey: .sessionID)
      .map(SessionID.init)
    startedAt = try container.decodeIfPresent(String.self, forKey: .startedAt)
      .flatMap(parseTimestamp)
    directoryName = try container.decodeIfPresent(String.self, forKey: .directoryName)
    pendingWrite = try container.decode(RecoveryPendingWrite.self, forKey: .pendingWrite)
    segments = try container.decode([RecoverySegment].self, forKey: .segments)
  }

  func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(version, forKey: .version)
    try container.encode(sessionID?.rawValue, forKey: .sessionID)
    try container.encode(startedAt.map(TranscriptExport.rfc3339), forKey: .startedAt)
    try container.encode(directoryName, forKey: .directoryName)
    try container.encode(pendingWrite, forKey: .pendingWrite)
    try container.encode(segments, forKey: .segments)
  }
}

enum RecoveryPendingWrite: Equatable {
  case finalise(endedAt: Date)
  case delete

  init(_ pendingWrite: PendingSessionWrite) {
    switch pendingWrite {
    case .finalise(let endedAt): self = .finalise(endedAt: endedAt)
    case .delete: self = .delete
    }
  }

  var pendingWrite: PendingSessionWrite {
    switch self {
    case .finalise(let endedAt): .finalise(endedAt: endedAt)
    case .delete: .delete
    }
  }
}

/// Tagged JSON object: `{"kind":"finalise","ended_at":…}` or
/// `{"kind":"delete"}`.
extension RecoveryPendingWrite: Codable {
  private enum CodingKeys: String, CodingKey {
    case kind
    case endedAt = "ended_at"
  }

  init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    switch try container.decode(String.self, forKey: .kind) {
    case "finalise":
      guard
        let endedAt = try parseTimestamp(
          container.decode(String.self, forKey: .endedAt)
        )
      else {
        throw DecodingError.dataCorruptedError(
          forKey: .endedAt,
          in: container,
          debugDescription: "invalid stop timestamp"
        )
      }
      self = .finalise(endedAt: endedAt)
    case "delete":
      self = .delete
    case let other:
      throw DecodingError.dataCorruptedError(
        forKey: .kind,
        in: container,
        debugDescription: "unrecognized pending write \(other)"
      )
    }
  }

  func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    switch self {
    case .finalise(let endedAt):
      try container.encode("finalise", forKey: .kind)
      try container.encode(TranscriptExport.rfc3339(endedAt), forKey: .endedAt)
    case .delete:
      try container.encode("delete", forKey: .kind)
    }
  }
}

struct RecoverySegment: Equatable {
  var source: SourceLabel
  var id: UInt64
  var text: String
  var startSeconds: Double
  var endSeconds: Double
  var isFinal: Bool

  init(
    source: SourceLabel,
    id: UInt64,
    text: String,
    startSeconds: Double,
    endSeconds: Double,
    isFinal: Bool
  ) {
    self.source = source
    self.id = id
    self.text = text
    self.startSeconds = startSeconds
    self.endSeconds = endSeconds
    self.isFinal = isFinal
  }

  init(_ segment: LiveSegment) {
    self.init(
      source: segment.source,
      id: segment.id,
      text: segment.text,
      startSeconds: segment.startSeconds,
      endSeconds: segment.endSeconds,
      isFinal: segment.isFinal
    )
  }
}

extension RecoverySegment: Codable {
  private enum CodingKeys: String, CodingKey {
    case source
    case id
    case text
    case startSeconds = "start_seconds"
    case endSeconds = "end_seconds"
    case isFinal = "is_final"
  }

  init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let sourceText = try container.decode(String.self, forKey: .source)
    guard let source = SourceLabel(parsing: sourceText) else {
      throw DecodingError.dataCorruptedError(
        forKey: .source,
        in: container,
        debugDescription: "unrecognized source label \(sourceText)"
      )
    }
    self.init(
      source: source,
      id: try container.decode(UInt64.self, forKey: .id),
      text: try container.decode(String.self, forKey: .text),
      startSeconds: try container.decode(Double.self, forKey: .startSeconds),
      endSeconds: try container.decode(Double.self, forKey: .endSeconds),
      isFinal: try container.decode(Bool.self, forKey: .isFinal)
    )
  }

  func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(source.stableString, forKey: .source)
    try container.encode(id, forKey: .id)
    try container.encode(text, forKey: .text)
    try container.encode(startSeconds, forKey: .startSeconds)
    try container.encode(endSeconds, forKey: .endSeconds)
    try container.encode(isFinal, forKey: .isFinal)
  }
}

/// Parse an RFC 3339 timestamp, with or without a fractional second.
func parseTimestamp(_ text: String) -> Date? {
  if let date = try? Date.ISO8601FormatStyle().parse(text) {
    return date
  }
  return try? Date.ISO8601FormatStyle(includingFractionalSeconds: true).parse(text)
}
