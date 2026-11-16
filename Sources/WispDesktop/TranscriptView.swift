import Observation
import SwiftUI
import WispCore

/// The mic/system transcript shared by the live and history screens.
///
/// A partial row keeps ghost-text styling (and the caret) until the engine
/// reports it final, so the reader can see which line is still moving. The live
/// view follows the newest row while the reader is near the bottom and stops
/// following once they scroll away.
struct TranscriptView: View {
  let segments: [LiveSegment]
  /// Whether the list should follow the newest row. History is static.
  var follows = false

  /// How close to the bottom still counts as "following".
  private static let followThreshold: CGFloat = 48
  private static let bottomAnchor = "transcript-bottom"

  @StateObject private var scroll = ScrollPosition()

  var body: some View {
    if segments.isEmpty {
      Text("Transcript will appear here as it is recognized.")
        .font(.callout)
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    } else {
      ScrollViewReader { proxy in
        ScrollView {
          LazyVStack(alignment: .leading, spacing: 8) {
            ForEach(Array(segments.enumerated()), id: \.offset) { index, segment in
              SegmentCard(segment: segment, isLatest: index == segments.count - 1)
            }
            Color.clear
              .frame(height: 1)
              .id(Self.bottomAnchor)
          }
          .padding(.bottom, 16)
        }
        .onScrollGeometryChange(for: CGFloat.self) { geometry in
          geometry.contentSize.height + geometry.contentInsets.bottom
            - (geometry.contentOffset.y + geometry.containerSize.height)
        } action: { _, distance in
          scroll.distanceFromBottom = distance
        }
        .onChange(of: segments.count) {
          guard follows, scroll.distanceFromBottom <= Self.followThreshold else { return }
          withAnimation(.easeOut(duration: 0.15)) {
            proxy.scrollTo(Self.bottomAnchor, anchor: .bottom)
          }
        }
      }
    }
  }
}

/// Where the transcript list is relative to its bottom edge.
private final class ScrollPosition: ObservableObject {
  @Published var distanceFromBottom: CGFloat = 0
}

private struct SegmentCard: View {
  let segment: LiveSegment
  let isLatest: Bool

  var body: some View {
    VStack(alignment: .leading, spacing: 6) {
      HStack(spacing: 8) {
        Text(segment.source == .mic ? "MIC" : "SYS")
          .font(.caption2.weight(.semibold))
          .padding(.horizontal, 7)
          .padding(.vertical, 2)
          .background(tagColor.opacity(0.16), in: Capsule())
          .foregroundStyle(tagColor)
        if !segment.isFinal {
          Text("transcribing…")
            .font(.caption2)
            .foregroundStyle(.secondary)
        }
      }
      Text(segmentText)
        .font(.system(size: 15))
        .lineSpacing(4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .textSelection(.enabled)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 12)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 8))
    .overlay(
      RoundedRectangle(cornerRadius: 8)
        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
    )
  }

  private var tagColor: Color {
    segment.source == .mic ? .accentColor : .orange
  }

  /// The caret marks the row the engine may still revise, so it only appears on
  /// the newest partial.
  private var segmentText: String {
    if !segment.isFinal, isLatest {
      return segment.displayText + "▍"
    }
    return segment.displayText
  }
}
