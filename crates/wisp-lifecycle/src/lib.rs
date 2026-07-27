//! Executable session-lifecycle policy shared by Wisp and its verification.
//!
//! Keep state-independent data (timestamps, paths, transcript contents) in
//! the application. This crate owns the decisions that protect a live
//! transcript from navigation and delayed worker updates.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Starting,
    Recording,
    Stopping,
    Failed,
}

impl Phase {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Recording | Self::Stopping)
    }

    /// Apply a `Started` update without undoing an already-requested stop.
    #[must_use]
    pub const fn worker_started(self) -> Self {
        if matches!(self, Self::Stopping) {
            Self::Stopping
        } else if self.is_active() {
            Self::Recording
        } else {
            self
        }
    }

    /// Request a stop only from a phase in which a worker can be starting or
    /// running. Repeated stop requests are idempotent.
    #[must_use]
    pub const fn request_stop(self) -> Self {
        if self.is_active() {
            Self::Stopping
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewOwner {
    Library,
    Live,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerUpdate {
    Started,
    Event,
    Stopped,
    StartFailed,
    Error,
}

/// The lifecycle fields needed to decide whether an asynchronous worker
/// update belongs to the currently visible live transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateContext {
    pub phase: Phase,
    pub view: ViewOwner,
    pub current_session_id: Option<i64>,
}

impl UpdateContext {
    #[must_use]
    pub const fn accepts(
        self,
        update_session_id: i64,
    ) -> bool {
        self.phase.is_active()
            && matches!(self.view, ViewOwner::Live)
            && matches!(self.current_session_id, Some(id) if id == update_session_id)
    }

    #[must_use]
    pub const fn next_phase(
        self,
        update_session_id: i64,
        update: WorkerUpdate,
    ) -> Option<Phase> {
        if !self.accepts(update_session_id) {
            return None;
        }
        Some(match update {
            WorkerUpdate::Started => self.phase.worker_started(),
            WorkerUpdate::Event => self.phase,
            WorkerUpdate::Stopped | WorkerUpdate::StartFailed | WorkerUpdate::Error => Phase::Idle,
        })
    }

    #[must_use]
    pub const fn invariant_holds(self) -> bool {
        !self.phase.is_active()
            || (matches!(self.view, ViewOwner::Live) && self.current_session_id.is_some())
    }
}

/// Stateful test driver around the production `UpdateContext` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reducer {
    context: UpdateContext,
    applied_events: u32,
    terminal_updates: u32,
}

impl Reducer {
    #[must_use]
    pub const fn new(session_id: i64) -> Self {
        Self {
            context: UpdateContext {
                phase: Phase::Starting,
                view: ViewOwner::Live,
                current_session_id: Some(session_id),
            },
            applied_events: 0,
            terminal_updates: 0,
        }
    }

    #[must_use]
    pub const fn context(self) -> UpdateContext {
        self.context
    }

    #[must_use]
    pub const fn applied_events(self) -> u32 {
        self.applied_events
    }

    #[must_use]
    pub const fn terminal_updates(self) -> u32 {
        self.terminal_updates
    }

    pub const fn request_stop(&mut self) {
        self.context.phase = self.context.phase.request_stop();
    }

    /// Returns whether the update was accepted for this session.
    pub const fn apply(
        &mut self,
        session_id: i64,
        update: WorkerUpdate,
    ) -> bool {
        let Some(next_phase) = self.context.next_phase(session_id, update) else {
            return false;
        };
        match update {
            WorkerUpdate::Event => {
                self.applied_events = self.applied_events.saturating_add(1);
            },
            WorkerUpdate::Stopped | WorkerUpdate::StartFailed | WorkerUpdate::Error => {
                self.context.current_session_id = None;
                self.terminal_updates = self.terminal_updates.saturating_add(1);
            },
            WorkerUpdate::Started => {},
        }
        self.context.phase = next_phase;
        true
    }
}

/// Navigation and restart must not replace a transcript while any worker or
/// persistence ownership remains.
#[must_use]
pub const fn can_replace_transcript(
    phase: Phase,
    pending_persistence: bool,
    retained_output: bool,
) -> bool {
    !phase.is_active() && !pending_persistence && !retained_output
}

#[cfg(kani)]
mod proofs {
    use super::{Phase, Reducer, UpdateContext, ViewOwner, WorkerUpdate, can_replace_transcript};

    fn any_phase(value: u8) -> Phase {
        match value % 5 {
            0 => Phase::Idle,
            1 => Phase::Starting,
            2 => Phase::Recording,
            3 => Phase::Stopping,
            _ => Phase::Failed,
        }
    }

    fn any_view(value: u8) -> ViewOwner {
        match value % 3 {
            0 => ViewOwner::Library,
            1 => ViewOwner::Live,
            _ => ViewOwner::History,
        }
    }

    fn any_update(value: u8) -> WorkerUpdate {
        match value % 5 {
            0 => WorkerUpdate::Started,
            1 => WorkerUpdate::Event,
            2 => WorkerUpdate::Stopped,
            3 => WorkerUpdate::StartFailed,
            _ => WorkerUpdate::Error,
        }
    }

    #[kani::proof]
    fn active_context_only_accepts_its_live_session() {
        let context = UpdateContext {
            phase: any_phase(kani::any()),
            view: any_view(kani::any()),
            current_session_id: kani::any(),
        };
        let update_session_id = kani::any();
        if context.accepts(update_session_id) {
            assert!(context.invariant_holds());
            assert_eq!(context.current_session_id, Some(update_session_id));
        }
    }

    #[kani::proof]
    fn reducer_preserves_ownership_invariant() {
        let session_id = kani::any();
        let update_session_id = kani::any();
        let update = any_update(kani::any());
        let request_stop = kani::any();
        let mut reducer = Reducer::new(session_id);
        if request_stop {
            reducer.request_stop();
        }
        let _ = reducer.apply(update_session_id, update);
        assert!(reducer.context().invariant_holds());
    }

    #[kani::proof]
    fn delayed_updates_cannot_mutate_another_session() {
        let session_id = kani::any();
        let delayed_session_id = kani::any();
        kani::assume(session_id != delayed_session_id);
        let mut reducer = Reducer::new(session_id);
        let before = reducer;
        assert!(!reducer.apply(delayed_session_id, any_update(kani::any())));
        assert_eq!(reducer, before);
    }

    #[kani::proof]
    fn stop_is_monotonic_and_idempotent() {
        let phase = any_phase(kani::any());
        let stopped = phase.request_stop();
        assert_eq!(stopped.request_stop(), stopped);
        if phase.is_active() {
            assert_eq!(stopped, Phase::Stopping);
        } else {
            assert_eq!(stopped, phase);
        }
    }

    #[kani::proof]
    fn navigation_requires_all_ownership_to_settle() {
        let phase = any_phase(kani::any());
        let pending = kani::any();
        let output = kani::any();
        if can_replace_transcript(phase, pending, output) {
            assert!(!phase.is_active());
            assert!(!pending);
            assert!(!output);
        }
    }
}
