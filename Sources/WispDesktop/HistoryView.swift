import SwiftUI
import WispCore

/// A saved session's transcript with inline rename, copy, and export.
struct HistoryView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      ScreenBody {
        TranscriptView(segments: model.segments)
      }
      ScreenFooter(
        leading: "stored locally as Ogg/Opus + SQLite",
        trailing: "⌘⇧C copy · ⌘⇧E export"
      )
    }
  }

  private var session: WispCore.Session? {
    model.viewedSession
  }

  private var duration: Int? {
    guard let session, let endedAt = session.endedAt else { return nil }
    return Int(endedAt.timeIntervalSince(session.startedAt).rounded(.down))
  }

  private var header: some View {
    HStack(spacing: 12) {
      Button {
        model.showLibrary()
      } label: {
        Image(systemName: "chevron.left")
      }
      .buttonStyle(.bordered)
      .help("Back to library")
      .accessibilityLabel("Back to library")

      TextField(
        "Untitled session",
        text: $model.historyTitleDraft
      )
      .textFieldStyle(.roundedBorder)
      .frame(minWidth: 160, maxWidth: 420)
      .layoutPriority(-1)
      .onSubmit { controller.renameViewedSession(model.historyTitleDraft) }

      HStack(spacing: 8) {
        if let session {
          Text(Format.startedAt(session.startedAt))
            .font(.callout)
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .fixedSize()
        }
        TagView(label: Format.duration(seconds: duration), color: .secondary)
        Button {
          controller.copyTranscript()
        } label: {
          Label("Copy", systemImage: "doc.on.doc")
        }
        .buttonStyle(.bordered)
        .help("Copy transcript (⌘⇧C)")
        Button {
          controller.exportTranscript()
        } label: {
          Label("Export", systemImage: "square.and.arrow.down")
        }
        .buttonStyle(.borderedProminent)
        .help("Export as Markdown (⌘⇧E)")
      }
      Spacer(minLength: 0)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 12)
  }
}
