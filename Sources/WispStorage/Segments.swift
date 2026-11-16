import Foundation
import WispCore

/// Read/write operations for the `segments` table.
public struct Segments {
  let connection: Connection

  /// Append a new segment to a session. `createdAt` is set to "now" in UTC;
  /// everything else comes from `new`.
  ///
  /// Throws on insertion failure, including unique-constraint violations on
  /// `(session_id, source, segment_index)`.
  @discardableResult
  public func append(_ new: NewSegment) throws -> SegmentID {
    try connection.run(
      """
      INSERT INTO segments \
      (session_id, source, segment_index, start_seconds, end_seconds, \
      text, speaker_label, created_at) \
      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
      """,
      [
        .integer(new.sessionID.rawValue),
        .text(new.source.stableString),
        .integer(Int64(new.segmentIndex)),
        .real(new.startSeconds),
        .real(new.endSeconds),
        .text(new.text),
        new.speakerLabel.map { SQLiteValue.text($0) } ?? .null,
        .text(SQLiteClock.string(from: Date())),
      ]
    )
    return SegmentID(connection.lastInsertRowID)
  }

  /// All segments for a session, ordered by `(start_seconds, id)` so mic and
  /// system tracks interleave by wall-clock time.
  public func listBySession(_ sessionID: SessionID) throws -> [Segment] {
    try connection.query(
      """
      SELECT id, session_id, source, segment_index, start_seconds, end_seconds, \
             text, speaker_label, created_at \
      FROM segments WHERE session_id = ?1 \
      ORDER BY start_seconds, id
      """,
      [.integer(sessionID.rawValue)],
      row: Self.rowToSegment
    )
  }

  /// Full-text search across every segment in the database. Returns matching
  /// segments ordered by FTS5 relevance (`bm25`). `query` is passed verbatim
  /// to FTS5 — the caller is responsible for any escaping.
  public func search(_ query: String, limit: UInt32) throws -> [Segment] {
    try connection.query(
      """
      SELECT s.id, s.session_id, s.source, s.segment_index, s.start_seconds, \
             s.end_seconds, s.text, s.speaker_label, s.created_at \
      FROM segments s \
      JOIN segments_fts f ON f.rowid = s.id \
      WHERE segments_fts MATCH ?1 \
      ORDER BY bm25(segments_fts) \
      LIMIT ?2
      """,
      [.text(query), .integer(Int64(limit))],
      row: Self.rowToSegment
    )
  }

  /// Search user-typed text across every segment. Whitespace-separated terms
  /// are joined with an implicit AND.
  ///
  /// Terms of three or more characters go through the trigram FTS index.
  /// Short terms are matched as substrings instead, because the trigram
  /// tokenizer cannot index a token shorter than three characters and a
  /// Japanese word is often one or two.
  public func searchText(_ text: String, limit: UInt32) throws -> [Segment] {
    let terms = text.split(whereSeparator: \.isWhitespace).map(String.init)
    if terms.isEmpty {
      return []
    }
    if terms.allSatisfy({ $0.count >= 3 }) {
      let query =
        terms
        .map { "\"" + $0.replacingOccurrences(of: "\"", with: "\"\"") + "\"" }
        .joined(separator: " ")
      return try search(query, limit: limit)
    }
    return try searchSubstring(terms, limit: limit)
  }

  private func searchSubstring(_ terms: [String], limit: UInt32) throws -> [Segment] {
    let clauses = (1...terms.count)
      .map { "text LIKE ?\($0) ESCAPE '\\'" }
      .joined(separator: " AND ")
    let sql = """
      SELECT id, session_id, source, segment_index, start_seconds, end_seconds, \
             text, speaker_label, created_at \
      FROM segments WHERE \(clauses) \
      ORDER BY start_seconds, id LIMIT ?\(terms.count + 1)
      """
    var bindings = terms.map { SQLiteValue.text("%\(escapeLike($0))%") }
    bindings.append(.integer(Int64(limit)))
    return try connection.query(sql, bindings, row: Self.rowToSegment)
  }

  private static func rowToSegment(_ row: Row) throws -> Segment {
    let sourceText = row.text(at: 2)
    guard let source = SourceLabel(rawValue: sourceText) else {
      throw StorageError.invalidSourceLabel(sourceText)
    }
    return try Segment(
      id: SegmentID(row.integer(at: 0)),
      sessionID: SessionID(row.integer(at: 1)),
      source: source,
      segmentIndex: UInt32(clamping: row.integer(at: 3)),
      startSeconds: row.double(at: 4),
      endSeconds: row.double(at: 5),
      text: row.text(at: 6),
      speakerLabel: row.optionalText(at: 7),
      createdAt: row.date(at: 8)
    )
  }
}

private func escapeLike(_ term: String) -> String {
  var escaped = ""
  escaped.reserveCapacity(term.count)
  for character in term {
    if character == "\\" || character == "%" || character == "_" {
      escaped.append("\\")
    }
    escaped.append(character)
  }
  return escaped
}
