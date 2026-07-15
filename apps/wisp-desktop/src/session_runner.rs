//! Owns the background OS thread that drives `wisp_audiokit::Session`.
//!
//! The Swift side calls back into Rust from arbitrary audio threads, and
//! `Session::start()/stop()` block while async work runs underneath. To
//! keep the GPUI main thread responsive we run the lifecycle on a worker
//! thread and surface everything as a stream of `Update`s the UI polls.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use wisp_audiokit::{Event, Session, SessionConfig, SessionError};
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
}

/// Commands the UI sends to the worker.
pub enum Command {
    Start {
        output_dir: PathBuf,
        config: SessionConfig,
        session: SessionStart,
    },
    Stop,
    Shutdown,
}

/// Updates the worker sends back to the UI.
pub enum Update {
    /// `Session::start()` returned successfully and audio is flowing.
    Started(SessionStart),
    /// One transcription / log event from the session.
    Event { session_id: SessionId, event: Event },
    /// `Session::stop()` returned; the session has been torn down.
    Stopped { session_id: SessionId },
    /// Audio startup failed after constructing a session. Any partial capture
    /// has been stopped and its flushed events precede this update.
    StartFailed {
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

    /// Drain everything the worker has produced since the last poll, without
    /// blocking.
    pub fn drain_updates(&self) -> Vec<Update> {
        let mut out = Vec::new();
        while let Ok(u) = self.update_rx.try_recv() {
            out.push(u);
        }
        out
    }

    /// Block until the worker reports a terminal update for `session_id`.
    /// Used while quitting; `SessionRunner::drop` also waits for the worker,
    /// so collecting the final updates here does not introduce a new hang.
    pub fn wait_until_finished(
        &self,
        session_id: SessionId,
    ) -> Vec<Update> {
        let mut collected = Vec::new();
        loop {
            collected.extend(self.drain_updates());
            if collected
                .iter()
                .any(|update| is_terminal_for(update, session_id))
            {
                break;
            }
            match self.update_rx.recv() {
                Ok(update) => collected.push(update),
                Err(_) => break,
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
            Ok(Command::Stop) => {}, // no-op, nothing running
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
    let mut session = match Session::new_with_config(output_dir, config) {
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
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                session.stop();
                let _ = update_tx.send(Update::Stopped { session_id });
                return;
            },
            Ok(Command::Start { .. }) | Err(TryRecvError::Empty) => {},
        }
        if let Some(event) = session.recv_timeout(CMD_POLL_INTERVAL) {
            let _ = update_tx.send(Update::Event { session_id, event });
        }
    }

    session.stop();
    // Drain whatever the analyzer flushed during stop().
    while let Some(event) = session.try_recv() {
        let _ = update_tx.send(Update::Event { session_id, event });
    }
    let _ = update_tx.send(Update::Stopped { session_id });
}

fn is_transcript_result(event: &Event) -> bool {
    matches!(event, Event::Result(_))
}

#[cfg(test)]
mod tests {
    use wisp_audiokit::{Event, SessionResult, SourceLabel};

    use super::is_transcript_result;

    #[test]
    fn only_transcript_results_require_preserving_a_failed_start() {
        assert!(!is_transcript_result(&Event::Log("stopping".into())));
        assert!(is_transcript_result(&Event::Result(SessionResult {
            source: SourceLabel::Mic,
            segment_id: 1,
            text: "partial transcript".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
        })));
    }
}
