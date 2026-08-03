//! Owns the background OS thread that drives the platform audio session.
//!
//! On macOS this is the backend-neutral orchestrator facade over the Swift
//! capture/transcription callbacks. Start/stop block while async platform work
//! runs underneath, so the lifecycle stays on a worker thread and surfaces
//! everything as a stream of `Update`s the UI polls.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
#[cfg(target_os = "macos")]
use wisp_audiokit::MacosSession as PlatformSession;
#[cfg(not(target_os = "macos"))]
use wisp_audiokit::Session as PlatformSession;
use wisp_audiokit::{Event, SessionConfig, SessionError};
use wisp_core::SessionId;

/// How often the running session checks for UI commands (Stop / Shutdown)
/// while waiting for the next audio event. Sets the worst-case latency for
/// a Stop press to be honoured. Events themselves are delivered
/// immediately — this only bounds the *idle* wake-up cadence.
const CMD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Stable identity allocated before audio starts. Every worker update carries
/// this id so a delayed update can never mutate a newer session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStart {
    pub session_id: SessionId,
    /// Timestamp and directory selected once by the UI before the database
    /// row and worker command are created. Every retry must reuse them.
    pub started_at: DateTime<Utc>,
    pub dir_name: String,
}

/// Commands the UI sends to the worker.
pub enum Command {
    Start {
        output_dir: PathBuf,
        config: SessionConfig,
        session: SessionStart,
    },
    SetMicrophoneMuted(bool),
    Stop,
    Shutdown,
}

/// Updates the worker sends back to the UI.
pub enum Update {
    /// The platform session started successfully and audio is flowing.
    Started(SessionStart),
    /// One transcription / log event from the session.
    Event { session_id: SessionId, event: Event },
    /// The platform session stopped and has been torn down.
    Stopped { session_id: SessionId },
    /// Audio startup failed after constructing a session. Any partial capture
    /// has been stopped and its flushed events precede this update.
    StartFailed {
        session_id: SessionId,
        error: SessionError,
    },
    /// Capture/transcription failed after start; platform cleanup has already
    /// completed and partial audio/transcript must be finalized.
    RuntimeFailed {
        session_id: SessionId,
        error: SessionError,
    },
    /// Session construction failed before capture could start.
    Error {
        session_id: SessionId,
        error: SessionError,
    },
}

pub struct SessionRunner {
    cmd_tx: Sender<Command>,
    update_rx: Receiver<Update>,
    join: Option<JoinHandle<()>>,
}

impl SessionRunner {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (update_tx, update_rx) = channel::<Update>();
        let join = std::thread::Builder::new()
            .name("wisp-session-runner".into())
            .spawn(move || worker_loop(&cmd_rx, &update_tx))
            .expect("spawn session-runner thread");
        Self {
            cmd_tx,
            update_rx,
            join: Some(join),
        }
    }

    #[must_use]
    pub fn start(
        &self,
        output_dir: PathBuf,
        config: SessionConfig,
        session: SessionStart,
    ) -> bool {
        self.cmd_tx
            .send(Command::Start {
                output_dir,
                config,
                session,
            })
            .is_ok()
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    #[must_use]
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) -> bool {
        self.cmd_tx.send(Command::SetMicrophoneMuted(muted)).is_ok()
    }

    /// Drain everything the worker has produced since the last poll, without
    /// blocking.
    pub fn drain_updates(&self) -> Vec<Update> {
        let mut out = Vec::new();
        while let Ok(u) = self.update_rx.try_recv() {
            out.push(u);
        }
        out
    }

    /// Block until the worker reports a terminal update for `session_id`, or
    /// until the bounded quit grace period expires.
    pub fn wait_for_idle(
        &self,
        session_id: SessionId,
        timeout: Duration,
    ) -> Vec<Update> {
        let deadline = Instant::now() + timeout;
        let mut collected = Vec::new();
        loop {
            collected.extend(self.drain_updates());
            if collected
                .iter()
                .any(|update| is_terminal_for(update, session_id))
            {
                break;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self.update_rx.recv_timeout(remaining) {
                Ok(update) => collected.push(update),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        collected
    }
}

fn is_terminal_for(
    update: &Update,
    expected_session_id: SessionId,
) -> bool {
    match update {
        Update::Stopped { session_id }
        | Update::StartFailed { session_id, .. }
        | Update::RuntimeFailed { session_id, .. }
        | Update::Error { session_id, .. } => *session_id == expected_session_id,
        Update::Started(_) | Update::Event { .. } => false,
    }
}

impl Drop for SessionRunner {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(
    cmd_rx: &Receiver<Command>,
    update_tx: &Sender<Update>,
) {
    loop {
        match cmd_rx.recv() {
            Ok(Command::Start {
                output_dir,
                config,
                session,
            }) => {
                run_session(&output_dir, config, session, cmd_rx, update_tx);
            },
            Ok(Command::SetMicrophoneMuted(_) | Command::Stop) => {
                // no-op, nothing running
            },
            Ok(Command::Shutdown) | Err(_) => return,
        }
    }
}

fn run_session(
    output_dir: &std::path::Path,
    config: SessionConfig,
    session_start: SessionStart,
    cmd_rx: &Receiver<Command>,
    update_tx: &Sender<Update>,
) {
    let session_id = session_start.session_id;
    let mut session = match PlatformSession::new_with_config(output_dir, config) {
        Ok(s) => s,
        Err(e) => {
            let _ = update_tx.send(Update::Error {
                session_id,
                error: e,
            });
            return;
        },
    };
    if let Err(e) = session.start() {
        let mut preserve_partial = session.has_started_capture();
        session.stop();
        while let Some(event) = session.try_recv() {
            preserve_partial |= is_transcript_result(&event);
            let _ = update_tx.send(Update::Event { session_id, event });
        }
        let update = if preserve_partial {
            Update::StartFailed {
                session_id,
                error: e,
            }
        } else {
            Update::Error {
                session_id,
                error: e,
            }
        };
        let _ = update_tx.send(update);
        return;
    }
    let _ = update_tx.send(Update::Started(session_start));

    // Pump events until the UI asks to stop. Between events we wake at
    // most every `CMD_POLL_INTERVAL` to check the command channel so a
    // Stop request doesn't have to wait for the next audio event; when
    // events are arriving we forward them immediately without polling.
    loop {
        match cmd_rx.try_recv() {
            Ok(Command::Stop) => break,
            Ok(Command::SetMicrophoneMuted(muted)) => {
                session.set_microphone_muted(muted);
            },
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                stop_and_publish(&mut session, session_id, update_tx);
                return;
            },
            Ok(Command::Start { .. }) | Err(TryRecvError::Empty) => {},
        }
        if let Some(event) = session.recv_timeout(CMD_POLL_INTERVAL) {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let terminal_error = session.take_runtime_failure();
            let _ = update_tx.send(Update::Event { session_id, event });
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if let Some(error) = terminal_error {
                // Runtime failure is terminal, but native capture/writer
                // ownership must be stopped and finalized before persistence
                // observes RuntimeFailed.
                #[cfg(target_os = "windows")]
                let error = merge_runtime_failures(error, session.stop_and_take_runtime_failure());
                #[cfg(target_os = "macos")]
                let error = {
                    session.stop();
                    merge_runtime_failures(error, session.take_runtime_failure())
                };
                publish_runtime_failure_after_drain(
                    || session.try_recv(),
                    session_id,
                    error,
                    update_tx,
                );
                return;
            }
        }
    }

    stop_and_publish(&mut session, session_id, update_tx);
}

fn stop_and_publish(
    session: &mut PlatformSession,
    session_id: SessionId,
    update_tx: &Sender<Update>,
) {
    #[cfg(target_os = "windows")]
    let terminal_error = session.stop_and_take_runtime_failure();
    #[cfg(not(target_os = "windows"))]
    session.stop();
    // Drain whatever the analyzer flushed during stop().
    while let Some(event) = session.try_recv() {
        let _ = update_tx.send(Update::Event { session_id, event });
    }
    #[cfg(target_os = "macos")]
    let terminal_error = session.take_runtime_failure();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if let Some(error) = terminal_error {
        let _ = update_tx.send(Update::RuntimeFailed { session_id, error });
        return;
    }
    let _ = update_tx.send(Update::Stopped { session_id });
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn merge_runtime_failures(
    primary: SessionError,
    cleanup: Option<SessionError>,
) -> SessionError {
    match cleanup {
        Some(cleanup) => SessionError::Start(format!(
            "{primary}; cleanup/finalization also failed: {cleanup}"
        )),
        None => primary,
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn publish_runtime_failure_after_drain(
    mut try_recv: impl FnMut() -> Option<Event>,
    session_id: SessionId,
    error: SessionError,
    update_tx: &Sender<Update>,
) {
    // A strict transcriber failure performs graceful native cleanup before it
    // becomes terminal. SpeechAnalyzer may emit its last final while that
    // cleanup is running, so publish every flushed event before the terminal
    // update tells the UI to finalize persistence.
    while let Some(event) = try_recv() {
        let _ = update_tx.send(Update::Event { session_id, event });
    }
    let _ = update_tx.send(Update::RuntimeFailed { session_id, error });
}

fn is_transcript_result(event: &Event) -> bool {
    matches!(event, Event::Result(_))
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use std::collections::VecDeque;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use std::sync::mpsc::channel;

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use wisp_audiokit::SessionError;
    use wisp_audiokit::{Event, SessionResult, SourceLabel};
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use wisp_core::SessionId;

    use super::is_transcript_result;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use super::{Update, merge_runtime_failures, publish_runtime_failure_after_drain};

    #[test]
    fn only_transcript_results_require_preserving_a_failed_start() {
        assert!(!is_transcript_result(&Event::Log("stopping".into())));
        assert!(is_transcript_result(&Event::Result(SessionResult {
            source: SourceLabel::Mic,
            segment_id: 1,
            is_final: false,
            text: "partial transcript".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            confidence_mean: None,
            confidence_min: None,
        })));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn runtime_failure_publishes_drained_events_before_terminal_update() {
        let session_id = SessionId::from(42);
        let final_result = Event::Result(SessionResult {
            source: SourceLabel::System,
            segment_id: 42,
            is_final: true,
            text: "cleanup final".into(),
            start_seconds: 2.0,
            end_seconds: 3.0,
            confidence_mean: Some(0.9),
            confidence_min: Some(0.8),
        });
        let mut cleanup_events = VecDeque::from([final_result.clone()]);
        let (tx, rx) = channel();

        publish_runtime_failure_after_drain(
            || cleanup_events.pop_front(),
            session_id,
            SessionError::Start("strict transcriber failure".into()),
            &tx,
        );

        assert!(matches!(
            rx.recv().unwrap(),
            Update::Event {
                session_id: actual,
                event,
            } if actual == session_id && event == final_result
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            Update::RuntimeFailed {
                session_id: actual,
                ..
            } if actual == session_id
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn runtime_failure_preserves_cleanup_failure_context() {
        let combined = merge_runtime_failures(
            SessionError::Start("capture failed".into()),
            Some(SessionError::Start("sync failed".into())),
        );
        let message = combined.to_string();
        assert!(message.contains("capture failed"));
        assert!(message.contains("sync failed"));
    }
}
