//! Decides when one app holding the microphone becomes a meeting.
//!
//! The platform probe reports candidates once a second; this module owns the
//! timing rules that separate a call from a voice note, and the snooze that
//! keeps one call from being offered twice.

use std::time::{Duration, Instant};

use wisp_core::{MeetingPolicy, MeetingTool};

/// An offered call stays snoozed until its app has been off the microphone
/// for about a minute, so one call is offered once.
pub const ABSENT_FOR_RESET: Duration = Duration::from_secs(60);

/// A call that drops off one microphone poll and returns right after keeps
/// its time toward the offer.
pub const DROPOUT_GRACE: Duration = Duration::from_secs(2);

/// Observations further apart than this do not make one run: a sleeping Mac
/// or a stalled loop restarts the clock instead of counting the gap.
pub const MAX_OBSERVATION_GAP: Duration = Duration::from_secs(10);

/// A call that has held the microphone long enough to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub tool: MeetingTool,
    /// The tool is set to start recording without an offer.
    pub auto_record: bool,
}

/// What one observation produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// A call became stable.
    Detected(Detection),
    /// A previously offered call left the microphone long enough to reset.
    Ended { tool: MeetingTool },
}

/// How long a condition has held, on the monotonic clock, so a slow loop
/// iteration cannot stretch a threshold.
#[derive(Debug, Clone, Copy)]
struct Run {
    since: Instant,
    last: Instant,
}

impl Run {
    fn start(now: Instant) -> Self {
        Self {
            since: now,
            last: now,
        }
    }

    fn extend(
        &mut self,
        now: Instant,
    ) -> Duration {
        if now.saturating_duration_since(self.last) > MAX_OBSERVATION_GAP {
            self.since = now;
        }
        self.last = now;
        now.saturating_duration_since(self.since)
    }
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    tool: MeetingTool,
    run: Run,
}

#[derive(Debug, Clone, Copy)]
struct Handled {
    tool: MeetingTool,
    last_present: Instant,
}

/// The detection state machine. One instance per running app.
#[derive(Debug, Default)]
pub struct Detector {
    pending: Option<Pending>,
    handled: Option<Handled>,
}

impl Detector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One observation under the user's per-tool policies.
    ///
    /// `active` is the set of tools currently holding the microphone, in any
    /// order. A tool with [`MeetingPolicy::Off`] is never detected and never
    /// offered.
    pub fn observe(
        &mut self,
        now: Instant,
        active: &[MeetingTool],
        policy: impl Fn(MeetingTool) -> MeetingPolicy,
    ) -> Option<Observation> {
        if let Some(handled) = &mut self.handled {
            if active.contains(&handled.tool) {
                handled.last_present = now;
                return None;
            }
            if now.saturating_duration_since(handled.last_present) < ABSENT_FOR_RESET {
                return None;
            }
            let tool = handled.tool;
            self.handled = None;
            self.pending = None;
            return Some(Observation::Ended { tool });
        }

        let candidate = MeetingTool::ALL
            .into_iter()
            .find(|tool| active.contains(tool) && policy(*tool) != MeetingPolicy::Off);

        let Some(tool) = candidate else {
            if self.pending.as_ref().is_some_and(|pending| {
                now.saturating_duration_since(pending.run.last) > DROPOUT_GRACE
            }) {
                self.pending = None;
            }
            return None;
        };

        let held = match &mut self.pending {
            Some(pending) if pending.tool == tool => pending.run.extend(now),
            _ => {
                self.pending = Some(Pending {
                    tool,
                    run: Run::start(now),
                });
                Duration::ZERO
            },
        };
        if held < tool.detection_delay() {
            return None;
        }

        self.pending = None;
        self.handled = Some(Handled {
            tool,
            last_present: now,
        });
        Some(Observation::Detected(Detection {
            tool,
            auto_record: policy(tool) == MeetingPolicy::AutoRecord,
        }))
    }

    /// A poll that could not read microphone activity.
    ///
    /// Unknown is not absence: a pending run keeps its progress, and a
    /// snooze's absence clock restarts so a failed read cannot count as the
    /// app having left.
    pub fn observe_unavailable(
        &mut self,
        now: Instant,
    ) {
        if let Some(pending) = &mut self.pending {
            pending.run.last = now;
        }
        if let Some(handled) = &mut self.handled {
            handled.last_present = now;
        }
    }

    /// Treat `tool` as already offered, for a call the user started recording
    /// by hand after it was detected.
    pub fn mark_handled(
        &mut self,
        now: Instant,
        tool: MeetingTool,
    ) {
        self.pending = None;
        self.handled = Some(Handled {
            tool,
            last_present: now,
        });
    }

    /// Forget every run and snooze, as when detection is turned off.
    pub fn reset(&mut self) {
        self.pending = None;
        self.handled = None;
    }

    /// The call currently snoozed, if any.
    #[must_use]
    pub fn handled_tool(&self) -> Option<MeetingTool> {
        self.handled.map(|handled| handled.tool)
    }
}

#[cfg(test)]
mod tests {
    use super::{ABSENT_FOR_RESET, DROPOUT_GRACE, Detector, Observation};
    use std::time::{Duration, Instant};
    use wisp_core::{MeetingPolicy, MeetingTool};

    fn at(
        base: Instant,
        millis: u64,
    ) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn ask(_: MeetingTool) -> MeetingPolicy {
        MeetingPolicy::Ask
    }

    #[test]
    fn native_tool_is_detected_after_two_seconds() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 1_000), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 1_999), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(super::Detection {
                tool: MeetingTool::Zoom,
                auto_record: false,
            }))
        );
    }

    #[test]
    fn browser_meeting_waits_out_the_voice_note_window() {
        let base = Instant::now();
        let mut detector = Detector::new();
        for millis in [0, 10_000, 19_000, 19_999] {
            assert_eq!(
                detector.observe(at(base, millis), &[MeetingTool::BrowserMeeting], ask),
                None
            );
        }
        assert!(matches!(
            detector.observe(at(base, 20_000), &[MeetingTool::BrowserMeeting], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn auto_record_comes_from_the_tool_policy() {
        let base = Instant::now();
        let mut detector = Detector::new();
        let policy = |tool: MeetingTool| match tool {
            MeetingTool::Zoom => MeetingPolicy::AutoRecord,
            _ => MeetingPolicy::Ask,
        };
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], policy),
            None
        );
        assert_eq!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], policy),
            Some(Observation::Detected(super::Detection {
                tool: MeetingTool::Zoom,
                auto_record: true,
            }))
        );
    }

    #[test]
    fn off_tools_are_never_detected() {
        let base = Instant::now();
        let mut detector = Detector::new();
        let off = |_: MeetingTool| MeetingPolicy::Off;
        for millis in [0, 1_000, 2_000, 30_000] {
            assert_eq!(
                detector.observe(at(base, millis), &[MeetingTool::Zoom], off),
                None
            );
        }
        assert_eq!(detector.handled_tool(), None);
    }

    #[test]
    fn a_different_tool_restarts_the_run() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 1_500), &[MeetingTool::MicrosoftTeams], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 3_499), &[MeetingTool::MicrosoftTeams], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 3_500), &[MeetingTool::MicrosoftTeams], ask),
            Some(Observation::Detected(super::Detection {
                tool: MeetingTool::MicrosoftTeams,
                ..
            }))
        ));
    }

    #[test]
    fn a_short_dropout_keeps_time_toward_the_offer() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(detector.observe(at(base, 1_000), &[], ask), None);
        assert!(matches!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn a_long_dropout_clears_the_pending_run() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 1_000), &[MeetingTool::Zoom], ask),
            None
        );
        let absent_at = 1_000 + u64::try_from(DROPOUT_GRACE.as_millis()).expect("fits") + 1;
        assert_eq!(detector.observe(at(base, absent_at), &[], ask), None);
        let restarted_at = absent_at + 1;
        assert_eq!(
            detector.observe(at(base, restarted_at), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(!matches!(
            detector.observe(at(base, restarted_at + 1_999), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
        assert!(matches!(
            detector.observe(at(base, restarted_at + 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn one_call_is_offered_once_until_it_leaves_the_microphone() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
        assert_eq!(
            detector.observe(at(base, 3_000), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 4_000), &[MeetingTool::Zoom], ask),
            None
        );
    }

    #[test]
    fn the_snooze_ends_after_a_minute_away_and_announces_it_once() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
        let reset_ms = 2_000 + u64::try_from(ABSENT_FOR_RESET.as_millis()).expect("fits");
        assert_eq!(detector.observe(at(base, reset_ms - 1), &[], ask), None);
        assert_eq!(
            detector.observe(at(base, reset_ms), &[], ask),
            Some(Observation::Ended {
                tool: MeetingTool::Zoom
            })
        );
        assert_eq!(detector.observe(at(base, reset_ms + 1), &[], ask), None);
    }

    #[test]
    fn an_unreadable_poll_does_not_restart_a_pending_run() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        detector.observe_unavailable(at(base, 1_000));
        assert!(matches!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn an_unreadable_poll_does_not_age_a_snooze() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 2_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
        let reset_ms = 2_000 + u64::try_from(ABSENT_FOR_RESET.as_millis()).expect("fits");
        detector.observe_unavailable(at(base, reset_ms - 1));
        assert_eq!(detector.observe(at(base, reset_ms), &[], ask), None);
        assert_eq!(
            detector.observe(at(base, reset_ms - 1 + 59_999), &[], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, reset_ms - 1 + 60_000), &[], ask),
            Some(Observation::Ended {
                tool: MeetingTool::Zoom
            })
        );
    }

    #[test]
    fn mark_handled_snoozes_a_call_started_by_hand() {
        let base = Instant::now();
        let mut detector = Detector::new();
        detector.mark_handled(at(base, 0), MeetingTool::MicrosoftTeams);
        assert_eq!(detector.handled_tool(), Some(MeetingTool::MicrosoftTeams));
        assert_eq!(
            detector.observe(at(base, 5_000), &[MeetingTool::MicrosoftTeams], ask),
            None
        );
    }

    #[test]
    fn reset_forgets_runs_and_snoozes() {
        let base = Instant::now();
        let mut detector = Detector::new();
        detector.mark_handled(at(base, 0), MeetingTool::Zoom);
        detector.reset();
        assert_eq!(detector.handled_tool(), None);
        assert_eq!(
            detector.observe(at(base, 1_000), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 3_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn a_long_observation_gap_restarts_the_run() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(at(base, 0), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 20_000), &[MeetingTool::Zoom], ask),
            None
        );
        assert_eq!(
            detector.observe(at(base, 21_999), &[MeetingTool::Zoom], ask),
            None
        );
        assert!(matches!(
            detector.observe(at(base, 22_000), &[MeetingTool::Zoom], ask),
            Some(Observation::Detected(_))
        ));
    }

    #[test]
    fn a_native_tool_wins_over_a_browser_holding_the_microphone() {
        let base = Instant::now();
        let mut detector = Detector::new();
        assert_eq!(
            detector.observe(
                at(base, 0),
                &[MeetingTool::BrowserMeeting, MeetingTool::Zoom],
                ask
            ),
            None
        );
        assert!(matches!(
            detector.observe(
                at(base, 2_000),
                &[MeetingTool::BrowserMeeting, MeetingTool::Zoom],
                ask
            ),
            Some(Observation::Detected(super::Detection {
                tool: MeetingTool::Zoom,
                ..
            }))
        ));
    }
}
