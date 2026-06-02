//! Format in-memory transcript segments for clipboard copy and file export.

use std::path::PathBuf;

use gpui::{App, ClipboardItem};
use wisp_audiokit::SourceLabel;

use crate::app::Segment;

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
pub fn export_transcript(
    segments: Vec<Segment>,
    suggested_name: &str,
    cx: &mut App,
) {
    let text = format_transcript_plain(&segments);
    if text.is_empty() {
        return;
    }

    let directory = default_export_directory();
    let suggested = sanitize_filename(suggested_name);
    let suggested = format!("{suggested}.txt");
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
}
