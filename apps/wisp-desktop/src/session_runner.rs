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

use wisp_audiokit::{Event, Session, SessionError};

/// Commands the UI sends to the worker.
pub enum Command {
    Start { output_dir: PathBuf, locale: String },
    Stop,
    Shutdown,
}

/// Updates the worker sends back to the UI.
pub enum Update {
    /// `Session::start()` returned successfully and audio is flowing.
    Started,
    /// One transcription / log event from the session.
    Event(Event),
    /// `Session::stop()` returned; the session has been torn down.
    Stopped,
    /// Lifecycle error (start/construct failed).
    Error(String),
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

    pub fn start(
        &self,
        output_dir: PathBuf,
        locale: String,
    ) {
        let _ = self.cmd_tx.send(Command::Start { output_dir, locale });
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
            Ok(Command::Start { output_dir, locale }) => {
                run_session(&output_dir, &locale, cmd_rx, update_tx);
            },
            Ok(Command::Stop) => {}, // no-op, nothing running
            Ok(Command::Shutdown) | Err(_) => return,
        }
    }
}

fn run_session(
    output_dir: &std::path::Path,
    locale: &str,
    cmd_rx: &Receiver<Command>,
    update_tx: &Sender<Update>,
) {
    let session = match Session::new(output_dir, locale) {
        Ok(s) => s,
        Err(e) => {
            let _ = update_tx.send(Update::Error(format_err(&e)));
            return;
        },
    };
    if let Err(e) = session.start() {
        let _ = update_tx.send(Update::Error(format_err(&e)));
        return;
    }
    let _ = update_tx.send(Update::Started);

    // Pump events until the UI asks to stop. We poll the cmd channel
    // between event reads so a Stop request doesn't have to wait for the
    // next audio event.
    loop {
        match cmd_rx.try_recv() {
            Ok(Command::Stop) => break,
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                session.stop();
                let _ = update_tx.send(Update::Stopped);
                return;
            },
            Ok(Command::Start { .. }) | Err(TryRecvError::Empty) => {},
        }
        if let Some(event) = session.try_recv() {
            let _ = update_tx.send(Update::Event(event));
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    session.stop();
    // Drain whatever the analyzer flushed during stop().
    while let Some(event) = session.try_recv() {
        let _ = update_tx.send(Update::Event(event));
    }
    let _ = update_tx.send(Update::Stopped);
}

fn format_err(e: &SessionError) -> String {
    format!("{e}")
}
