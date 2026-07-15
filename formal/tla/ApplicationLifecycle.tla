------------------------ MODULE ApplicationLifecycle ------------------------
EXTENDS Integers, Naturals

(******************************************************************************
Whole-application workflow abstraction.  SessionLifecycle models the two FIFO
queues in detail; this module linearizes that protocol and composes it with
top-level navigation, transcript ownership, setup gating, and persistence.

`MaxSegments` bounds transcript size for TLC.  Text, timestamps, rendering,
and independent settings such as the local MCP bridge are intentionally
abstracted away.
******************************************************************************)

CONSTANT MaxSegments

ASSUME MaxSegments \in Nat \ {0}

Views == {"Library", "Live", "History"}
UiStates == {"Idle", "Starting", "Recording", "Stopping", "Failed"}
WorkerStates == {"Idle", "Starting", "Running", "Stopping"}
Owners == {"None", "Live", "History"}
StoredSessionStates == {"None", "Open", "Ended"}
QuitModes == {"Running", "Explicit", "OS"}
ActiveStates == {"Starting", "Recording", "Stopping"}
TerminalStates == {"Idle", "Failed"}

VARIABLES
    view,
    ui,
    worker,
    segmentOwner,
    segments,
    currentSession,
    storedSession,
    viewedSession,
    historyAvailable,
    launchMetadata,
    recoveryDurable,
    lastError,
    setupReady,
    cancelPending,
    quitMode

vars == <<
    view,
    ui,
    worker,
    segmentOwner,
    segments,
    currentSession,
    storedSession,
    viewedSession,
    historyAvailable,
    launchMetadata,
    recoveryDurable,
    lastError,
    setupReady,
    cancelPending,
    quitMode
>>

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
    /\ ui \in TerminalStates
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
           /\ ui \in ActiveStates

Init ==
    /\ view = "Library"
    /\ ui = "Idle"
    /\ worker = "Idle"
    /\ segmentOwner = "None"
    /\ segments = 0
    /\ currentSession = FALSE
    /\ storedSession = "None"
    /\ viewedSession = FALSE
    /\ historyAvailable = FALSE
    /\ launchMetadata = FALSE
    /\ recoveryDurable = FALSE
    /\ lastError = FALSE
    /\ setupReady = FALSE
    /\ cancelPending = FALSE
    /\ quitMode = "Running"

CompleteSetup ==
    /\ ~setupReady
    /\ setupReady' = TRUE
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, currentSession,
        storedSession, viewedSession, historyAvailable, launchMetadata,
        recoveryDurable, lastError, cancelPending, quitMode
       >>

EnterNewSession ==
    /\ ui \in TerminalStates
    /\ worker = "Idle"
    /\ ~currentSession
    /\ storedSession # "Open"
    /\ ~launchMetadata
    /\ view' = "Live"
    /\ ui' = "Idle"
    /\ segmentOwner' = "Live"
    /\ segments' = 0
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ viewedSession' = FALSE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<worker, historyAvailable, setupReady, quitMode>>

(******************************************************************************
Starting through the global menu first normalizes the view to Live.  This is
the guard that prevents History(old) + Recording(new) and Library + Running.
******************************************************************************)
RequestStart ==
    /\ ui \in TerminalStates
    /\ worker = "Idle"
    /\ ~currentSession
    /\ storedSession # "Open"
    /\ ~launchMetadata
    /\ setupReady
    /\ view' = "Live"
    /\ ui' = "Starting"
    /\ worker' = "Starting"
    /\ segmentOwner' = "Live"
    /\ segments' = 0
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ viewedSession' = FALSE
    /\ launchMetadata' = TRUE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<historyAvailable, setupReady, quitMode>>

StartSucceededWithStorage ==
    /\ ui = "Starting"
    /\ worker = "Starting"
    /\ ~cancelPending
    /\ ui' = "Recording"
    /\ worker' = "Running"
    /\ currentSession' = TRUE
    /\ storedSession' = "Open"
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata, recoveryDurable, lastError,
        setupReady, cancelPending, quitMode
       >>

StartSucceededWithoutStorage ==
    /\ ui = "Starting"
    /\ worker = "Starting"
    /\ ~cancelPending
    /\ ui' = "Recording"
    /\ worker' = "Running"
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata, recoveryDurable, lastError,
        setupReady, cancelPending, quitMode
       >>

StartFailureReady ==
    /\ worker = "Starting"
    /\ \/ /\ ui = "Starting"
           /\ ~cancelPending
       \/ /\ ui = "Stopping"
           /\ cancelPending

StartFailedWithoutSegments ==
    /\ StartFailureReady
    /\ segments = 0
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession,
        historyAvailable, setupReady, quitMode
       >>

StartFailedWithSegmentsSucceeded ==
    /\ StartFailureReady
    /\ segments > 0
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ historyAvailable' = TRUE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, setupReady, quitMode
       >>

StartFailedWithSegmentsPersistenceFailedRecovered ==
    /\ StartFailureReady
    /\ segments > 0
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ lastError' = TRUE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, historyAvailable,
        launchMetadata, setupReady, quitMode
       >>

StartFailedWithSegmentsPersistenceFailedWithoutRecovery ==
    /\ StartFailureReady
    /\ segments > 0
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ FailureOwnership
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, historyAvailable,
        launchMetadata, setupReady, quitMode
       >>

StartFailed ==
    \/ StartFailedWithoutSegments
    \/ StartFailedWithSegmentsSucceeded
    \/ StartFailedWithSegmentsPersistenceFailedRecovered
    \/ StartFailedWithSegmentsPersistenceFailedWithoutRecovery

(******************************************************************************
Stopping while native start is still in flight is a real graceful-quit path.
The start result must first settle before the pending stop can finish.
******************************************************************************)
RequestStopWhileStarting ==
    /\ ui = "Starting"
    /\ worker = "Starting"
    /\ ~cancelPending
    /\ ui' = "Stopping"
    /\ cancelPending' = TRUE
    /\ UNCHANGED <<
        view, worker, segmentOwner, segments, currentSession, storedSession,
        viewedSession, historyAvailable, launchMetadata, recoveryDurable,
        lastError, setupReady, quitMode
       >>

CanceledStartSucceededWithStorage ==
    /\ ui = "Stopping"
    /\ worker = "Starting"
    /\ cancelPending
    /\ worker' = "Stopping"
    /\ currentSession' = TRUE
    /\ storedSession' = "Open"
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, ui, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata, recoveryDurable, lastError,
        setupReady, quitMode
       >>

CanceledStartSucceededWithoutStorage ==
    /\ ui = "Stopping"
    /\ worker = "Starting"
    /\ cancelPending
    /\ worker' = "Stopping"
    /\ currentSession' = FALSE
    /\ storedSession' = "None"
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, ui, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata, recoveryDurable, lastError,
        setupReady, quitMode
       >>

ResolveStart ==
    \/ StartSucceededWithStorage
    \/ StartSucceededWithoutStorage
    \/ StartFailed
    \/ CanceledStartSucceededWithStorage
    \/ CanceledStartSucceededWithoutStorage

(******************************************************************************
Swift may flush microphone results while rolling back a failed native start.
They are delivered before Error, so the linearized model admits transcript
growth while the worker is still Starting.  StartFailed then persists the
partial transcript or retains it behind the recovery guard.
******************************************************************************)
ReceiveFailedStartSegment ==
    /\ worker = "Starting"
    /\ ui \in {"Starting", "Stopping"}
    /\ launchMetadata
    /\ segments < MaxSegments
    /\ segments' = segments + 1
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, currentSession, storedSession,
        viewedSession, historyAvailable, launchMetadata, recoveryDurable,
        lastError, setupReady, cancelPending, quitMode
       >>

ReceiveSegment ==
    /\ ui \in {"Recording", "Stopping"}
    /\ worker \in {"Running", "Stopping"}
    /\ segments < MaxSegments
    /\ segments' = segments + 1
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, currentSession, storedSession,
        viewedSession, historyAvailable, launchMetadata, recoveryDurable,
        lastError, setupReady, cancelPending, quitMode
       >>

RequestStop ==
    /\ ui = "Recording"
    /\ worker = "Running"
    /\ ui' = "Stopping"
    /\ worker' = "Stopping"
    /\ cancelPending' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, currentSession, storedSession,
        viewedSession, historyAvailable, launchMetadata, recoveryDurable,
        lastError, setupReady, quitMode
       >>

FinishStopSucceeded ==
    /\ ui = "Stopping"
    /\ worker = "Stopping"
    /\ ~cancelPending
    /\ PersistableContext
    /\ ui' = "Idle"
    /\ worker' = "Idle"
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ historyAvailable' = TRUE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, lastError, setupReady,
        cancelPending, quitMode
       >>

(******************************************************************************
Finalization creates a missing row when Started could not do so.  On any
database failure the launch metadata and optional open-row handle are retained
while an atomic recovery snapshot is attempted.  Snapshot success and failure
are separate branches because they have different quit safety.
******************************************************************************)
FinishStopPersistenceFailedRecovered ==
    /\ ui = "Stopping"
    /\ worker = "Stopping"
    /\ ~cancelPending
    /\ PersistableContext
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, historyAvailable,
        launchMetadata, setupReady, cancelPending, quitMode
       >>

FinishStopPersistenceFailedWithoutRecovery ==
    /\ ui = "Stopping"
    /\ worker = "Stopping"
    /\ ~cancelPending
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ FailureOwnership
    /\ recoveryDurable' = FALSE
    /\ lastError' = TRUE
    /\ UNCHANGED <<
        view, segmentOwner, segments, viewedSession, historyAvailable,
        launchMetadata, setupReady, cancelPending, quitMode
       >>

FinishStop ==
    \/ FinishStopSucceeded
    \/ FinishStopPersistenceFailedRecovered
    \/ FinishStopPersistenceFailedWithoutRecovery

RetryPersistenceSucceeded ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ ui' = "Idle"
    /\ storedSession' = "Ended"
    /\ currentSession' = FALSE
    /\ historyAvailable' = TRUE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ UNCHANGED <<
        view, worker, segmentOwner, segments, viewedSession, setupReady,
        cancelPending, quitMode
       >>

RetryPersistenceFailedRecovered ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = TRUE
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending, quitMode
       >>

RetryPersistenceFailedRecoveryUnchanged ==
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ lastError
    /\ FailureOwnership
    /\ recoveryDurable' = recoveryDurable
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending, quitMode
       >>

RetryPersistence ==
    \/ RetryPersistenceSucceeded
    \/ RetryPersistenceFailedRecovered
    \/ RetryPersistenceFailedRecoveryUnchanged

(******************************************************************************
Navigation is legal only when the worker is idle and the UI is terminal.
The Rust AppModel repeats this guard so callers cannot bypass the UI button.
******************************************************************************)
NavigateToLibrary ==
    /\ ui \in TerminalStates
    /\ worker = "Idle"
    /\ ~currentSession
    /\ storedSession # "Open"
    /\ ~launchMetadata
    /\ view' = "Library"
    /\ segmentOwner' = "None"
    /\ segments' = 0
    /\ currentSession' = FALSE
    /\ viewedSession' = FALSE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ UNCHANGED <<
        ui, worker, storedSession, historyAvailable, setupReady,
        cancelPending, quitMode
       >>

OpenHistory ==
    /\ view = "Library"
    /\ ui \in TerminalStates
    /\ worker = "Idle"
    /\ ~currentSession
    /\ storedSession # "Open"
    /\ ~launchMetadata
    /\ historyAvailable
    /\ view' = "History"
    /\ segmentOwner' = "History"
    /\ segments' = 1
    /\ currentSession' = FALSE
    /\ viewedSession' = TRUE
    /\ UNCHANGED <<
        ui, worker, storedSession, historyAvailable, launchMetadata,
        recoveryDurable, lastError, setupReady, cancelPending, quitMode
       >>

(******************************************************************************
Explicit quit may commit only after persistence settled or a recovery snapshot
was durably written.  The OS callback is unable to veto termination, so its
failure branch records the same best-effort attempt without the explicit guard.
******************************************************************************)
ExplicitQuitSettled ==
    /\ quitMode = "Running"
    /\ SettledSafe
    /\ quitMode' = "Explicit"
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, currentSession,
        storedSession, viewedSession, historyAvailable, launchMetadata,
        recoveryDurable, lastError, setupReady, cancelPending
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
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending
       >>

ExplicitQuitRecoveryFailed ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ lastError
    /\ FailureOwnership
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata, recoveryDurable, lastError,
        setupReady, cancelPending, quitMode
       >>

OsQuitSettled ==
    /\ quitMode = "Running"
    /\ SettledSafe
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, currentSession,
        storedSession, viewedSession, historyAvailable, launchMetadata,
        recoveryDurable, lastError, setupReady, cancelPending
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
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending
       >>

OsQuitRecoveryFailed ==
    /\ quitMode = "Running"
    /\ ui = "Failed"
    /\ worker = "Idle"
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ lastError
    /\ FailureOwnership
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, viewedSession,
        historyAvailable, launchMetadata,
        recoveryDurable, lastError, setupReady, cancelPending
       >>

OsQuitActiveTimeoutRecovered ==
    /\ quitMode = "Running"
    /\ ui \in ActiveStates
    /\ PersistableContext
    /\ recoveryDurable' = TRUE
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, currentSession,
        storedSession, viewedSession, historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending
       >>

OsQuitActiveTimeoutWithoutRecovery ==
    /\ quitMode = "Running"
    /\ ui \in ActiveStates
    /\ PersistableContext
    /\ ~recoveryDurable
    /\ recoveryDurable' = FALSE
    /\ quitMode' = "OS"
    /\ UNCHANGED <<
        view, ui, worker, segmentOwner, segments, currentSession,
        storedSession, viewedSession, historyAvailable, launchMetadata,
        lastError, setupReady, cancelPending
       >>

(******************************************************************************
On launch, production validates and reconciles recovery sidecars until the
first database failure.  The model tracks one selected valid snapshot: success
returns to a settled library; failure restores its transcript into Live/Failed
with the durable snapshot retained, so new sessions remain guarded and a later
retry can reconcile that selected snapshot.
******************************************************************************)
RelaunchRecoverySucceeded ==
    /\ RelaunchCandidate
    /\ view' = "Library"
    /\ ui' = "Idle"
    /\ worker' = "Idle"
    /\ segmentOwner' = "None"
    /\ segments' = 0
    /\ currentSession' = FALSE
    /\ storedSession' = "Ended"
    /\ viewedSession' = FALSE
    /\ historyAvailable' = TRUE
    /\ launchMetadata' = FALSE
    /\ recoveryDurable' = FALSE
    /\ lastError' = FALSE
    /\ cancelPending' = FALSE
    /\ quitMode' = "Running"
    /\ UNCHANGED setupReady

RelaunchRecoveryFailed ==
    /\ RelaunchCandidate
    /\ view' = "Live"
    /\ ui' = "Failed"
    /\ worker' = "Idle"
    /\ segmentOwner' = "Live"
    /\ viewedSession' = FALSE
    /\ FailureOwnership
    /\ lastError' = TRUE
    /\ cancelPending' = FALSE
    /\ quitMode' = "Running"
    /\ UNCHANGED <<
        segments, historyAvailable, launchMetadata, recoveryDurable, setupReady
       >>

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
    \/ CompleteSetup
    \/ EnterNewSession
    \/ RequestStart
    \/ RequestStop
    \/ RequestStopWhileStarting
    \/ RetryPersistence
    \/ NavigateToLibrary
    \/ OpenHistory
    \/ ExplicitQuitSettled
    \/ ExplicitQuitWithRecovery
    \/ ExplicitQuitRecoveryFailed
    \/ OsQuitSettled
    \/ OsQuitWithRecovery
    \/ OsQuitRecoveryFailed
    \/ OsQuitActiveTimeoutRecovered
    \/ OsQuitActiveTimeoutWithoutRecovery
    \/ AwaitUserRetry

InternalStep ==
    ResolveStart \/ ReceiveFailedStartSegment \/ ReceiveSegment \/ FinishStop

Next ==
    \/ /\ quitMode = "Running"
       /\ (UserStep \/ InternalStep)
    \/ RelaunchRecoverySucceeded
    \/ RelaunchRecoveryFailed
    \/ QuitComplete

RunningResolveStart == quitMode = "Running" /\ ResolveStart

RunningFinishStop == quitMode = "Running" /\ FinishStop

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(RunningResolveStart)
    /\ WF_vars(RunningFinishStop)

TypeOK ==
    /\ view \in Views
    /\ ui \in UiStates
    /\ worker \in WorkerStates
    /\ segmentOwner \in Owners
    /\ segments \in 0..MaxSegments
    /\ currentSession \in BOOLEAN
    /\ storedSession \in StoredSessionStates
    /\ viewedSession \in BOOLEAN
    /\ historyAvailable \in BOOLEAN
    /\ launchMetadata \in BOOLEAN
    /\ recoveryDurable \in BOOLEAN
    /\ lastError \in BOOLEAN
    /\ setupReady \in BOOLEAN
    /\ cancelPending \in BOOLEAN
    /\ quitMode \in QuitModes

ActiveSessionIsLive ==
    ui \in ActiveStates =>
        view = "Live" /\ segmentOwner = "Live" /\ launchMetadata

WorkerMatchesUi ==
    /\ (ui \in TerminalStates => worker = "Idle" /\ ~cancelPending)
    /\ (ui = "Starting" => worker = "Starting" /\ ~cancelPending)
    /\ (ui = "Recording" => worker = "Running" /\ ~cancelPending)
    /\ (ui = "Stopping" =>
            \/ /\ cancelPending
               /\ worker = "Starting"
            \/ /\ ~cancelPending
               /\ worker = "Stopping")

LibraryOwnsNoTranscript ==
    view = "Library" => segmentOwner = "None" /\ segments = 0

HistoryIsIsolated ==
    view = "History" =>
        /\ ui \in TerminalStates
        /\ worker = "Idle"
        /\ segmentOwner = "History"
        /\ viewedSession
        /\ ~currentSession
        /\ ~launchMetadata

LiveOwnsLiveTranscript == view = "Live" => segmentOwner = "Live"

(******************************************************************************
`currentSession` is the open persistence handle, while an ended row combined
with a Live-owned transcript is the abstraction of Rust's retained
`linked_session_id`. Finalisation must release the former without losing the
latter until navigation replaces the visible transcript.
******************************************************************************)
SettledLiveTranscriptIsLinked ==
    (view = "Live" /\ storedSession = "Ended") =>
        /\ segmentOwner = "Live"
        /\ ui \in TerminalStates
        /\ worker = "Idle"
        /\ ~currentSession
        /\ ~launchMetadata

CurrentSessionIsCoherent ==
    currentSession =>
        /\ storedSession = "Open"
        /\ launchMetadata
        /\ view = "Live"
        /\ segmentOwner = "Live"
        /\ \/ /\ ui \in {"Recording", "Stopping"}
               /\ worker \in {"Running", "Stopping"}
           \/ /\ ui = "Failed"
               /\ worker = "Idle"
               /\ lastError

OpenSessionHasHandle == storedSession = "Open" => currentSession

LaunchMetadataIsCoherent ==
    launchMetadata =>
        /\ view = "Live"
        /\ segmentOwner = "Live"
        /\ ui \in {"Starting", "Recording", "Stopping", "Failed"}

RecoveryRequiresMetadata == recoveryDurable => launchMetadata

ErrorMatchesFailure == lastError => ui = "Failed"

ExplicitQuitIsGuarded == quitMode = "Explicit" => QuitSafe

StartingEventuallyResolves ==
    (ui = "Starting" /\ quitMode = "Running")
        ~> (ui # "Starting" \/ quitMode # "Running")

StoppingEventuallySettles ==
    (ui = "Stopping" /\ quitMode = "Running")
        ~> (ui \in {"Idle", "Failed"} \/ quitMode # "Running")

=============================================================================
