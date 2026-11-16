import SwiftUI
import WispCore

/// Saved sessions: search, open, rename by navigation, and delete.
struct LibraryView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      ScreenBody {
        list
      }
      ScreenFooter(leading: "stored locally as Ogg/Opus + SQLite", trailing: nil)
    }
  }

  private var total: Int {
    model.library.count
  }

  private var queryActive: Bool {
    !model.libraryQuery.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
  }

  private var visible: [WispCore.Session] {
    guard let hits = model.libraryHits else { return model.library }
    return hits.compactMap { id in model.library.first { $0.id == id } }
  }

  private var header: some View {
    HStack(alignment: .center) {
      VStack(alignment: .leading, spacing: 2) {
        Text("Sessions").font(.title3.weight(.semibold))
        Text(countLine)
          .font(.callout)
          .foregroundStyle(.secondary)
      }
      Spacer()
      TextField(
        "Search transcripts",
        text: Binding(
          get: { model.libraryQuery },
          set: { controller.setLibraryQuery($0) }
        )
      )
      .textFieldStyle(.roundedBorder)
      .frame(width: 220)
      Button {
        model.showNewSession()
      } label: {
        Label("New Session", systemImage: "plus")
      }
      .buttonStyle(.borderedProminent)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 12)
  }

  private var countLine: String {
    if total == 0 {
      return "Recordings and transcripts stay on this Mac."
    }
    if queryActive {
      return "\(visible.count) of \(total) sessions match."
    }
    return total == 1 ? "1 session saved locally." : "\(total) sessions saved locally."
  }

  @ViewBuilder
  private var list: some View {
    if total == 0 {
      emptyState
    } else if visible.isEmpty {
      noMatchState
    } else {
      ScrollView {
        LazyVStack(spacing: 4) {
          ForEach(visible, id: \.id) { session in
            LibraryRow(session: session, model: model, controller: controller)
          }
        }
      }
    }
  }

  private var emptyState: some View {
    VStack(spacing: 12) {
      Image(systemName: "mic")
        .font(.system(size: 36, weight: .light))
        .foregroundStyle(.secondary)
      Text("No sessions yet").font(.headline)
      Text(
        "Start a new session to capture a meeting, a call, or a voice memo — "
          + "transcribed entirely on-device."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      .multilineTextAlignment(.center)
      .fixedSize(horizontal: false, vertical: true)
      Button {
        model.showNewSession()
      } label: {
        Label("New Session", systemImage: "plus")
      }
      .buttonStyle(.borderedProminent)
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity)
  }

  private var noMatchState: some View {
    VStack(spacing: 10) {
      Image(systemName: "magnifyingglass")
        .font(.system(size: 36, weight: .light))
        .foregroundStyle(.secondary)
      Text("No sessions match").font(.headline)
      Text(
        "Nothing in the saved transcripts matches \"\(model.libraryQuery.trimmingCharacters(in: .whitespacesAndNewlines))\"."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      .multilineTextAlignment(.center)
      Button("Clear Search") {
        controller.clearLibrarySearch()
      }
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity)
  }
}

private struct LibraryRow: View {
  let session: WispCore.Session
  let model: AppModel
  let controller: SessionController

  private var isPendingDelete: Bool {
    model.pendingDelete == session.id
  }

  private var isLive: Bool {
    session.endedAt == nil
  }

  private var duration: Int? {
    session.endedAt.map { Int($0.timeIntervalSince(session.startedAt).rounded(.down)) }
  }

  var body: some View {
    HStack(spacing: 12) {
      // Navigation lives on the label so the delete buttons cannot also open
      // the session before the confirm state renders.
      Button {
        controller.openHistory(session.id)
      } label: {
        HStack(spacing: 12) {
          VStack(alignment: .leading, spacing: 2) {
            Text(session.title)
              .font(.body.weight(.medium))
              .lineLimit(1)
              .truncationMode(.tail)
            Text(Format.startedAt(session.startedAt))
              .font(.caption)
              .foregroundStyle(.secondary)
          }
          Spacer(minLength: 8)
          Text(Format.duration(seconds: duration))
            .font(.callout)
            .foregroundStyle(.secondary)
          if isLive {
            TagView(label: "live", color: .red)
          }
        }
        .contentShape(Rectangle())
      }
      .buttonStyle(.plain)

      if !isLive {
        if isPendingDelete {
          Button("Delete", role: .destructive) {
            controller.confirmDelete(session.id)
          }
          .controlSize(.small)
          Button("Cancel") {
            model.pendingDelete = nil
          }
          .controlSize(.small)
        } else {
          Button {
            model.pendingDelete = session.id
          } label: {
            Image(systemName: "trash")
          }
          .buttonStyle(.borderless)
          .controlSize(.small)
          .help("Delete session")
          .accessibilityLabel("Delete session")
        }
      }
    }
    .padding(.horizontal, 12)
    .padding(.vertical, 8)
    .background(Color.clear, in: RoundedRectangle(cornerRadius: 6))
    .contentShape(Rectangle())
  }
}
