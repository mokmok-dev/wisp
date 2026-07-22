//! Argument parsing and stable cursor pagination for transcript tool calls.

use serde_json::Value;

pub const DEFAULT_LOOPBACK_SECONDS: f64 = 600.0;
pub const MAX_CURSOR_LENGTH: usize = 256;
pub const MAX_PAGE_LIMIT: usize = 500;

#[derive(Debug, Clone)]
pub struct ToolArguments {
    pub(super) question: String,
    loopback_seconds: Option<f64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

impl ToolArguments {
    pub(super) fn from_params(params: &Value) -> Result<Self, String> {
        let arguments = match params.get("arguments") {
            None => &Value::Null,
            Some(arguments @ Value::Object(_)) => arguments,
            Some(_) => return Err("arguments must be an object".to_owned()),
        };
        let question = match arguments.get("question") {
            Some(value) => value
                .as_str()
                .ok_or_else(|| "question must be a string".to_owned())?
                .to_owned(),
            None => "いまの話ってどういうこと?".to_owned(),
        };
        let loopback_seconds = arguments
            .get("loopback_seconds")
            .map(|value| {
                let seconds = value
                    .as_f64()
                    .ok_or_else(|| "loopback_seconds must be a number".to_owned())?;
                if !seconds.is_finite() || seconds < 0.0 {
                    return Err("loopback_seconds must be a finite, non-negative number".to_owned());
                }
                Ok(seconds.abs())
            })
            .transpose()?;
        let cursor = arguments
            .get("cursor")
            .map(|value| {
                let cursor = value
                    .as_str()
                    .ok_or_else(|| "cursor must be a string".to_owned())?;
                if cursor.is_empty() || cursor.len() > MAX_CURSOR_LENGTH {
                    return Err(format!(
                        "cursor must contain between 1 and {MAX_CURSOR_LENGTH} bytes"
                    ));
                }
                Ok(cursor.to_owned())
            })
            .transpose()?;
        let limit = match arguments.get("limit") {
            Some(value) => {
                let limit = value
                    .as_u64()
                    .and_then(|limit| usize::try_from(limit).ok())
                    .ok_or_else(|| "limit must be an integer".to_owned())?;
                if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
                    return Err(format!("limit must be between 1 and {MAX_PAGE_LIMIT}"));
                }
                Some(limit)
            },
            None => None,
        };
        if cursor.is_some() && limit.is_none() {
            return Err("limit is required when cursor is provided".to_owned());
        }
        Ok(Self {
            question,
            loopback_seconds,
            cursor,
            limit,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorView {
    Library,
    LiveSession,
    History,
}

impl CursorView {
    fn from_snapshot(snapshot: &Value) -> Option<Self> {
        match snapshot.get("view").and_then(Value::as_str) {
            Some("library") => Some(Self::Library),
            Some("live_session") => Some(Self::LiveSession),
            Some("history") => Some(Self::History),
            _ => None,
        }
    }

    const fn as_cursor_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::LiveSession => "live",
            Self::History => "history",
        }
    }

    fn from_cursor_str(value: &str) -> Option<Self> {
        match value {
            "library" => Some(Self::Library),
            "live" => Some(Self::LiveSession),
            "history" => Some(Self::History),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TranscriptCursor {
    view: CursorView,
    session_id: i64,
    anchor_end_seconds: f64,
    loopback_seconds: f64,
    snapshot_end_exclusive: usize,
    before_index: usize,
}

impl TranscriptCursor {
    fn encode(self) -> String {
        format!(
            "v1:{}:{}:{:016x}:{:016x}:{}:{}",
            self.view.as_cursor_str(),
            self.session_id,
            self.anchor_end_seconds.to_bits(),
            self.loopback_seconds.to_bits(),
            self.snapshot_end_exclusive,
            self.before_index
        )
    }

    fn decode(cursor: &str) -> Result<Self, String> {
        let parts = cursor.split(':').collect::<Vec<_>>();
        let [
            version,
            view,
            session_id,
            anchor_end_seconds,
            loopback_seconds,
            snapshot_end_exclusive,
            before_index,
        ] = parts.as_slice()
        else {
            return Err(
                "Invalid cursor. Start a new pagination request without cursor.".to_owned(),
            );
        };
        if *version != "v1" {
            return Err(
                "Invalid cursor. Start a new pagination request without cursor.".to_owned(),
            );
        }
        let view =
            CursorView::from_cursor_str(view).ok_or_else(|| "Invalid cursor view".to_owned())?;
        let session_id = session_id
            .parse::<i64>()
            .map_err(|_| "Invalid cursor session".to_owned())?;
        let anchor_end_seconds = parse_cursor_seconds(anchor_end_seconds)?;
        let loopback_seconds = parse_cursor_seconds(loopback_seconds)?;
        if anchor_end_seconds < 0.0 || loopback_seconds < 0.0 {
            return Err("Invalid cursor time window".to_owned());
        }
        let anchor_end_seconds = anchor_end_seconds.abs();
        let loopback_seconds = loopback_seconds.abs();
        let snapshot_end_exclusive = snapshot_end_exclusive
            .parse::<usize>()
            .map_err(|_| "Invalid cursor snapshot boundary".to_owned())?;
        let before_index = before_index
            .parse::<usize>()
            .map_err(|_| "Invalid cursor page boundary".to_owned())?;
        if before_index > snapshot_end_exclusive {
            return Err("Invalid cursor boundaries".to_owned());
        }
        Ok(Self {
            view,
            session_id,
            anchor_end_seconds,
            loopback_seconds,
            snapshot_end_exclusive,
            before_index,
        })
    }
}

fn parse_cursor_seconds(value: &str) -> Result<f64, String> {
    let bits = u64::from_str_radix(value, 16).map_err(|_| "Invalid cursor time".to_owned())?;
    let seconds = f64::from_bits(bits);
    if !seconds.is_finite() {
        return Err("Invalid cursor time".to_owned());
    }
    Ok(seconds)
}

#[derive(Debug, Clone, Copy)]
struct PaginationWindow {
    view: Option<CursorView>,
    session_id: Option<i64>,
    anchor_end_seconds: f64,
    loopback_seconds: f64,
    snapshot_end_exclusive: usize,
    before_index: usize,
}

impl PaginationWindow {
    fn from_snapshot(
        snapshot: &Value,
        segments: &[Value],
        arguments: &ToolArguments,
    ) -> Result<Self, String> {
        let session_id = snapshot.get("session_id").and_then(Value::as_i64);
        let view = CursorView::from_snapshot(snapshot);
        let latest_end_seconds = segments
            .iter()
            .filter(|segment| segment_has_text(segment))
            .map(segment_end_seconds)
            .fold(0.0_f64, f64::max);
        let cursor = arguments
            .cursor
            .as_deref()
            .map(TranscriptCursor::decode)
            .transpose()?;
        let Some(cursor) = cursor else {
            return Ok(Self {
                view,
                session_id,
                anchor_end_seconds: latest_end_seconds,
                loopback_seconds: arguments
                    .loopback_seconds
                    .unwrap_or(DEFAULT_LOOPBACK_SECONDS),
                snapshot_end_exclusive: segments.len(),
                before_index: segments.len(),
            });
        };
        if Some(cursor.session_id) != session_id || Some(cursor.view) != view {
            return Err(
                "The cursor belongs to a different Wisp session or view. Start again without cursor."
                    .to_owned(),
            );
        }
        if cursor.snapshot_end_exclusive > segments.len()
            || cursor.before_index > cursor.snapshot_end_exclusive
            || cursor.anchor_end_seconds > latest_end_seconds
        {
            return Err("The cursor is no longer valid. Start again without cursor.".to_owned());
        }
        if arguments
            .loopback_seconds
            .is_some_and(|seconds| seconds.to_bits() != cursor.loopback_seconds.to_bits())
        {
            return Err("loopback_seconds cannot change while continuing from a cursor".to_owned());
        }
        Ok(Self {
            view,
            session_id,
            anchor_end_seconds: cursor.anchor_end_seconds,
            loopback_seconds: cursor.loopback_seconds,
            snapshot_end_exclusive: cursor.snapshot_end_exclusive,
            before_index: cursor.before_index,
        })
    }

    fn next_cursor(
        self,
        remaining_segments: usize,
        before_index: Option<usize>,
    ) -> Result<Option<String>, String> {
        if remaining_segments == 0 {
            return Ok(None);
        }
        let session_id = self.session_id.ok_or_else(|| {
            "Cannot paginate a transcript without a session id. Start or open a session first."
                .to_owned()
        })?;
        let view = self
            .view
            .ok_or_else(|| "Cannot paginate an unknown Wisp view".to_owned())?;
        let before_index =
            before_index.ok_or_else(|| "Invalid empty transcript page".to_owned())?;
        Ok(Some(
            TranscriptCursor {
                view,
                session_id,
                anchor_end_seconds: self.anchor_end_seconds,
                loopback_seconds: self.loopback_seconds,
                snapshot_end_exclusive: self.snapshot_end_exclusive,
                before_index,
            }
            .encode(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub(super) segments: Vec<Value>,
    pub(super) loopback_seconds: f64,
    pub(super) anchor_end_seconds: f64,
    pub(super) total_segments: usize,
    pub(super) remaining_segments: usize,
    pub(super) limit: Option<usize>,
    pub(super) next_cursor: Option<String>,
}

impl TranscriptPage {
    pub(super) const fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }
}

pub fn paginate_transcript(
    snapshot: &Value,
    arguments: &ToolArguments,
) -> Result<TranscriptPage, String> {
    let segments = snapshot
        .get("segments")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let window = PaginationWindow::from_snapshot(snapshot, segments, arguments)?;
    let window_start_seconds = (window.anchor_end_seconds - window.loopback_seconds).max(0.0);
    let eligible = segments
        .iter()
        .enumerate()
        .filter(|(index, _)| *index < window.snapshot_end_exclusive)
        .filter(|(_, segment)| segment_has_text(segment))
        .filter(|(_, segment)| {
            segment_end_seconds(segment) >= window_start_seconds
                && segment_start_seconds(segment) <= window.anchor_end_seconds
        })
        .collect::<Vec<_>>();
    let total_segments = eligible.len();
    let available_end = eligible.partition_point(|(index, _)| *index < window.before_index);
    let page_start = arguments
        .limit
        .map_or(0, |limit| available_end.saturating_sub(limit));
    let page = &eligible[page_start..available_end];
    let next_cursor = window.next_cursor(page_start, page.first().map(|(index, _)| *index))?;
    Ok(TranscriptPage {
        segments: page.iter().map(|(_, segment)| (*segment).clone()).collect(),
        loopback_seconds: window.loopback_seconds,
        anchor_end_seconds: window.anchor_end_seconds,
        total_segments,
        remaining_segments: page_start,
        limit: arguments.limit,
        next_cursor,
    })
}

fn segment_has_text(segment: &Value) -> bool {
    segment
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn segment_start_seconds(segment: &Value) -> f64 {
    segment
        .get("start_seconds")
        .and_then(Value::as_f64)
        .filter(|seconds| seconds.is_finite())
        .unwrap_or(0.0)
}

fn segment_end_seconds(segment: &Value) -> f64 {
    segment
        .get("end_seconds")
        .and_then(Value::as_f64)
        .filter(|seconds| seconds.is_finite())
        .unwrap_or_else(|| segment_start_seconds(segment))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ToolArguments, TranscriptPage, paginate_transcript};

    fn segment(
        text: &str,
        start_seconds: f64,
        end_seconds: f64,
    ) -> Value {
        json!({
            "source": "mic",
            "text": text,
            "start_seconds": start_seconds,
            "end_seconds": end_seconds,
            "is_final": true
        })
    }

    fn snapshot(
        session_id: i64,
        segments: &[Value],
    ) -> Value {
        json!({
            "view": "live_session",
            "state": "recording",
            "session_id": session_id,
            "title": "demo",
            "segments": segments
        })
    }

    fn arguments(value: &Value) -> ToolArguments {
        ToolArguments::from_params(&json!({"arguments": value})).expect("valid arguments")
    }

    fn page_texts(page: &TranscriptPage) -> Vec<&str> {
        page.segments
            .iter()
            .filter_map(|segment| segment.get("text").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn default_loopback_includes_boundary_and_ignores_blank_segments() {
        let snapshot = snapshot(
            7,
            &[
                segment("too old", 300.0, 399.9),
                segment("boundary", 390.0, 400.0),
                segment("recent", 999.0, 1_000.0),
                segment("   ", 1_000.0, 1_000.0),
            ],
        );
        let page =
            paginate_transcript(&snapshot, &arguments(&json!({"question": "q"}))).expect("page");

        assert!((page.loopback_seconds - 600.0).abs() < f64::EPSILON);
        assert_eq!(page_texts(&page), vec!["boundary", "recent"]);
        assert_eq!(page.total_segments, 2);
        assert_eq!(page.limit, None);
        assert!(!page.has_more());

        let custom = paginate_transcript(
            &snapshot,
            &arguments(&json!({"question": "q", "loopback_seconds": 1})),
        )
        .expect("custom window");
        assert_eq!(page_texts(&custom), vec!["recent"]);
    }

    #[test]
    fn cursor_pages_by_display_order_and_stays_stable_after_live_append() {
        let mut snapshot = snapshot(
            7,
            &[
                segment("A", 90.0, 100.0),
                segment("", 150.0, 160.0),
                segment("B", 390.0, 400.0),
                segment("C", 190.0, 200.0),
                segment("D", 490.0, 500.0),
                segment("E", 290.0, 300.0),
            ],
        );
        let first = paginate_transcript(
            &snapshot,
            &arguments(&json!({
                "question": "q",
                "loopback_seconds": 1_000,
                "limit": 2
            })),
        )
        .expect("first page");
        assert_eq!(page_texts(&first), vec!["D", "E"]);
        assert_eq!(first.remaining_segments, 3);
        let first_cursor = first.next_cursor.expect("first cursor");

        snapshot["segments"]
            .as_array_mut()
            .expect("segments")
            .push(segment("F", 590.0, 600.0));
        let second = paginate_transcript(
            &snapshot,
            &arguments(&json!({
                "question": "q",
                "cursor": first_cursor,
                "limit": 2
            })),
        )
        .expect("second page");
        assert_eq!(page_texts(&second), vec!["B", "C"]);
        assert!((second.loopback_seconds - 1_000.0).abs() < f64::EPSILON);
        assert_eq!(second.total_segments, 5);
        let second_cursor = second.next_cursor.expect("second cursor");

        let third = paginate_transcript(
            &snapshot,
            &arguments(&json!({
                "question": "q",
                "cursor": second_cursor,
                "limit": 2
            })),
        )
        .expect("third page");
        assert_eq!(page_texts(&third), vec!["A"]);
        assert!(!third.has_more());
    }

    #[test]
    fn invalid_pagination_arguments_are_rejected() {
        assert!(
            ToolArguments::from_params(
                &json!({"arguments": {"question": "q", "loopback_seconds": -1}})
            )
            .is_err()
        );
        assert!(
            ToolArguments::from_params(&json!({"arguments": {"question": "q", "limit": 0}}))
                .is_err()
        );
        assert!(
            ToolArguments::from_params(&json!({"arguments": {"question": "q", "limit": 501}}))
                .is_err()
        );
        assert!(
            ToolArguments::from_params(
                &json!({"arguments": {"question": "q", "loopback_seconds": "600"}})
            )
            .is_err()
        );
        assert!(
            ToolArguments::from_params(&json!({"arguments": {"question": "q", "limit": 1.5}}))
                .is_err()
        );
        assert!(
            ToolArguments::from_params(
                &json!({"arguments": {"question": "q", "cursor": "opaque"}})
            )
            .is_err()
        );
        assert!(ToolArguments::from_params(&json!({"arguments": "bad"})).is_err());
        assert!(ToolArguments::from_params(&json!({"arguments": null})).is_err());
    }

    #[test]
    fn malformed_and_stale_cursors_are_rejected() {
        let current_snapshot = snapshot(7, &[segment("A", 0.0, 1.0), segment("B", 1.0, 2.0)]);
        assert!(
            paginate_transcript(
                &current_snapshot,
                &arguments(&json!({"question": "q", "cursor": "bad", "limit": 1}))
            )
            .is_err()
        );
        let first = paginate_transcript(
            &current_snapshot,
            &arguments(&json!({
                "question": "q",
                "loopback_seconds": 10,
                "limit": 1
            })),
        )
        .expect("first page");
        let cursor = first.next_cursor.expect("cursor");
        assert!(
            paginate_transcript(
                &current_snapshot,
                &arguments(&json!({
                    "question": "q",
                    "loopback_seconds": 20,
                    "cursor": cursor,
                    "limit": 1
                }))
            )
            .is_err()
        );

        let first = paginate_transcript(
            &current_snapshot,
            &arguments(&json!({"question": "q", "limit": 1})),
        )
        .expect("first page");
        let cursor = first.next_cursor.expect("cursor");
        let mut different_view = current_snapshot;
        different_view["view"] = json!("history");
        assert!(
            paginate_transcript(
                &different_view,
                &arguments(&json!({
                    "question": "q",
                    "cursor": cursor,
                    "limit": 1
                }))
            )
            .is_err()
        );
        let different_session = snapshot(8, &[segment("A", 0.0, 1.0)]);
        assert!(
            paginate_transcript(
                &different_session,
                &arguments(&json!({"question": "q", "cursor": cursor, "limit": 1}))
            )
            .is_err()
        );

        let zero_snapshot = snapshot(9, &[segment("A", 0.0, 2.0), segment("B", 1.0, 2.0)]);
        let first = paginate_transcript(
            &zero_snapshot,
            &arguments(&json!({
                "question": "q",
                "loopback_seconds": -0.0,
                "limit": 1
            })),
        )
        .expect("negative zero page");
        let cursor = first.next_cursor.expect("negative zero cursor");
        assert!(
            paginate_transcript(
                &zero_snapshot,
                &arguments(&json!({
                    "question": "q",
                    "loopback_seconds": 0,
                    "cursor": cursor,
                    "limit": 1
                }))
            )
            .is_ok()
        );
    }
}
