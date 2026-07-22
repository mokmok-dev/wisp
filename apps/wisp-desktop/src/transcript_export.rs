//! Format in-memory transcript segments for clipboard copy and file export.

use std::path::PathBuf;

use gpui::{App, ClipboardItem};
use serde::Serialize;
use wisp_audiokit::SourceLabel;
use wisp_core::Session as StoredSession;

use crate::app::Segment;

/// CloudEvents-inspired `type` for exported transcripts. Fixed value that
/// identifies what the document is, independent of any single session.
const TRANSCRIPT_TYPE: &str = "dev.mokmok.wisp.transcript";

/// Envelope schema identifier. Bump the version when the frontmatter shape
/// changes so downstream parsers can branch on it.
const TRANSCRIPT_SCHEMA: &str = "wisp.transcript/v1";

/// Plain-text transcript with one line per segment, sorted by start time.
///
/// Each non-empty segment becomes `[MIC] …` or `[SYS] …` so pasted text
/// stays readable outside Wisp.
pub fn format_transcript_plain(segments: &[Segment]) -> String {
    let mut ordered: Vec<&Segment> = segments
        .iter()
        .filter(|seg| !seg.text.trim().is_empty())
        .collect();
    ordered.sort_by(|a, b| {
        a.start_seconds
            .partial_cmp(&b.start_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    ordered
        .into_iter()
        .map(|seg| {
            let label = match seg.source {
                SourceLabel::Mic => "MIC",
                SourceLabel::System => "SYS",
            };
            format!("[{label}] {}", seg.text.trim())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Full Markdown export: a YAML frontmatter envelope followed by the
/// plain-text transcript body.
///
/// The envelope borrows the *essence* of a CloudEvents envelope — a small,
/// standard set of "what / when / who produced this" attributes (`id`,
/// `type`, `source`, `time`, `subject`) — without adopting the full
/// messaging spec. Returns an empty string when there is nothing to export,
/// so callers can keep their existing empty-guard.
pub fn format_transcript_markdown(
    session: Option<&StoredSession>,
    segments: &[Segment],
) -> String {
    let body = format_transcript_plain(segments);
    if body.is_empty() {
        return String::new();
    }
    let frontmatter = build_frontmatter(session, segments);
    format!("{frontmatter}\n\n{body}\n")
}

/// The CloudEvents-inspired export envelope, serialized as YAML frontmatter.
///
/// Field declaration order is the emitted order. `Option` fields are skipped
/// entirely when absent (e.g. a still-recording session without an `id`).
#[derive(Serialize)]
struct TranscriptEnvelope<'a> {
    /// Envelope schema + version, so downstream parsers can branch on shape.
    schema: &'static str,
    /// CloudEvents `type`: what the document is, independent of any session.
    #[serde(rename = "type")]
    event_type: &'static str,
    /// CloudEvents `source`: a stable, machine-independent producer id.
    /// Deliberately no hostname, absolute paths, or WAV locations, to honour
    /// Wisp's offline / privacy-first promise.
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    /// CloudEvents `time`: session start, RFC 3339.
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<&'a str>,
    segment_count: usize,
    datacontenttype: &'static str,
    generator: String,
}

/// Build the YAML frontmatter block (including the enclosing `---` fences).
fn build_frontmatter(
    session: Option<&StoredSession>,
    segments: &[Segment],
) -> String {
    let mut envelope = TranscriptEnvelope {
        schema: TRANSCRIPT_SCHEMA,
        event_type: TRANSCRIPT_TYPE,
        source: "wisp",
        id: None,
        subject: None,
        time: None,
        ended_at: None,
        duration_seconds: None,
        sources: present_sources(segments),
        segment_count: count_nonempty(segments),
        datacontenttype: "text/plain",
        generator: format!("wisp/{}", env!("CARGO_PKG_VERSION")),
    };

    if let Some(session) = session {
        envelope.id = Some(session.id.as_i64());

        let title = session.title.trim();
        if !title.is_empty() {
            envelope.subject = Some(title.to_string());
        }

        envelope.time = Some(session.started_at.to_rfc3339());
        if let Some(ended_at) = session.ended_at {
            envelope.ended_at = Some(ended_at.to_rfc3339());
            let duration = (ended_at - session.started_at).num_seconds();
            if duration >= 0 {
                envelope.duration_seconds = Some(duration);
            }
        }
    }

    // Serialization of this fixed struct cannot realistically fail; fall back
    // to an empty body rather than panicking if it somehow does.
    let yaml = serde_norway::to_string(&envelope).unwrap_or_default();
    format!("---\n{yaml}---")
}

/// Which sources actually contributed non-empty text, mic before system.
fn present_sources(segments: &[Segment]) -> Vec<&'static str> {
    let mut has_mic = false;
    let mut has_system = false;
    for seg in segments {
        if seg.text.trim().is_empty() {
            continue;
        }
        match seg.source {
            SourceLabel::Mic => has_mic = true,
            SourceLabel::System => has_system = true,
        }
    }
    let mut out = Vec::new();
    if has_mic {
        out.push("mic");
    }
    if has_system {
        out.push("system");
    }
    out
}

/// Count segments that survive into the body (non-empty after trimming).
fn count_nonempty(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|seg| !seg.text.trim().is_empty())
        .count()
}

/// Copy the transcript to the system clipboard.
pub fn copy_transcript_to_clipboard(
    segments: &[Segment],
    cx: &App,
) -> bool {
    let text = format_transcript_plain(segments);
    if text.is_empty() {
        return false;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    true
}

/// Open the platform save dialog and write the transcript to the chosen path.
///
/// Writes a Markdown file: a YAML frontmatter envelope (built from
/// `session` when available) followed by the transcript body. The
/// clipboard path stays plain text — the envelope is file-only.
pub fn export_transcript(
    segments: Vec<Segment>,
    session: Option<StoredSession>,
    suggested_name: &str,
    cx: &mut App,
) {
    let text = format_transcript_markdown(session.as_ref(), &segments);
    if text.is_empty() {
        return;
    }

    let directory = default_export_directory();
    let suggested = sanitize_filename(suggested_name);
    let suggested = format!("{suggested}.md");
    let rx = cx.prompt_for_new_path(&directory, Some(&suggested));

    cx.spawn(async move |cx| {
        let path = match rx.await {
            Ok(Ok(Some(path))) => path,
            _ => return,
        };
        if let Err(err) = std::fs::write(&path, text.as_bytes()) {
            eprintln!(
                "wisp: failed to export transcript to {}: {err}",
                path.display()
            );
            return;
        }
        let _ = cx.update(|cx| cx.reveal_path(&path));
    })
    .detach();
}

/// Default folder for the save dialog — `~/Downloads` when available.
fn default_export_directory() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let home_path = PathBuf::from(&home);
        let downloads = home_path.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return home_path;
    }
    std::env::temp_dir()
}

/// Turn a session title into a safe default filename (no extension).
fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "transcript".to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if c.is_whitespace() {
            if !out.ends_with('_') {
                out.push('_');
            }
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "transcript".to_string()
    } else {
        out.to_string()
    }
}

/// Suggested export basename for a live or historical session view.
pub fn suggested_export_name(
    title: Option<&str>,
    fallback: &str,
) -> String {
    title
        .map(sanitize_filename)
        .unwrap_or_else(|| sanitize_filename(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Segment;

    fn seg(
        source: SourceLabel,
        start: f64,
        text: &str,
    ) -> Segment {
        Segment {
            source,
            id: 1,
            text: text.into(),
            display_text: text.into(),
            start_seconds: start,
            end_seconds: start + 1.0,
            is_final: true,
        }
    }

    #[test]
    fn formats_segments_in_time_order_with_labels() {
        let segments = vec![
            seg(SourceLabel::System, 2.0, "はい"),
            seg(SourceLabel::Mic, 1.0, "こんにちは"),
        ];
        assert_eq!(
            format_transcript_plain(&segments),
            "[MIC] こんにちは\n\n[SYS] はい"
        );
    }

    #[test]
    fn skips_empty_segments() {
        let segments = vec![seg(SourceLabel::Mic, 0.0, "   ")];
        assert!(format_transcript_plain(&segments).is_empty());
    }

    #[test]
    fn sanitize_filename_replaces_unsafe_chars() {
        assert_eq!(
            sanitize_filename("Meeting 2026/06/02"),
            "Meeting_2026_06_02"
        );
    }

    fn stored_session(title: &str) -> wisp_core::Session {
        use chrono::{TimeZone, Utc};
        wisp_core::Session {
            id: wisp_core::SessionId::from(42),
            started_at: Utc
                .with_ymd_and_hms(2026, 7, 22, 9, 0, 0)
                .single()
                .expect("valid start timestamp"),
            ended_at: Some(
                Utc.with_ymd_and_hms(2026, 7, 22, 9, 47, 12)
                    .single()
                    .expect("valid end timestamp"),
            ),
            title: title.to_string(),
            mic_wav_path: "mic.wav".to_string(),
            system_wav_path: "system.wav".to_string(),
            notes: String::new(),
        }
    }

    /// Split a Markdown export into its parsed frontmatter value and body.
    fn parse_export(md: &str) -> (serde_norway::Value, String) {
        let after_open = md.strip_prefix("---\n").expect("opening fence");
        let end = after_open.find("\n---").expect("closing fence");
        let yaml = &after_open[..end];
        let value: serde_norway::Value =
            serde_norway::from_str(yaml).expect("frontmatter is valid YAML");
        let body = after_open[end..]
            .trim_start_matches('\n')
            .trim_start_matches("---")
            .trim()
            .to_string();
        (value, body)
    }

    #[test]
    fn markdown_export_includes_frontmatter_envelope() {
        let session = stored_session("Weekly sync: Q3");
        let segments = vec![
            seg(SourceLabel::Mic, 1.0, "こんにちは"),
            seg(SourceLabel::System, 2.0, "はい"),
        ];

        let md = format_transcript_markdown(Some(&session), &segments);
        let (env, body) = parse_export(&md);

        assert_eq!(env["schema"].as_str(), Some("wisp.transcript/v1"));
        assert_eq!(env["type"].as_str(), Some("dev.mokmok.wisp.transcript"));
        assert_eq!(env["source"].as_str(), Some("wisp"));
        assert_eq!(env["id"].as_i64(), Some(42));
        // A title with a colon must survive the YAML round-trip intact.
        assert_eq!(env["subject"].as_str(), Some("Weekly sync: Q3"));
        assert_eq!(env["time"].as_str(), Some("2026-07-22T09:00:00+00:00"));
        assert_eq!(env["ended_at"].as_str(), Some("2026-07-22T09:47:12+00:00"));
        assert_eq!(env["duration_seconds"].as_i64(), Some(2832));
        assert_eq!(env["segment_count"].as_i64(), Some(2));
        assert_eq!(env["datacontenttype"].as_str(), Some("text/plain"));

        let sources: Vec<&str> = env["sources"]
            .as_sequence()
            .expect("sources sequence")
            .iter()
            .filter_map(serde_norway::Value::as_str)
            .collect();
        assert_eq!(sources, vec!["mic", "system"]);

        assert_eq!(body, "[MIC] こんにちは\n\n[SYS] はい");
    }

    #[test]
    fn markdown_export_without_session_omits_session_fields() {
        let segments = vec![seg(SourceLabel::Mic, 0.0, "hi")];
        let md = format_transcript_markdown(None, &segments);
        let (env, _) = parse_export(&md);

        assert_eq!(env["schema"].as_str(), Some("wisp.transcript/v1"));
        assert_eq!(env["segment_count"].as_i64(), Some(1));
        assert!(env.get("id").is_none());
        assert!(env.get("subject").is_none());
        assert!(env.get("time").is_none());
    }

    #[test]
    fn markdown_export_is_empty_when_no_body() {
        let segments = vec![seg(SourceLabel::Mic, 0.0, "   ")];
        assert!(
            format_transcript_markdown(Some(&stored_session("x")), &segments).is_empty()
        );
    }
}
