use chrono::Utc;
use rusqlite::{Connection, params};
use wisp_core::{NewSegment, Segment, SegmentId, SessionId, SourceLabel};

use crate::error::Result;

/// Read/write operations for the `segments` table.
pub struct Segments<'a> {
    conn: &'a Connection,
}

/// One hit from a full-text search.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub segment: Segment,
}

impl<'a> Segments<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Append a new segment to a session. `created_at` is set to "now" in
    /// UTC; everything else comes from `new`.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on insertion failure
    /// (including unique-constraint violations on `(session_id, source,
    /// segment_index)`).
    pub fn append(
        &self,
        new: &NewSegment,
    ) -> Result<SegmentId> {
        let now = Utc::now();
        self.conn.execute(
            "INSERT INTO segments \
             (session_id, source, segment_index, start_seconds, end_seconds, text, speaker_label, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                new.session_id.as_i64(),
                new.source.as_str(),
                new.segment_index,
                new.start_seconds,
                new.end_seconds,
                new.text,
                new.speaker_label,
                now,
            ],
        )?;
        Ok(SegmentId::from(self.conn.last_insert_rowid()))
    }

    /// All segments for a session, ordered by `(start_seconds, id)` so
    /// mic + system tracks interleave by wall-clock time.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on query failure or
    /// [`StorageError::SourceLabel`] if the DB holds a `source` value
    /// outside `{"mic","system"}` (only possible via direct DB edits).
    pub fn list_by_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Segment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, source, segment_index, start_seconds, end_seconds, \
                    text, speaker_label, created_at \
             FROM segments WHERE session_id = ?1 \
             ORDER BY start_seconds, id",
        )?;
        let mut rows = stmt.query([session_id.as_i64()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row_to_segment(row)?);
        }
        Ok(out)
    }

    /// Full-text search across every segment in the database. Returns hits
    /// ordered by FTS5 relevance (`bm25`). `query` is passed verbatim to
    /// FTS5 — the caller is responsible for any escaping.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on query failure or
    /// [`StorageError::SourceLabel`] (see [`Self::list_by_session`]).
    pub fn search(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.session_id, s.source, s.segment_index, s.start_seconds, \
                    s.end_seconds, s.text, s.speaker_label, s.created_at \
             FROM segments s \
             JOIN segments_fts f ON f.rowid = s.id \
             WHERE segments_fts MATCH ?1 \
             ORDER BY bm25(segments_fts) \
             LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![query, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(SearchHit {
                segment: row_to_segment(row)?,
            });
        }
        Ok(out)
    }
}

fn row_to_segment(row: &rusqlite::Row<'_>) -> Result<Segment> {
    let source_str: String = row.get(2)?;
    let source = source_str.parse::<SourceLabel>()?;
    Ok(Segment {
        id: SegmentId::from(row.get::<_, i64>(0)?),
        session_id: SessionId::from(row.get::<_, i64>(1)?),
        source,
        segment_index: row.get(3)?,
        start_seconds: row.get(4)?,
        end_seconds: row.get(5)?,
        text: row.get(6)?,
        speaker_label: row.get(7)?,
        created_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use wisp_core::{NewSegment, NewSession, SourceLabel};

    use crate::Storage;

    fn fresh_session(storage: &Storage) -> wisp_core::SessionId {
        storage
            .sessions()
            .create(&NewSession {
                started_at: Utc.with_ymd_and_hms(2026, 5, 28, 10, 0, 0).unwrap(),
                title: "test".into(),
                mic_wav_path: "s/mic.wav".into(),
                system_wav_path: "s/system.wav".into(),
            })
            .expect("create session")
    }

    fn seg(
        session_id: wisp_core::SessionId,
        source: SourceLabel,
        idx: u32,
        start: f64,
        end: f64,
        text: &str,
    ) -> NewSegment {
        NewSegment {
            session_id,
            source,
            segment_index: idx,
            start_seconds: start,
            end_seconds: end,
            text: text.into(),
            speaker_label: None,
        }
    }

    #[test]
    fn append_then_list_interleaves_by_time() {
        let storage = Storage::open_in_memory().expect("open");
        let sid = fresh_session(&storage);
        let segments = storage.segments();
        segments
            .append(&seg(sid, SourceLabel::Mic, 0, 0.0, 2.0, "hello"))
            .unwrap();
        segments
            .append(&seg(sid, SourceLabel::System, 0, 1.0, 3.0, "hi there"))
            .unwrap();
        segments
            .append(&seg(sid, SourceLabel::Mic, 1, 5.0, 7.0, "let's start"))
            .unwrap();
        let all = segments.list_by_session(sid).expect("list");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text, "hello");
        assert_eq!(all[1].text, "hi there");
        assert_eq!(all[2].text, "let's start");
    }

    #[test]
    fn duplicate_segment_index_within_source_fails() {
        let storage = Storage::open_in_memory().expect("open");
        let sid = fresh_session(&storage);
        let segments = storage.segments();
        segments
            .append(&seg(sid, SourceLabel::Mic, 0, 0.0, 1.0, "a"))
            .unwrap();
        let err = segments
            .append(&seg(sid, SourceLabel::Mic, 0, 0.0, 1.0, "dup"))
            .expect_err("expected unique-constraint violation");
        assert!(matches!(err, crate::StorageError::Sqlite(_)), "got {err:?}");
    }

    #[test]
    fn full_text_search_returns_relevant_segment() {
        let storage = Storage::open_in_memory().expect("open");
        let sid = fresh_session(&storage);
        let segments = storage.segments();
        segments
            .append(&seg(
                sid,
                SourceLabel::Mic,
                0,
                0.0,
                1.0,
                "today's weather is nice",
            ))
            .unwrap();
        segments
            .append(&seg(
                sid,
                SourceLabel::Mic,
                1,
                2.0,
                3.0,
                "let us discuss the roadmap",
            ))
            .unwrap();
        segments
            .append(&seg(
                sid,
                SourceLabel::System,
                0,
                0.5,
                1.5,
                "good morning everyone",
            ))
            .unwrap();
        let hits = segments.search("weather", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].segment.text.contains("weather"));
    }

    #[test]
    fn deleting_session_cascades_to_segments() {
        let storage = Storage::open_in_memory().expect("open");
        let sid = fresh_session(&storage);
        let segments = storage.segments();
        segments
            .append(&seg(sid, SourceLabel::Mic, 0, 0.0, 1.0, "x"))
            .unwrap();
        assert_eq!(segments.list_by_session(sid).unwrap().len(), 1);
        storage.sessions().delete(sid).expect("delete session");
        assert!(segments.list_by_session(sid).unwrap().is_empty());
    }
}
