-------------------------- MODULE SessionLifecycle --------------------------
EXTENDS Integers, Naturals, Sequences

(******************************************************************************
The asynchronous protocol implemented by:

  * apps/wisp-desktop/src/main.rs::toggle_recording
  * apps/wisp-desktop/src/session_runner.rs
  * apps/wisp-desktop/src/session_updates.rs::apply_update

The UI and worker communicate through FIFO queues.  `MaxEvents` bounds only
the number of transcript events in one run so TLC can exhaustively enumerate
the state space.  Session counters remain unbounded in the companion Z3
proof.  Here `Event` means a result that creates a new persistable segment.
Log events and same-segment partial revisions, which do not grow the persisted
segment vector, are omitted as stuttering steps of this projection.
******************************************************************************)

CONSTANT MaxEvents

ASSUME MaxEvents \in Nat \ {0}

UiStates == {"Idle", "Starting", "Recording", "Stopping", "Failed"}
WorkerStates == {"Idle", "Starting", "Failing", "Running", "Stopping"}
CommandKinds == {"Start", "Stop"}
UpdateKinds == {"Started", "Event", "Stopped", "Error"}
StoredSessionStates == {"None", "Open", "Ended"}
QuitModes == {"Running", "Explicit", "OS"}

VARIABLES
    ui,
    worker,
    commands,
    updates,
    emittedEvents,
    segments,
    segmentsFinal,
    currentSession,
    storedSession,
    launchMetadata,
    recoveryDurable,
    lastError,
    quitMode

vars == <<
    ui,
    worker,
    commands,
    updates,
    emittedEvents,
    segments,
    segmentsFinal,
    currentSession,
    storedSession,
    launchMetadata,
    recoveryDurable,
    lastError,
    quitMode
>>

RECURSIVE SeqCount(_, _)
SeqCount(sequence, value) ==
    IF Len(sequence) = 0
    THEN 0
    ELSE IF Head(sequence) = value
         THEN 1 + SeqCount(Tail(sequence), value)
         ELSE SeqCount(Tail(sequence), value)

HasHead(queue, value) == queue # <<>> /\ Head(queue) = value

PersistableContext ==
    /\ launchMetadata
    /\ \/ /\ currentSession
           /\ storedSession = "Open"
       \/ /\ ~currentSession
           /\ storedSession = "None"

FailureOwnership ==
    \/ /\ currentSession' = currentSession
       /\ storedSession' = storedSession
    \/ /\ ~currentSession
       /\ storedSession = "None"
       /\ currentSession' = TRUE
       /\ storedSession' = "Open"

SettledSafe ==
    /\ ui \in {"Idle", "Failed"}
    /\ worker = "Idle"
    /\ ~launchMetadata
    /\ ~currentSession
    /\ storedSession # "Open"

QuitSafe == SettledSafe \/ recoveryDurable

RelaunchCandidate ==
    /\ recoveryDurable
    /\ PersistableContext
    /\ \/ /\ quitMode \in {"Explicit", "OS"}
           /\ ui = "Failed"
           /\ worker = "Idle"
       \/ /\ quitMode = "OS"
           /\ ui \in {"Starting", "Recording", "Stopping"}

Init ==
    /\ ui = "Idle"
    /\ worker = "Idle"
    /\ commands = <<>>
    /\ updates = <<>>
    /\ emittedEvents = 0
    /\ segments = 0
    /\ segmentsFinal = TRUE
    /\ currentSession = FALSE
    /\ storedSession = "None"
    /\ launchMetadata = FALSE
    /\ recoveryDurable = FALSE
    /\ lastError = FALSE
    /\ quitMode = "Running"

RequestStart ==
    /\ ui \in {"Idle", "Failed"}
    /\ worker = "Idle"
    /\ ~currentSession
    /\ storedSession # "Open"
    /\ ~launchMetadata
    /\ ui' = "Starting"
    /\ commands' = Append(commands, "Start")
    /\ emittedEvents' = 0
    /\ segments' = 0
    /\ segmentsFinal' = TRUE
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ launchMetadata' = TRUE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ UNCHANGED <<worker, updates, quitMode>>

RequestStop ==
    /\ ui = "Recording"
    /\ ui' = "Stopping"
    /\ commands' = Append(commands, "Stop")
    /\ UNCHANGED <<
        worker, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

RequestStopWhileStarting ==
    /\ ui = "Starting"
    /\ ui' = "Stopping"
    /\ commands' = Append(commands, "Stop")
    /\ UNCHANGED <<
        worker, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerTakeStart ==
    /\ worker = "Idle"
    /\ HasHead(commands, "Start")
    /\ worker' = "Starting"
    /\ commands' = Tail(commands)
    /\ UNCHANGED <<
        ui, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerStartSucceeded ==
    /\ worker = "Starting"
    /\ worker' = "Running"
    /\ updates' = Append(updates, "Started")
    /\ UNCHANGED <<
        ui, commands, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerBeginStartFailure ==
    /\ worker = "Starting"
    /\ worker' = "Failing"
    /\ UNCHANGED <<
        ui, commands, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerFlushFailedStartEvent ==
    /\ worker = "Failing"
    /\ emittedEvents < MaxEvents
    /\ updates' = Append(updates, "Event")
    /\ emittedEvents' = emittedEvents + 1
    /\ UNCHANGED <<
        ui, worker, commands, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerFinishStartFailure ==
    /\ worker = "Failing"
    /\ worker' = "Idle"
    /\ updates' = Append(updates, "Error")
    /\ UNCHANGED <<
        ui, commands, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

ResolveWorkerStart == WorkerStartSucceeded \/ WorkerBeginStartFailure

WorkerEmitEvent ==
    /\ worker = "Running"
    /\ emittedEvents < MaxEvents
    /\ updates' = Append(updates, "Event")
    /\ emittedEvents' = emittedEvents + 1
    /\ UNCHANGED <<
        ui, worker, commands, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerTakeStop ==
    /\ worker = "Running"
    /\ HasHead(commands, "Stop")
    /\ worker' = "Stopping"
    /\ commands' = Tail(commands)
    /\ UNCHANGED <<
        ui, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerFlushEvent ==
    /\ worker = "Stopping"
    /\ emittedEvents < MaxEvents
    /\ updates' = Append(updates, "Event")
    /\ emittedEvents' = emittedEvents + 1
    /\ UNCHANGED <<
        ui, worker, commands, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerFinishStop ==
    /\ worker = "Stopping"
    /\ worker' = "Idle"
    /\ updates' = Append(updates, "Stopped")
    /\ UNCHANGED <<
        ui, commands, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerIgnoreStop ==
    /\ worker = "Idle"
    /\ HasHead(commands, "Stop")
    /\ commands' = Tail(commands)
    /\ UNCHANGED <<
        ui, worker, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

WorkerStep ==
    \/ WorkerTakeStart
    \/ ResolveWorkerStart
    \/ WorkerFlushFailedStartEvent
    \/ WorkerFinishStartFailure
    \/ WorkerEmitEvent
    \/ WorkerTakeStop
    \/ WorkerFlushEvent
    \/ WorkerFinishStop
    \/ WorkerIgnoreStop

ApplyStarted ==
    /\ HasHead(updates, "Started")
    /\ ui \in {"Starting", "Stopping"}
    /\ launchMetadata
    /\ ui' = IF ui = "Stopping" THEN "Stopping" ELSE "Recording"
    /\ updates' = Tail(updates)
    /\ \/ /\ currentSession' = TRUE
           /\ storedSession' = "Open"
       \/ /\ currentSession' = FALSE
           /\ storedSession' = "None"
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, segmentsFinal,
        launchMetadata, recoveryDurable, lastError, quitMode
       >>

ApplyEvent ==
    /\ HasHead(updates, "Event")
    /\ ui \in {"Starting", "Recording", "Stopping"}
    /\ updates' = Tail(updates)
    /\ segments' = segments + 1
    /\ segmentsFinal' = FALSE
    /\ UNCHANGED <<
        ui, worker, commands, emittedEvents,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError, quitMode
       >>

ApplyStoppedSucceeded ==
    /\ HasHead(updates, "Stopped")
    /\ ui \in {"Recording", "Stopping"}
    /\ PersistableContext
    /\ ui' = "Idle"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, lastError, quitMode
       >>

(******************************************************************************
Database finalization creates a missing row when Started could not do so, then
replaces all segments and ends the row transactionally.  Failure retains the
launch metadata and any open-row handle.  The implementation atomically tries
to write a recovery snapshot; its success is tracked separately because quit
must remain blocked if no durable copy exists.
******************************************************************************)
ApplyStoppedPersistenceFailedRecovered ==
    /\ HasHead(updates, "Stopped")
    /\ ui \in {"Recording", "Stopping"}
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, launchMetadata, quitMode
       >>

ApplyStoppedPersistenceFailedWithoutRecovery ==
    /\ HasHead(updates, "Stopped")
    /\ ui \in {"Recording", "Stopping"}
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ FailureOwnership
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, launchMetadata, quitMode
       >>

ApplyStopped ==
    \/ ApplyStoppedSucceeded
    \/ ApplyStoppedPersistenceFailedRecovered
    \/ ApplyStoppedPersistenceFailedWithoutRecovery

ApplyErrorWithoutSegments ==
    /\ HasHead(updates, "Error")
    /\ ui \in {"Starting", "Stopping"}
    /\ segments = 0
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ UNCHANGED <<worker, commands, emittedEvents, segments, quitMode>>

(******************************************************************************
A native start failure may flush microphone results before Error.  The reducer
first consumes those FIFO Event updates, then persists the non-empty partial
transcript.  Database failure follows the same durable-sidecar policy as a
normal stop instead of discarding captured speech.
******************************************************************************)
ApplyErrorWithSegmentsSucceeded ==
    /\ HasHead(updates, "Error")
    /\ ui \in {"Starting", "Stopping"}
    /\ segments > 0
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ UNCHANGED <<worker, commands, emittedEvents, segments, quitMode>>

ApplyErrorWithSegmentsPersistenceFailedRecovered ==
    /\ HasHead(updates, "Error")
    /\ ui \in {"Starting", "Stopping"}
    /\ segments > 0
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, launchMetadata, quitMode
       >>

ApplyErrorWithSegmentsPersistenceFailedWithoutRecovery ==
    /\ HasHead(updates, "Error")
    /\ ui \in {"Starting", "Stopping"}
    /\ segments > 0
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ ui' = "Failed"
    /\ updates' = Tail(updates)
    /\ segmentsFinal' = TRUE
    /\ FailureOwnership
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        worker, commands, emittedEvents, segments, launchMetadata, quitMode
       >>

ApplyError ==
    \/ ApplyErrorWithoutSegments
    \/ ApplyErrorWithSegmentsSucceeded
    \/ ApplyErrorWithSegmentsPersistenceFailedRecovered
    \/ ApplyErrorWithSegmentsPersistenceFailedWithoutRecovery

ApplyUpdate == ApplyStarted \/ ApplyEvent \/ ApplyStopped \/ ApplyError

RetryPersistenceSucceeded ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ ui' = "Idle"
    /\ storedSession' = "Ended"
    /\ currentSession' = FALSE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ UNCHANGED <<
        worker, commands, updates, emittedEvents, segments, segmentsFinal,
        quitMode
       >>

RetryPersistenceFailedRecovered ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, lastError, quitMode
       >>

RetryPersistenceFailedRecoveryUnchanged ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = recoveryDurable
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, lastError, quitMode
       >>

RetryPersistence ==
    \/ RetryPersistenceSucceeded
    \/ RetryPersistenceFailedRecovered
    \/ RetryPersistenceFailedRecoveryUnchanged

ExplicitQuitSettled ==
    /\ quitMode = "Running"
    /\ SettledSafe
    /\ quitMode' = "Explicit"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError
       >>

ExplicitQuitWithRecovery ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ quitMode' = "Explicit"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, lastError
       >>

ExplicitQuitRecoveryFailed ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ FailureOwnership
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, recoveryDurable, lastError, quitMode
       >>

(******************************************************************************
The OS quit callback cannot veto process termination.  It makes the same
best-effort snapshot attempt, but the forced-quit failure branch is allowed to
commit without a durable recovery.  Explicit quit never has that branch.
******************************************************************************)
OsQuitSettled ==
    /\ quitMode = "Running"
    /\ SettledSafe
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, recoveryDurable,
        lastError
       >>

OsQuitWithRecovery ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, lastError
       >>

OsQuitRecoveryFailed ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ FailureOwnership
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        launchMetadata, recoveryDurable, lastError
       >>

OsQuitActiveTimeoutRecovered ==
    /\ quitMode = "Running"
    /\ ui \in {"Starting", "Recording", "Stopping"}
    /\ PersistableContext
    /\ recoveryDurable' = TRUE
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, lastError
       >>

OsQuitActiveTimeoutWithoutRecovery ==
    /\ quitMode = "Running"
    /\ ui \in {"Starting", "Recording", "Stopping"}
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ recoveryDurable' = FALSE
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        ui, worker, commands, updates, emittedEvents, segments, segmentsFinal,
        currentSession, storedSession, launchMetadata, lastError
       >>

(******************************************************************************
On the next launch, validated sidecars are reconciled automatically.  This
single-snapshot abstraction represents the first valid sidecar selected by
the production directory scan.  Success closes it; database failure restores
the transcript into the same guarded Failed state so a later retry can
reconcile that selected snapshot without enabling a new recording.
******************************************************************************)
RelaunchRecoverySucceeded ==
    /\ RelaunchCandidate
    /\ ui' = "Idle"
    /\ worker' = "Idle"
    /\ commands' = <<>>
    /\ updates' = <<>>
    /\ emittedEvents' = 0
    /\ segments' = 0
    /\ segmentsFinal' = TRUE
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ quitMode' = "Running"

RelaunchRecoveryFailed ==
    /\ RelaunchCandidate
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ commands' = <<>>
    /\ updates' = <<>>
    /\ emittedEvents' = segments
    /\ segmentsFinal' = TRUE
    /\ FailureOwnership
    /\ lastError' = TRUE
    /\ quitMode' = "Running"
    /\ UNCHANGED <<segments, launchMetadata, recoveryDurable>>

AwaitUserRetry ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ UNCHANGED vars

QuitComplete ==
    /\ quitMode \in {"Explicit", "OS"}
    /\ UNCHANGED vars

UserStep ==
    RequestStart \/ RequestStop \/ RequestStopWhileStarting
    \/ RetryPersistence
    \/ ExplicitQuitSettled \/ ExplicitQuitWithRecovery
    \/ ExplicitQuitRecoveryFailed
    \/ OsQuitSettled \/ OsQuitWithRecovery \/ OsQuitRecoveryFailed
    \/ OsQuitActiveTimeoutRecovered \/ OsQuitActiveTimeoutWithoutRecovery
    \/ AwaitUserRetry

Next ==
    \/ /\ quitMode = "Running"
       /\ (UserStep \/ WorkerStep \/ ApplyUpdate)
    \/ RelaunchRecoverySucceeded
    \/ RelaunchRecoveryFailed
    \/ QuitComplete

RunningWorkerStep == quitMode = "Running" /\ WorkerStep

RunningApplyUpdate == quitMode = "Running" /\ ApplyUpdate

(******************************************************************************
Weak fairness is assumed only for the in-process worker and UI update pump.
There is deliberately no fairness assumption that forces a user to press
Start or Stop.
******************************************************************************)
Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(RunningWorkerStep)
    /\ WF_vars(RunningApplyUpdate)

TypeOK ==
    /\ ui \in UiStates
    /\ worker \in WorkerStates
    /\ commands \in Seq(CommandKinds)
    /\ updates \in Seq(UpdateKinds)
    /\ emittedEvents \in 0..MaxEvents
    /\ segments \in Nat
    /\ segmentsFinal \in BOOLEAN
    /\ currentSession \in BOOLEAN
    /\ storedSession \in StoredSessionStates
    /\ launchMetadata \in BOOLEAN
    /\ recoveryDurable \in BOOLEAN
    /\ lastError \in BOOLEAN
    /\ quitMode \in QuitModes

SingleStartCommand == SeqCount(commands, "Start") <= 1

AppliedEventsWereEmitted == segments <= emittedEvents

EventDebtIsExact ==
    emittedEvents = segments + SeqCount(updates, "Event")

CurrentSessionIsOpen == currentSession => storedSession = "Open"

OpenSessionHasHandle == storedSession = "Open" => currentSession

CurrentSessionHasMetadata == currentSession => launchMetadata

EndedSessionHasNoHandle ==
    storedSession = "Ended" => ~currentSession /\ ~launchMetadata

LaunchMetadataTracksSession ==
    /\ ui \in {"Starting", "Recording", "Stopping"} => launchMetadata
    /\ launchMetadata => ui \in {"Starting", "Recording", "Stopping", "Failed"}

RecoveryRequiresMetadata == recoveryDurable => launchMetadata

SettledStateIsFinal == ui \in {"Idle", "Failed"} => segmentsFinal

IdleStateHasNoCurrentSession ==
    ui = "Idle" => ~currentSession /\ ~launchMetadata

FailedSessionHandleIsRetained ==
    ui = "Failed" /\ currentSession =>
        storedSession = "Open" /\ launchMetadata /\ lastError

ErrorMatchesFailedState == lastError => ui = "Failed"

ExplicitQuitIsGuarded == quitMode = "Explicit" => QuitSafe

StartingEventuallyResolves ==
    (ui = "Starting" /\ quitMode = "Running")
        ~> (ui # "Starting" \/ quitMode # "Running")

StoppingEventuallySettles ==
    (ui = "Stopping" /\ quitMode = "Running")
        ~> (ui \in {"Idle", "Failed"} \/ quitMode # "Running")

=============================================================================
