import Observation
import WispAudioKit
import WispLifecycle

/// How a finished dictation run ends.
enum DictationOutcome: Equatable, Sendable {
  /// Copy the transcript to the clipboard.
  case deliver
  /// Drop it, for example when the chord interrupts a starting run.
  case discard
}

/// The dictation side of the microphone, from a chord press to the settle.
enum DictationState: Equatable, Sendable {
  case idle
  case starting
  case listening
  case finishing(DictationOutcome)

  /// True from the moment capture is claimed until the engine has stopped.
  var holdsMicrophone: Bool {
    switch self {
    case .idle: false
    case .starting, .listening, .finishing: true
    }
  }
}

/// Inputs the reducer understands; anything not named is not an event.
enum DictationEvent: Equatable, Sendable {
  case hotKeyPressed
  case captureStarted
  case captureFailed(DictationCaptureError)
  case captureFinished
  case stopRequested(DictationOutcome)
  case terminate
}

/// One side effect the controller performs after a transition.
enum DictationEffect: Equatable, Sendable {
  case beginCapture
  case stopCapture(DictationOutcome)
  case fail(DictationCaptureError)
  /// The engine has stopped. `deliver` copies the transcript, `discard` drops it.
  case settle(DictationOutcome)
  case none
}

struct DictationTransition: Equatable, Sendable {
  let state: DictationState
  let effect: DictationEffect
}

/// The complete dictation transition table. `nil` means the event does not
/// apply in this state and the caller drops it.
func dictationTransition(
  from state: DictationState,
  on event: DictationEvent
) -> DictationTransition? {
  switch (state, event) {
  case (.idle, .hotKeyPressed):
    DictationTransition(state: .starting, effect: .beginCapture)
  case (.starting, .captureStarted):
    DictationTransition(state: .listening, effect: .none)
  case (.starting, .hotKeyPressed):
    DictationTransition(state: .finishing(.discard), effect: .stopCapture(.discard))
  case (.starting, .captureFailed(let error)):
    DictationTransition(state: .idle, effect: .fail(error))
  case (.listening, .hotKeyPressed):
    DictationTransition(state: .finishing(.deliver), effect: .stopCapture(.deliver))
  case (.listening, .stopRequested(let outcome)):
    DictationTransition(state: .finishing(outcome), effect: .stopCapture(outcome))
  case (.listening, .captureFailed(let error)):
    DictationTransition(state: .idle, effect: .fail(error))
  case (.finishing(let outcome), .captureFinished):
    DictationTransition(state: .idle, effect: .settle(outcome))
  case (.finishing, .captureFailed):
    DictationTransition(state: state, effect: .none)
  case (.finishing, .hotKeyPressed), (.finishing, .stopRequested):
    DictationTransition(state: state, effect: .none)
  case (_, .terminate):
    DictationTransition(state: .idle, effect: .stopCapture(.discard))
  default:
    nil
  }
}

/// The notice card the HUD shows.
enum DictationNotice: Equatable, Sendable {
  /// A recording owns the device.
  case microphoneBusy
  case permissionRequired(String)
  case nothingRecognized
  case copied
  case copiedAfterFailure
  case failed(String)
}

enum MicrophoneClaimant: Equatable, Sendable {
  case recording
  case dictation
}

/// Recording owns the device through `Phase.isActive`; dictation through
/// `DictationState.holdsMicrophone`. No third stored flag exists.
///
/// Each side only has to answer for the other: dictation may press to stop
/// itself, and a recording may stop itself, while neither may start over the
/// other's claim.
func canStartCapture(
  _ claimant: MicrophoneClaimant,
  recording phase: Phase,
  dictation state: DictationState
) -> Bool {
  switch claimant {
  case .recording: !state.holdsMicrophone
  case .dictation: !phase.isActive
  }
}

/// The dictated text, ready to paste as prose.
///
/// Segments break where the analyzer paused, not where the writer ended a
/// sentence, so a newline between them would paste a hard break mid-sentence.
/// A space separates two Latin words; scripts written without word spaces get
/// no separator. The raw text is used rather than `displayText`, whose
/// sentence breaks exist to lay out the transcript view.
func dictationText(_ segments: [LiveSegment]) -> String {
  var out = ""
  for segment in segments {
    let line = segment.text.trimmingCharacters(in: .whitespacesAndNewlines)
    if line.isEmpty { continue }
    if let last = out.last, let first = line.first,
      needsWordSpace(after: last, before: first)
    {
      out.append(" ")
    }
    out.append(line)
  }
  return out
}

/// Whether two adjacent characters need a space between them when their
/// segments are joined.
func needsWordSpace(after previous: Character, before next: Character) -> Bool {
  !isUnspacedScript(previous) && !isUnspacedScript(next)
}

/// Scripts that run words together. This covers the ranges Wisp's language
/// list writes in without word spaces; anything else takes a space.
func isUnspacedScript(_ character: Character) -> Bool {
  guard let scalar = character.unicodeScalars.first else { return false }
  switch scalar.value {
  case 0x3000...0x30FF,  // CJK punctuation, Hiragana, Katakana
    0x3400...0x4DBF,  // CJK unified ideographs extension A
    0x4E00...0x9FFF,  // CJK unified ideographs
    0xAC00...0xD7AF,  // Hangul syllables
    0xF900...0xFAFF,  // CJK compatibility ideographs
    0xFF00...0xFF60:  // Fullwidth forms
    return true
  default:
    return false
  }
}

/// Observable state the HUD and the reducer share. `apply` performs the state
/// change and returns the effect; the controller performs the effect, so this
/// type never calls out.
@MainActor
@Observable
final class DictationModel {
  private(set) var state: DictationState = .idle
  /// Same element type and only writer as the live transcript.
  private(set) var segments: [LiveSegment] = []
  private(set) var notice: DictationNotice?
  private(set) var shortcutFailure: String?

  /// Upsert one capture row into dictation's own array, ignoring anything
  /// that arrives once the engine has stopped.
  func ingest(_ update: DictationCapture.Update) {
    guard state.holdsMicrophone else { return }
    upsertSegment(
      LiveSegment(
        source: .mic,
        id: update.segmentID,
        text: update.text,
        startSeconds: update.startSeconds,
        endSeconds: update.endSeconds,
        isFinal: update.isFinal
      ),
      into: &segments
    )
  }

  @discardableResult
  func apply(_ event: DictationEvent) -> DictationEffect {
    guard let transition = dictationTransition(from: state, on: event) else {
      return .none
    }
    state = transition.state
    return transition.effect
  }

  func showNotice(_ notice: DictationNotice) {
    self.notice = notice
  }

  func clearNotice() {
    notice = nil
  }

  func reportShortcutFailure(_ message: String?) {
    shortcutFailure = message
  }

  /// Leave `finishing` for `idle` and drop the transcript. Idempotent.
  func settle() {
    state = .idle
    segments = []
    notice = nil
  }
}
