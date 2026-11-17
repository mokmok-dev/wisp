import AppKit
import SwiftUI

/// Placement, free of AppKit state so it is testable.
enum DictationHUDFrame {
  static let width: Double = 520
  /// The HUD sits this far above the bottom of the visible frame.
  static let bottomInset: Double = 24

  /// Bottom centre of `visibleFrame`, `bottomInset` above its lower edge.
  static func origin(size: CGSize, in visibleFrame: CGRect) -> CGPoint {
    CGPoint(
      x: visibleFrame.midX - size.width / 2,
      y: visibleFrame.minY + bottomInset
    )
  }

  /// The visible frame containing the pointer, else `nil`.
  static func target(mouse: CGPoint, visibleFrames: [CGRect]) -> CGRect? {
    visibleFrames.first { $0.contains(mouse) }
  }
}

/// The floating dictation card: a non-activating panel that can appear over
/// any space, including a full-screen one, and never takes focus.
@MainActor
final class DictationHUD {
  private let model: DictationModel
  private let panel: Panel
  private var visible = false
  private var screenObserver: NSObjectProtocol?
  /// The screen frame the card was shown over, for reflowing when the
  /// content's height changes.
  private var anchoredVisibleFrame: CGRect?

  init(model: DictationModel) {
    self.model = model
    panel = Panel(
      contentRect: .zero,
      styleMask: [.borderless, .nonactivatingPanel],
      backing: .buffered,
      defer: false
    )
    panel.level = .floating
    panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
    panel.hidesOnDeactivate = false
    panel.isOpaque = false
    panel.backgroundColor = .clear
    panel.isReleasedWhenClosed = false
    panel.ignoresMouseEvents = true
    panel.contentView = hosting
  }

  /// Built on first use so the resize callback can reach `fit()` once `self`
  /// is fully initialized.
  private lazy var hosting = NSHostingView(
    rootView: Content(model: model) { [weak self] in
      self?.fit()
    }
  )

  /// Order the panel in without activating Wisp.
  func show() {
    guard !visible else { return }
    visible = true
    installScreenObserver()
    position()
    panel.alphaValue = 0
    panel.orderFrontRegardless()
    guard !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else {
      panel.alphaValue = 1
      return
    }
    panel.animator().alphaValue = 1
  }

  func dismiss() {
    guard visible else { return }
    visible = false
    removeScreenObserver()
    anchoredVisibleFrame = nil
    panel.orderOut(nil)
  }

  // MARK: - Placement

  private func position() {
    let frames = NSScreen.screens.map(\.visibleFrame)
    let frame =
      DictationHUDFrame.target(mouse: NSEvent.mouseLocation, visibleFrames: frames)
      ?? NSScreen.main?.visibleFrame ?? .zero
    anchoredVisibleFrame = frame
    fit()
  }

  /// Size the panel to the card at a fixed width, then keep its bottom edge
  /// where `DictationHUDFrame.origin` puts it. Asking the hosting view for its
  /// fitting size at that width makes the height follow the text without
  /// letting AppKit also resize the width around a shortest line.
  private func fit() {
    guard let frame = anchoredVisibleFrame, !frame.isEmpty else { return }
    let width = min(DictationHUDFrame.width, frame.width)
    hosting.frame.size.width = width
    let height = min(max(hosting.fittingSize.height, 60), 320)
    let origin = DictationHUDFrame.origin(
      size: CGSize(width: width, height: height),
      in: frame
    )
    panel.setFrame(
      NSRect(origin: origin, size: CGSize(width: width, height: height)),
      display: true
    )
  }

  private func installScreenObserver() {
    guard screenObserver == nil else { return }
    screenObserver = NotificationCenter.default.addObserver(
      forName: NSApplication.didChangeScreenParametersNotification,
      object: nil,
      queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated {
        self?.position()
      }
    }
  }

  private func removeScreenObserver() {
    if let screenObserver {
      NotificationCenter.default.removeObserver(screenObserver)
      self.screenObserver = nil
    }
  }
}

/// The card's panel class. Rule details live in the type itself so `init`
/// stays a straight constructor; `canBecomeKey` and `canBecomeMain` false is
/// what keeps `orderFrontRegardless` from stealing focus.
private final class Panel: NSPanel {
  override var canBecomeKey: Bool { false }
  override var canBecomeMain: Bool { false }
}

/// The card the HUD renders. `Content` holds `DictationModel` through the
/// object itself, so a transcription update paints in the frame it lands in.
private struct Content: View {
  @Bindable var model: DictationModel
  let onResize: () -> Void

  var body: some View {
    bodyContent
      // A row or notice change moves the card's bottom edge, so the frame
      // tracks the model rather than polling.
      .onChange(of: bodyKey) { _, _ in onResize() }
      .onAppear(perform: onResize)
  }

  private var bodyKey: String {
    "\(model.notice == nil)\(model.state)(\(model.segments.count))"
  }

  @ViewBuilder private var bodyContent: some View {
    VStack(alignment: .leading, spacing: 8) {
      if let notice = model.notice {
        Text(notice.message)
          .font(.callout)
          .foregroundStyle(notice.isError ? Color.red : Color.primary)
          .frame(maxWidth: .infinity, alignment: .leading)
      } else {
        Text(caption)
          .font(.caption2.weight(.semibold))
          .foregroundStyle(.secondary)
        rows
      }
    }
    .padding(14)
    .frame(width: DictationHUDFrame.width, alignment: .leading)
    .background(
      RoundedRectangle(cornerRadius: 12)
        .fill(Color(nsColor: .textBackgroundColor))
    )
    .overlay(
      RoundedRectangle(cornerRadius: 12)
        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
    )
    .shadow(color: .black.opacity(0.22), radius: 12, y: 4)
    // The card is read as one transcript, so its rows stay inaccessible
    // separately.
    .accessibilityElement(children: .combine)
    .accessibilityLabel(model.notice == nil ? "Live dictation transcript" : "Dictation notice")
  }

  private var caption: String {
    switch model.state {
    case .starting: "Starting dictation…"
    case .listening: "Dictating"
    case .finishing(.deliver): "Finishing — copying…"
    case .finishing(.discard): "Finishing dictation…"
    case .idle: ""
    }
  }

  /// Only the last three rows show; each line sits on one row and a partial
  /// newest row carries the caret the engine may still revise.
  @ViewBuilder private var rows: some View {
    let visible = model.segments.suffix(3)
    ForEach(Array(visible.enumerated()), id: \.offset) { pair in
      let segment = pair.element
      Text(rowText(segment, isLast: pair.offset == visible.count - 1))
        .font(.system(size: 14))
        .lineLimit(1)
        .truncationMode(.head)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
    if visible.isEmpty {
      Text(model.state == .starting ? "Waiting for audio…" : "Listening…")
        .font(.system(size: 14))
        .foregroundStyle(.secondary)
    }
  }

  private func rowText(_ segment: LiveSegment, isLast: Bool) -> String {
    let line = segment.displayText.replacingOccurrences(of: "\n", with: " ")
    if !segment.isFinal, isLast {
      return line + "▍"
    }
    return line
  }
}

extension DictationNotice {
  var message: String {
    switch self {
    case .microphoneBusy: "A recording is in progress. Finish it before dictating."
    case .permissionRequired(let permission): "\(permission) permission is required to dictate."
    case .nothingRecognized: "Nothing was recognized."
    case .copied: "Dictation copied to the clipboard."
    case .copiedAfterFailure: "Dictation stopped early. The transcript so far was copied."
    case .failed(let message): message
    }
  }

  var isError: Bool {
    switch self {
    case .microphoneBusy, .permissionRequired, .failed:
      true
    case .nothingRecognized, .copied, .copiedAfterFailure:
      false
    }
  }
}
