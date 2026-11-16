import Foundation
import Observation
import SwiftUI
import WispAudioKit
import WispCore
import WispLifecycle

/// A failure the status strip reports.
enum AppError: Error, Equatable, Sendable {
  /// The audio session failed to start, run, or clean up.
  case audio(SessionError)
  /// A `sessions.db` read or write failed.
  case persistence(String)
}

extension AppError: CustomStringConvertible {
  var description: String {
    switch self {
    case .audio(let error):
      error.description
    case .persistence(let message):
      "session history persistence failed: \(message)"
    }
  }
}

/// Which top-level screen the window shows.
enum AppViewRoute: Equatable, Hashable, Sendable {
  /// Saved sessions, newest first.
  case library
  /// The recording screen: idle, recording, or just-stopped.
  case live
  /// A saved session's read-only transcript.
  case history(SessionID)
  /// Preferences. Opening it never discards a settling session.
  case settings
}

/// A storage operation that failed and must be retried before the live
/// transcript may be discarded or another session started.
enum PendingSessionWrite: Equatable, Sendable {
  case finalise(endedAt: Date)
  case delete
}

/// One row of the live transcript.
struct LiveSegment: Identifiable, Equatable, Sendable {
  var source: SourceLabel
  /// Monotonic-per-source identifier from the transcription pipeline. The
  /// same id while the engine revises the row; a new id finalises the previous.
  var id: UInt64
  var text: String
  /// Pre-rendered body with sentence breaks applied, so a long history does
  /// not re-walk every segment on each frame.
  var displayText: String
  var startSeconds: Double
  var endSeconds: Double
  /// True once the speech engine has stopped revising this row.
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
    displayText = breakOnSentenceEnd(text)
    self.startSeconds = startSeconds
    self.endSeconds = endSeconds
    self.isFinal = isFinal
  }
}

/// Insert a `\n` after each sentence-ending `。` *except* a trailing one, so
/// the partial line does not visibly break the moment the punctuation is
/// recognised; the break appears once the next sentence starts arriving.
func breakOnSentenceEnd(_ text: String) -> String {
  var out = ""
  out.reserveCapacity(text.count + 8)
  var iterator = text.makeIterator()
  var current = iterator.next()
  while let character = current {
    let next = iterator.next()
    out.append(character)
    if character == "。", next != nil {
      out.append("\n")
    }
    current = next
  }
  return out
}

/// Heuristic: are these two strings the same utterance emitted under two
/// segment ids?
///
/// `SpeechAnalyzer` sometimes ratifies the previous range and starts the next
/// one under a fresh id with (nearly) identical text. Comparing common prefix
/// plus common suffix against the shorter string catches that, and the 60%
/// floor catches small in-the-middle revisions (`MaOS` → `MacOS`) while not
/// collapsing genuinely separate short utterances.
func looksLikeContinuation(_ prev: String, _ new: String) -> Bool {
  if prev == new {
    return true
  }
  if prev.isEmpty || new.isEmpty {
    return false
  }
  let commonPrefix = zip(prev, new).prefix { $0 == $1 }.count
  let commonSuffix = zip(prev.reversed(), new.reversed()).prefix { $0 == $1 }.count
  let shorter = min(prev.count, new.count)
  // Capped so prefix + suffix cannot double-count on tiny strings.
  let matched = min(commonPrefix + commonSuffix, shorter)
  let threshold = max(6, shorter * 6 / 10)
  return matched >= threshold
}

/// Either revise the active partial for this source or start a new segment,
/// finalising the previous one for the same source.
func upsertSegment(_ incoming: LiveSegment, into segments: inout [LiveSegment]) {
  // Walk newest → oldest to find the latest entry for this source.
  for index in segments.indices.reversed() {
    guard segments[index].source == incoming.source else { continue }
    if segments[index].id == incoming.id {
      segments[index] = incoming
      return
    }
    // A different id for the same source whose text looks like a continuation
    // is the same utterance re-emitted, so it replaces the row in place rather
    // than appearing twice. Anything else locks the previous row in.
    if !segments[index].isFinal,
      looksLikeContinuation(segments[index].text, incoming.text)
    {
      segments[index] = incoming
      return
    }
    segments[index].isFinal = true
    break
  }
  segments.append(incoming)
}

extension WispSession.Source {
  /// The persisted/displayable label for this capture track.
  var label: SourceLabel {
    self == .mic ? .mic : .system
  }
}

/// A detected call waiting for the user's answer.
struct MeetingPrompt: Equatable, Sendable {
  let tool: MeetingTool
}

/// In-memory transcript model plus the library, permission, and navigation
/// state the SwiftUI screens read.
///
/// The session worker writes here through `SessionIngest`; the views only read
/// and call the intent methods, so one instance is the single source of truth.
@MainActor
@Observable
final class AppModel {
  var state: Phase = .idle
  /// Whether live microphone samples are currently replaced with silence.
  var microphoneMuted = false
  var view: AppViewRoute = .library
  var segments: [LiveSegment] = []
  /// All persisted sessions, newest first.
  var library: [WispCore.Session] = []
  /// Open database row owned by an active or not-yet-persisted recording.
  /// Cleared only when finalisation commits or an unstarted row is deleted.
  var currentSessionID: SessionID?
  /// Persisted row for the transcript on screen. Survives a successful
  /// finalisation so export metadata cannot drift to another session.
  var linkedSessionID: SessionID?
  var pendingSessionWrite: PendingSessionWrite?
  /// Launch metadata retained until the transcript transaction commits, so a
  /// retry can create the row if the `started` update never acquired one.
  var currentSessionStartedAt: Date?
  var currentSessionDirName: String?
  /// Per-run audio directory, retained with the transcript after a storage
  /// failure so a recovery snapshot can be written beside the Ogg files.
  var currentOutputDir: URL?
  /// The session shown by `.history`, kept so the header renders its title
  /// without re-querying.
  var viewedSession: WispCore.Session?
  /// Working title for the live session; empty means "fall back to the
  /// recorded-at timestamp" when the row is created.
  var liveTitle = ""
  /// User-typed transcript search. Empty shows every session.
  var libraryQuery = ""
  /// Session ids matching `libraryQuery` in relevance order. `nil` means no
  /// query is active.
  var libraryHits: [SessionID]?
  /// Session waiting for the second click on Delete.
  var pendingDelete: SessionID?
  var meetingPrompt: MeetingPrompt?
  var permissions = Permissions()
  var lastError: AppError?
  /// The screen Settings was opened from.
  var preSettingsView: AppViewRoute?
  /// Sidebar column visibility, toggled from the sidebar footer.
  var sidebarVisibility = NavigationSplitViewVisibility.all
  /// The history title as typed. Committed on submit, so a half-typed rename is
  /// never written to the library.
  var historyTitleDraft = ""

  /// Anchor for the recording clock, on the monotonic clock so a wall-clock
  /// change cannot make it jump.
  private var recordingStartedAt: ContinuousClock.Instant?
  /// The recording clock as last sampled. The pump refreshes it, so the view
  /// does not need a timer of its own.
  private(set) var elapsedMilliseconds: UInt64 = 0

  /// Whether the live transcript still owns worker or persistence state. A
  /// retained handle after a failed finalisation deliberately counts, so
  /// navigation cannot silently discard it.
  var hasUnsettledSession: Bool {
    !canReplaceTranscript(
      phase: state,
      pendingPersistence: pendingSessionWrite != nil,
      retainedOutput: currentOutputDir != nil
    )
  }

  /// A stopped worker whose transcript transaction needs a retry.
  var hasPendingPersistence: Bool {
    state == .failed && pendingSessionWrite != nil
  }

  /// Whether the minimum permissions for this build are available.
  var setupComplete: Bool {
    permissions.satisfies(recordOnlyAllowed: true)
  }

  /// The live owner for the current screen.
  ///
  /// Settings owns no session, so it maps to `.live` and a recording keeps
  /// ingesting while the user reads preferences.
  var viewOwner: ViewOwner {
    switch view {
    case .library: .library
    case .live, .settings: .live
    case .history: .history
    }
  }

  private var updateContext: UpdateContext {
    UpdateContext(
      phase: state,
      view: viewOwner,
      currentSessionID: currentSessionID
    )
  }

  /// Whether an update tagged with `sessionID` may mutate this transcript.
  func acceptsWorkerUpdate(_ sessionID: SessionID) -> Bool {
    updateContext.accepts(sessionID)
  }

  /// The phase the context moves to for `update`, or `nil` when rejected.
  func workerUpdatePhase(_ sessionID: SessionID, update: WorkerUpdate) -> Phase? {
    updateContext.nextPhase(sessionID, update: update)
  }

  /// Refresh the elapsed clock while a recording runs.
  func tickClock() {
    let elapsed: UInt64
    if state == .recording, let recordingStartedAt {
      elapsed = UInt64(max(0, recordingStartedAt.duration(to: .now) / .milliseconds(1)))
    } else {
      elapsed = 0
    }
    // Observation fires on assignment, so a second at rest must not repaint.
    if elapsed != elapsedMilliseconds {
      elapsedMilliseconds = elapsed
    }
  }

  /// Show the preferences page. Unlike the other `show*` methods this never
  /// consults `hasUnsettledSession`: opening settings discards nothing.
  func showSettings() {
    guard view != .settings else { return }
    preSettingsView = view
    view = .settings
  }

  /// Return to the screen Settings was opened from, bypassing the `show*`
  /// guards so a recording stays reachable.
  func closeSettings() {
    view = preSettingsView ?? .library
    preSettingsView = nil
  }

  /// Move to the library and drop any live or historical transcript.
  ///
  /// Ignored while a worker still owns a session: its future events would
  /// otherwise attach to the library or to a historical transcript.
  func showLibrary() {
    guard !hasUnsettledSession else { return }
    view = .library
    resetTranscript()
  }

  /// Move to the live screen in idle state, used by New Session.
  func showNewSession() {
    guard !hasUnsettledSession else { return }
    view = .live
    state = .idle
    resetTranscript()
  }

  /// Move to a historical transcript. The caller loads `segments` from storage.
  func showHistory(_ session: WispCore.Session, segments: [LiveSegment]) {
    guard !hasUnsettledSession else { return }
    view = .history(session.id)
    microphoneMuted = false
    self.segments = segments
    viewedSession = session
    historyTitleDraft = session.title
    currentSessionID = nil
    linkedSessionID = nil
    pendingSessionWrite = nil
    currentSessionStartedAt = nil
    currentSessionDirName = nil
    currentOutputDir = nil
    pendingDelete = nil
    preSettingsView = nil
    // Historical segments come back finalised already.
    finalizeAllSegments()
  }

  /// Normalize any terminal screen to a fresh live transcript and enter the
  /// starting phase. Global shortcuts call this before enqueueing Start so
  /// Library or History can never own live worker events.
  @discardableResult
  func beginSessionStart() -> Bool {
    guard !hasUnsettledSession else { return false }
    showNewSession()
    state = .starting
    microphoneMuted = false
    meetingPrompt = nil
    return true
  }

  func setState(_ state: Phase) {
    self.state = state
    if state.isActive {
      meetingPrompt = nil
    }
    if state == .recording {
      if recordingStartedAt == nil {
        recordingStartedAt = .now
      }
    } else {
      recordingStartedAt = nil
      microphoneMuted = false
    }
  }

  func fail(_ error: AppError) {
    lastError = error
    state = .failed
    recordingStartedAt = nil
    elapsedMilliseconds = 0
    microphoneMuted = false
    finalizeAllSegments()
  }

  /// Mark every segment final, so the previously active row drops its
  /// ghost-text styling when recording stops.
  func finalizeAllSegments() {
    for index in segments.indices {
      segments[index].isFinal = true
    }
  }

  /// Apply one transcript result from the worker.
  func ingest(_ result: WispSession.Result) {
    upsertSegment(
      LiveSegment(
        source: result.source.label,
        id: result.segmentID,
        text: result.text,
        startSeconds: result.startSeconds,
        endSeconds: result.endSeconds,
        isFinal: result.isFinal
      ),
      into: &segments
    )
  }

  /// Clear every transcript-ownership field without touching `state`, which
  /// the caller has already set.
  private func resetTranscript() {
    microphoneMuted = false
    segments = []
    viewedSession = nil
    liveTitle = ""
    currentSessionID = nil
    linkedSessionID = nil
    pendingSessionWrite = nil
    currentSessionStartedAt = nil
    currentSessionDirName = nil
    currentOutputDir = nil
    lastError = nil
    pendingDelete = nil
    preSettingsView = nil
  }
}
