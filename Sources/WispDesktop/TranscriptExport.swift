import AppKit
import Foundation
import UniformTypeIdentifiers
import WispCore

/// Format in-memory transcript segments for clipboard copy and file export.
enum TranscriptExport {
  /// What the document is, independent of any session.
  static let envelopeType = "dev.mokmok.wisp.transcript"
  /// Envelope schema identifier; bump when the frontmatter shape changes.
  static let envelopeSchema = "wisp.transcript/v1"

  /// Plain text with one block per segment, sorted by start time.
  ///
  /// Each non-empty segment becomes `[MIC] …` or `[SYS] …` so pasted text
  /// stays readable outside Wisp.
  static func formatPlain(segments: [LiveSegment]) -> String {
    let ordered =
      segments
      .filter { !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
      // Both tracks' first segments start at the same offset, so ties are the
      // normal case. `sorted(by:)` is not documented as stable, so the input
      // position is the tie-break; that reproduces a stable sort by start time
      // and keeps export byte-identical between runs.
      .enumerated()
      .sorted { ($0.element.startSeconds, $0.offset) < ($1.element.startSeconds, $1.offset) }
      .map(\.element)
    return
      ordered
      .map { segment in
        let label = segment.source == .mic ? "MIC" : "SYS"
        let text = segment.text.trimmingCharacters(in: .whitespacesAndNewlines)
        return "[\(label)] \(text)"
      }
      .joined(separator: "\n\n")
  }

  /// Markdown export: a YAML frontmatter envelope followed by the plain-text
  /// body. Empty when there is nothing to export, so callers keep their
  /// existing empty guard.
  static func formatMarkdown(
    session: WispCore.Session?,
    segments: [LiveSegment]
  ) -> String {
    let body = formatPlain(segments: segments)
    guard !body.isEmpty else { return "" }
    let frontmatter = buildFrontmatter(session: session, segments: segments)
    return "\(frontmatter)\n\n\(body)\n"
  }

  /// The YAML frontmatter block, including both `---` fences.
  ///
  /// Field order is fixed and matters: the document opens with the identity
  /// fields and only then the session metadata, so a reader can branch on
  /// `schema` without parsing the rest.
  private static func buildFrontmatter(
    session: WispCore.Session?,
    segments: [LiveSegment]
  ) -> String {
    var lines = [
      "schema: \(yamlString(envelopeSchema))",
      "type: \(yamlString(envelopeType))",
      "source: \(yamlString("wisp"))",
    ]
    let sources = presentSources(segments: segments)
    if let session {
      lines.append("id: \(session.id.rawValue)")
      let title = session.title.trimmingCharacters(in: .whitespacesAndNewlines)
      if !title.isEmpty {
        lines.append("subject: \(yamlString(title))")
      }
      lines.append("time: \(rfc3339(session.startedAt))")
      if let endedAt = session.endedAt {
        lines.append("ended_at: \(rfc3339(endedAt))")
        let duration = Int(endedAt.timeIntervalSince(session.startedAt).rounded(.down))
        if duration >= 0 {
          lines.append("duration_seconds: \(duration)")
        }
      }
    }
    if !sources.isEmpty {
      let list = sources.map(yamlString).joined(separator: ", ")
      lines.append("sources: [\(list)]")
    }
    lines.append("segment_count: \(countNonEmpty(segments: segments))")
    lines.append("datacontenttype: \(yamlString("text/plain"))")
    lines.append("generator: \(yamlString("wisp/\(AppVersion.current)"))")
    return "---\n" + lines.joined(separator: "\n") + "\n---"
  }

  /// Which sources contributed non-empty text, mic before system.
  private static func presentSources(segments: [LiveSegment]) -> [String] {
    var hasMic = false
    var hasSystem = false
    for segment in segments
    where !segment.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
      switch segment.source {
      case .mic: hasMic = true
      case .system: hasSystem = true
      }
    }
    var out: [String] = []
    if hasMic {
      out.append("mic")
    }
    if hasSystem {
      out.append("system")
    }
    return out
  }

  private static func countNonEmpty(segments: [LiveSegment]) -> Int {
    segments.count { !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
  }

  /// RFC 3339 in UTC with an explicit `+00:00` offset. The offset is spelled
  /// out rather than `Z` because the export envelope is a fixed contract.
  static func rfc3339(_ date: Date) -> String {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(secondsFromGMT: 0) ?? .gmt
    let parts = calendar.dateComponents(
      [.year, .month, .day, .hour, .minute, .second],
      from: date
    )
    return String(
      format: "%04d-%02d-%02dT%02d:%02d:%02d+00:00",
      parts.year ?? 0,
      parts.month ?? 0,
      parts.day ?? 0,
      parts.hour ?? 0,
      parts.minute ?? 0,
      parts.second ?? 0
    )
  }

  /// Quote a YAML scalar only when the bare form would be ambiguous.
  ///
  /// Hand-rolled rather than a general encoder: the envelope is a fixed set
  /// of single-line scalars, and a title containing `:` must survive.
  private static func yamlString(_ value: String) -> String {
    let needsQuoting =
      value.isEmpty
      || value.contains(":")
      || value.contains("#")
      || value.contains("\"")
      || value.contains("'")
      || value.contains("\n")
      || value.contains("\t")
      || value.hasPrefix(" ")
      || value.hasSuffix(" ")
      || value.hasPrefix("-")
      || value.hasPrefix("?")
      || value.hasPrefix("&")
      || value.hasPrefix("*")
      || value.hasPrefix("!")
      || value.hasPrefix("|")
      || value.hasPrefix(">")
      || value.hasPrefix("%")
      || value.hasPrefix("@")
      || value.hasPrefix("`")
      || value.hasPrefix("[")
      || value.hasPrefix("]")
      || value.hasPrefix("{")
      || value.hasPrefix("}")
      || value.hasPrefix(",")
      || value.first?.isNumber == true
      || ["true", "false", "null", "yes", "no", "on", "off"]
        .contains(value.lowercased())
    guard needsQuoting else { return value }
    let escaped =
      value
      .replacingOccurrences(of: "\\", with: "\\\\")
      .replacingOccurrences(of: "\"", with: "\\\"")
      .replacingOccurrences(of: "\n", with: "\\n")
      .replacingOccurrences(of: "\t", with: "\\t")
    return "\"\(escaped)\""
  }

  /// Turn a session title into a safe default filename (no extension).
  static func sanitizeFilename(_ name: String) -> String {
    let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty else { return "transcript" }
    var out = ""
    out.reserveCapacity(trimmed.count)
    for character in trimmed {
      if character.isASCII, character.isLetter || character.isNumber {
        out.append(character)
      } else if character == "-" || character == "_" {
        out.append(character)
      } else if character.isWhitespace {
        if !out.hasSuffix("_") {
          out.append("_")
        }
      } else {
        out.append("_")
      }
    }
    let collapsed = out.trimmingCharacters(in: CharacterSet(charactersIn: "_"))
    return collapsed.isEmpty ? "transcript" : collapsed
  }

  /// Suggested export basename for a live or historical session view.
  static func suggestedExportName(title: String?, fallback: String) -> String {
    sanitizeFilename(title ?? fallback)
  }

  /// Copy the transcript text for `segments` to the pasteboard. Returns
  /// false when there is nothing to copy.
  static func copyToClipboard(segments: [LiveSegment]) -> Bool {
    let text = formatPlain(segments: segments)
    guard !text.isEmpty else { return false }
    let pasteboard = NSPasteboard.general
    pasteboard.clearContents()
    pasteboard.setString(text, forType: .string)
    return true
  }

  /// Ask for a path and write `text` to it, then reveal the file.
  ///
  /// The Markdown is built by the caller so no model state crosses into the
  /// save panel's completion. Returns false when there is nothing to export.
  @MainActor
  static func export(
    text: String,
    suggestedName: String,
    onError: @escaping @MainActor (String) -> Void
  ) -> Bool {
    guard !text.isEmpty else { return false }
    let panel = NSSavePanel()
    if let markdown = UTType(filenameExtension: "md") {
      panel.allowedContentTypes = [markdown]
    }
    panel.canCreateDirectories = true
    panel.nameFieldStringValue = "\(sanitizeFilename(suggestedName)).md"
    panel.directoryURL = defaultExportDirectory()
    panel.begin { response in
      guard response == .OK, let url = panel.url else { return }
      do {
        try Data(text.utf8).write(to: url, options: .atomic)
      } catch {
        Task { @MainActor in onError("\(error)") }
        return
      }
      Task { @MainActor in NSWorkspace.shared.activateFileViewerSelecting([url]) }
    }
    return true
  }

  /// `~/Downloads` when it exists, else `$HOME`, else the temp directory.
  static func defaultExportDirectory() -> URL {
    if let home = ProcessInfo.processInfo.environment["HOME"], !home.isEmpty {
      let homeURL = URL(fileURLWithPath: home, isDirectory: true)
      var isDirectory: ObjCBool = false
      let downloads = homeURL.appendingPathComponent("Downloads", isDirectory: true)
      if FileManager.default.fileExists(atPath: downloads.path, isDirectory: &isDirectory),
        isDirectory.boolValue
      {
        return downloads
      }
      return homeURL
    }
    return FileManager.default.temporaryDirectory
  }
}

/// The app's own version string, from the bundle when one exists.
enum AppVersion {
  static var current: String {
    (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String)
      ?? fallback
  }

  /// Audio engine version, shown beside the app version in About.
  static let audioKit = "0.1.0"

  /// Used by `swift run`, where there is no bundle to read. Resolving the
  /// checked-out `VERSION` keeps the exported `generator:` field identical to
  /// what the packaged app writes; a build with neither falls back to zero.
  static let fallback: String =
    (try? String(
      contentsOf: URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("VERSION"),
      encoding: .utf8
    ))?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "0.0.0"
}
