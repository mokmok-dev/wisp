//! Background poll of which meeting tools hold the microphone.
//!
//! The thread owns the [`Detector`] and its clock. The GPUI thread only sends
//! preference changes and drains the resulting one-shot events, so no render
//! frame waits on Core Audio.

use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wisp_audiokit::{Detector, Observation, active_meeting_tools};
use wisp_core::{MeetingDetectionSettings, MeetingTool};

/// How often microphone activity is sampled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the watcher reports to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingEvent {
    /// A call held the microphone long enough to act on.
    Detected {
        tool: MeetingTool,
        auto_record: bool,
    },
    /// A previously offered call left the microphone long enough to reset.
    Ended { tool: MeetingTool },
    /// Microphone activity could not be read. The detector kept its state.
    SamplerUnavailable,
}

enum Control {
    SetPreferences(Box<MeetingDetectionSettings>),
    Shutdown,
}

/// Owns the polling thread and its channel endpoints.
pub struct MeetingWatcher {
    control: Sender<Control>,
    events: Mutex<Receiver<MeetingEvent>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for MeetingWatcher {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter
            .debug_struct("MeetingWatcher")
            .finish_non_exhaustive()
    }
}

impl MeetingWatcher {
    /// Start the poll thread. Nothing is sampled until detection is enabled.
    #[must_use]
    pub fn spawn(preferences: MeetingDetectionSettings) -> Self {
        let (control, controls) = channel::<Control>();
        let (events, event_rx) = sync_channel::<MeetingEvent>(16);
        let handle = thread::Builder::new()
            .name("wisp-meeting-watch".into())
            .spawn(move || run(preferences, &controls, &events))
            .ok();
        Self {
            control,
            events: Mutex::new(event_rx),
            handle: Mutex::new(handle),
        }
    }

    /// Replace the enabled flag and per-tool policies.
    pub fn set_preferences(
        &self,
        preferences: MeetingDetectionSettings,
    ) {
        let _ = self
            .control
            .send(Control::SetPreferences(Box::new(preferences)));
    }

    /// Take every event since the last drain.
    pub fn drain_events(&self) -> Vec<MeetingEvent> {
        lock(&self.events).try_iter().collect()
    }
}

impl Drop for MeetingWatcher {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Shutdown);
        if let Some(handle) = lock(&self.handle).take() {
            let _ = handle.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run(
    mut preferences: MeetingDetectionSettings,
    controls: &Receiver<Control>,
    events: &SyncSender<MeetingEvent>,
) {
    let mut detector = Detector::new();
    let mut announced_failure = false;
    loop {
        match wait(controls) {
            Wake::Preferences(next) => {
                // Re-enabling starts a fresh run; a policy edit must not clear
                // the snooze for a call that is still in progress.
                let enabled_changed = preferences.enabled != next.enabled;
                preferences = *next;
                if enabled_changed {
                    detector.reset();
                }
            },
            Wake::Shutdown => return,
            Wake::Tick => {},
        }
        if !preferences.enabled {
            detector.reset();
            continue;
        }
        if !sample(&preferences, &mut detector, events, &mut announced_failure) {
            return;
        }
    }
}

enum Wake {
    Tick,
    Preferences(Box<MeetingDetectionSettings>),
    Shutdown,
}

fn wait(controls: &Receiver<Control>) -> Wake {
    match controls.recv_timeout(POLL_INTERVAL) {
        Ok(Control::SetPreferences(next)) => Wake::Preferences(next),
        Ok(Control::Shutdown) | Err(RecvTimeoutError::Disconnected) => Wake::Shutdown,
        Err(RecvTimeoutError::Timeout) => Wake::Tick,
    }
}

/// One poll. Returns false when the receiver is gone.
fn sample(
    preferences: &MeetingDetectionSettings,
    detector: &mut Detector,
    events: &SyncSender<MeetingEvent>,
    announced_failure: &mut bool,
) -> bool {
    let now = Instant::now();
    let Ok(active) = active_meeting_tools() else {
        detector.observe_unavailable(now);
        if !*announced_failure {
            *announced_failure = true;
            let _ = events.try_send(MeetingEvent::SamplerUnavailable);
        }
        return true;
    };
    *announced_failure = false;
    let Some(observation) = detector.observe(now, &active, |tool| preferences.policy(tool)) else {
        return true;
    };
    let event = match observation {
        Observation::Detected(detection) => MeetingEvent::Detected {
            tool: detection.tool,
            auto_record: detection.auto_record,
        },
        Observation::Ended { tool } => MeetingEvent::Ended { tool },
    };
    match events.try_send(event) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}
