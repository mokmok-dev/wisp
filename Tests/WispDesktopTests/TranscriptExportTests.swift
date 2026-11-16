import Foundation
import WispCore
import XCTest

@testable import WispDesktop

final class TranscriptExportTests: XCTestCase {
  private func segments() -> [LiveSegment] {
    [
      DesktopFixtures.segment(.system, id: 2, text: "はい", start: 2, end: 3, isFinal: true),
      DesktopFixtures.segment(.mic, id: 1, text: "こんにちは", start: 1, end: 2, isFinal: true),
    ]
  }

  // MARK: - Plain text

  func testPlainTextIsOrderedByStartTimeWithSourceLabels() {
    XCTAssertEqual(
      TranscriptExport.formatPlain(segments: segments()),
      "[MIC] こんにちは\n\n[SYS] はい"
    )
  }

  func testPlainTextSkipsEmptySegments() {
    XCTAssertEqual(
      TranscriptExport.formatPlain(
        segments: [DesktopFixtures.segment(.mic, id: 1, text: "   ")]
      ),
      ""
    )
  }

  // MARK: - Markdown

  /// Split an export into its frontmatter lines and body.
  private func split(_ markdown: String) -> (fields: [String: String], body: String) {
    let lines = markdown.components(separatedBy: "\n")
    XCTAssertEqual(lines.first, "---", "export must open with a fence")
    guard let closing = lines.dropFirst().firstIndex(of: "---") else {
      XCTFail("export must close the frontmatter fence")
      return ([:], "")
    }
    var fields: [String: String] = [:]
    for line in lines[1..<closing] {
      guard let separator = line.firstIndex(of: ":") else { continue }
      let key = String(line[line.startIndex..<separator])
      let value = String(line[line.index(after: separator)...])
        .trimmingCharacters(in: .whitespaces)
      fields[key] = value
    }
    let body = lines[(closing + 1)...].joined(separator: "\n")
      .trimmingCharacters(in: .whitespacesAndNewlines)
    return (fields, body)
  }

  func testMarkdownEnvelopeCarriesTheSessionMetadata() {
    let session = DesktopFixtures.storedSession(id: 42, title: "Weekly sync: Q3")
    let markdown = TranscriptExport.formatMarkdown(session: session, segments: segments())
    let (fields, body) = split(markdown)

    XCTAssertEqual(fields["schema"], "wisp.transcript/v1")
    XCTAssertEqual(fields["type"], "dev.mokmok.wisp.transcript")
    XCTAssertEqual(fields["source"], "wisp")
    XCTAssertEqual(fields["id"], "42")
    // A title containing `:` must survive as one scalar.
    XCTAssertEqual(fields["subject"], "\"Weekly sync: Q3\"")
    XCTAssertEqual(fields["time"], "2026-07-22T09:00:00+00:00")
    XCTAssertEqual(fields["ended_at"], "2026-07-22T09:47:12+00:00")
    XCTAssertEqual(fields["duration_seconds"], "2832")
    XCTAssertEqual(fields["sources"], "[mic, system]")
    XCTAssertEqual(fields["segment_count"], "2")
    XCTAssertEqual(fields["datacontenttype"], "text/plain")
    XCTAssertEqual(fields["generator"], "wisp/\(AppVersion.current)")
    XCTAssertEqual(body, "[MIC] こんにちは\n\n[SYS] はい")
  }

  func testMarkdownEmitsExactlyOneFencePairAndABlankLineBeforeTheBody() {
    let markdown = TranscriptExport.formatMarkdown(session: nil, segments: segments())
    XCTAssertEqual(markdown.components(separatedBy: "\n---").count - 1, 1)
    XCTAssertTrue(markdown.contains("\n---\n\n[MIC]"))
  }

  func testMarkdownWithoutASessionOmitsSessionFields() {
    let markdown = TranscriptExport.formatMarkdown(session: nil, segments: segments())
    let (fields, _) = split(markdown)

    XCTAssertEqual(fields["schema"], "wisp.transcript/v1")
    XCTAssertEqual(fields["segment_count"], "2")
    XCTAssertNil(fields["id"])
    XCTAssertNil(fields["subject"])
    XCTAssertNil(fields["time"])
    XCTAssertNil(fields["ended_at"])
    XCTAssertNil(fields["duration_seconds"])
  }

  func testMarkdownIsEmptyWhenThereIsNoBody() {
    XCTAssertEqual(
      TranscriptExport.formatMarkdown(
        session: DesktopFixtures.storedSession(id: 1, title: "x"),
        segments: [DesktopFixtures.segment(.mic, id: 1, text: "   ")]
      ),
      ""
    )
  }

  func testMarkdownOmitsSourcesThatProducedNoText() {
    let markdown = TranscriptExport.formatMarkdown(
      session: nil,
      segments: [DesktopFixtures.segment(.system, id: 1, text: "はい")]
    )
    let (fields, _) = split(markdown)
    XCTAssertEqual(fields["sources"], "[system]")
  }

  func testMarkdownOmitsTheEndTimeForAStillRecordingSession() {
    let markdown = TranscriptExport.formatMarkdown(
      session: DesktopFixtures.storedSession(id: 1, endedAt: nil),
      segments: segments()
    )
    let (fields, _) = split(markdown)
    XCTAssertNil(fields["ended_at"])
    XCTAssertNil(fields["duration_seconds"])
  }

  func testMarkdownOmitsABlankTitle() {
    let markdown = TranscriptExport.formatMarkdown(
      session: DesktopFixtures.storedSession(id: 1, title: "  "),
      segments: segments()
    )
    let (fields, _) = split(markdown)
    XCTAssertNil(fields["subject"])
  }

  // MARK: - Filenames

  func testFilenameSanitizingReplacesUnsafeCharacters() {
    XCTAssertEqual(
      TranscriptExport.sanitizeFilename("Meeting 2026/06/02"),
      "Meeting_2026_06_02"
    )
  }

  func testFilenameSanitizingCollapsesWhitespaceRuns() {
    XCTAssertEqual(TranscriptExport.sanitizeFilename("a   b\tc"), "a_b_c")
  }

  func testFilenameSanitizingKeepsAsciiLettersDigitsDashAndUnderscore() {
    XCTAssertEqual(TranscriptExport.sanitizeFilename("Q3_review-notes"), "Q3_review-notes")
  }

  func testFilenameSanitizingFallsBackWhenNothingSurvives() {
    XCTAssertEqual(TranscriptExport.sanitizeFilename("   "), "transcript")
    XCTAssertEqual(TranscriptExport.sanitizeFilename("会議"), "transcript")
  }

  func testFilenameSanitizingTrimsEdgeUnderscores() {
    XCTAssertEqual(TranscriptExport.sanitizeFilename("_x_"), "x")
  }

  func testSuggestedNamePrefersTheTitleAndFallsBack() {
    XCTAssertEqual(
      TranscriptExport.suggestedExportName(title: "Weekly sync", fallback: "transcript"),
      "Weekly_sync"
    )
    XCTAssertEqual(
      TranscriptExport.suggestedExportName(title: nil, fallback: "transcript"),
      "transcript"
    )
  }

  // MARK: - Timestamps

  func testTimestampsUseAColonOffsetInUTC() {
    XCTAssertEqual(
      TranscriptExport.rfc3339(DesktopFixtures.date(2026, 7, 22, 9, 0, 0)),
      "2026-07-22T09:00:00+00:00"
    )
  }

  func testTimestampsRoundTripThroughTheParser() {
    let date = DesktopFixtures.date(2026, 7, 22, 9, 47, 12)
    XCTAssertEqual(parseTimestamp(TranscriptExport.rfc3339(date)), date)
  }
}

final class FormatTests: XCTestCase {
  func testClockHandlesMinutesAndHours() {
    XCTAssertEqual(Format.clock(totalMilliseconds: 0), "00:00")
    XCTAssertEqual(Format.clock(totalMilliseconds: 5000), "00:05")
    XCTAssertEqual(Format.clock(totalMilliseconds: 65000), "01:05")
    XCTAssertEqual(Format.clock(totalMilliseconds: 3_725_000), "1:02:05")
  }

  func testDurationRendersAnEmDashWhenUnknown() {
    XCTAssertEqual(Format.duration(seconds: nil), "—")
    XCTAssertEqual(Format.duration(seconds: -1), "—")
    XCTAssertEqual(Format.duration(seconds: 90), "01:30")
  }

  func testStartedAtUsesLocalTime() {
    let started = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.calendar = Calendar(identifier: .gregorian)
    formatter.timeZone = .current
    formatter.dateFormat = "yyyy-MM-dd HH:mm"
    XCTAssertEqual(Format.startedAt(started), formatter.string(from: started))
  }
}
