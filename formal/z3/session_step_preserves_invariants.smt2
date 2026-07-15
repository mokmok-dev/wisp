; Symbolically check an asynchronous counter abstraction of SessionLifecycle.
; Command/update counts retain causal in-flight state while `segments`,
; `emitted`, `pending_events`, and pending Stop commands remain unbounded.
; `Event` denotes only a result that creates a new persistable segment; Log
; updates and same-segment partial revisions are stuttering projections here.
; TLC remains the source of truth for exact FIFO order.
(set-logic ALL)

(declare-datatypes () ((Ui Idle Starting Recording Stopping Failed)))
(declare-datatypes () ((Worker WorkerIdle WorkerStarting WorkerFailing WorkerRunning WorkerStopping)))
(declare-datatypes () ((Row NoRow OpenRow EndedRow)))
(declare-datatypes () ((QuitMode AppRunning ExplicitQuit OsQuit)))

(define-fun Terminal ((u Ui)) Bool
  (or (= u Idle) (= u Failed)))

(define-fun Active ((u Ui)) Bool
  (or (= u Starting) (= u Recording) (= u Stopping)))

(define-fun Persistable ((metadata Bool) (current Bool) (row Row)) Bool
  (and metadata
       (or (and current (= row OpenRow))
           (and (not current) (= row NoRow)))))

(define-fun FailureOwnership
  ((current Bool) (row Row) (current-next Bool) (row-next Row)) Bool
  (or (and (= current-next current) (= row-next row))
      (and (not current) (= row NoRow) current-next (= row-next OpenRow))))

(define-fun RelaunchCandidate
  ((quit QuitMode) (u Ui) (w Worker) (recovery Bool)
   (metadata Bool) (current Bool) (row Row)) Bool
  (and recovery
       (Persistable metadata current row)
       (or
         (and (or (= quit ExplicitQuit) (= quit OsQuit))
              (= u Failed) (= w WorkerIdle))
         (and (= quit OsQuit) (Active u)))))

(define-fun SettledSafe
  ((u Ui) (w Worker) (row Row) (metadata Bool) (current Bool)) Bool
  (and (Terminal u) (= w WorkerIdle) (not metadata) (not current)
       (not (= row OpenRow))))

(define-fun QuitSafe
  ((u Ui) (w Worker) (row Row) (metadata Bool) (current Bool)
   (recovery Bool)) Bool
  (or (SettledSafe u w row metadata current) recovery))

(define-fun Inv
  ((u Ui) (w Worker) (row Row)
   (segments Int) (emitted Int) (pending_events Int)
   (finalized Bool) (current Bool) (metadata Bool) (recovery Bool)
   (has_error Bool) (quit QuitMode)
   (start_commands Int) (stop_commands Int)
   (started_updates Int) (stopped_updates Int) (error_updates Int)) Bool
  (and
    (>= segments 0)
    (>= emitted 0)
    (>= pending_events 0)
    (>= start_commands 0) (<= start_commands 1)
    ; A stale Stop can survive a failed start and overlap a later request, so
    ; this counter is intentionally unbounded.
    (>= stop_commands 0)
    (>= started_updates 0) (<= started_updates 1)
    (>= stopped_updates 0) (<= stopped_updates 1)
    (>= error_updates 0) (<= error_updates 1)
    (= emitted (+ segments pending_events))
    (=> (> error_updates 0)
      (and (= started_updates 0) (= stopped_updates 0) (= w WorkerIdle)))
    (=> current (= row OpenRow))
    (=> (= row OpenRow) current)
    (=> current metadata)
    (=> recovery metadata)
    (=> (= row EndedRow) (and (not current) (not metadata)))
    (=> (= u Idle) (and (not current) (not metadata)))
    (=> has_error (= u Failed))
    (=> (= u Failed) has_error)
    (=> (Active u) metadata)
    (=> metadata (or (Active u) (= u Failed)))
    (=> current (or (= u Recording) (= u Stopping) (= u Failed)))
    (=> (= w WorkerStarting) (or (= u Starting) (= u Stopping)))
    (=> (= w WorkerFailing) (or (= u Starting) (= u Stopping)))
    (=> (= w WorkerRunning)
      (or (= u Starting) (= u Recording) (= u Stopping)))
    (=> (= w WorkerStopping) (= u Stopping))
    (=> (Terminal u)
      (and (= w WorkerIdle)
           finalized
           (= pending_events 0)
           (= started_updates 0)
           (= stopped_updates 0)
           (= error_updates 0)))
    (=> (= quit ExplicitQuit)
      (QuitSafe u w row metadata current recovery))))

(declare-const ui Ui)
(declare-const worker Worker)
(declare-const row Row)
(declare-const segments Int)
(declare-const emitted Int)
(declare-const pending_events Int)
(declare-const finalized Bool)
(declare-const current Bool)
(declare-const metadata Bool)
(declare-const recovery Bool)
(declare-const has_error Bool)
(declare-const quit QuitMode)
(declare-const start_commands Int)
(declare-const stop_commands Int)
(declare-const started_updates Int)
(declare-const stopped_updates Int)
(declare-const error_updates Int)

(declare-const ui_next Ui)
(declare-const worker_next Worker)
(declare-const row_next Row)
(declare-const segments_next Int)
(declare-const emitted_next Int)
(declare-const pending_events_next Int)
(declare-const finalized_next Bool)
(declare-const current_next Bool)
(declare-const metadata_next Bool)
(declare-const recovery_next Bool)
(declare-const has_error_next Bool)
(declare-const quit_next QuitMode)
(declare-const start_commands_next Int)
(declare-const stop_commands_next Int)
(declare-const started_updates_next Int)
(declare-const stopped_updates_next Int)
(declare-const error_updates_next Int)

(define-fun SameUi () Bool (= ui_next ui))
(define-fun SameWorker () Bool (= worker_next worker))
(define-fun SameRow () Bool (= row_next row))
(define-fun SameSegments () Bool (= segments_next segments))
(define-fun SameEmitted () Bool (= emitted_next emitted))
(define-fun SamePendingEvents () Bool (= pending_events_next pending_events))
(define-fun SameFinalized () Bool (= finalized_next finalized))
(define-fun SameCurrent () Bool (= current_next current))
(define-fun SameMetadata () Bool (= metadata_next metadata))
(define-fun SameRecovery () Bool (= recovery_next recovery))
(define-fun SameError () Bool (= has_error_next has_error))
(define-fun SameQuit () Bool (= quit_next quit))
(define-fun SameStartCommands () Bool (= start_commands_next start_commands))
(define-fun SameStopCommands () Bool (= stop_commands_next stop_commands))
(define-fun SameStartedUpdates () Bool (= started_updates_next started_updates))
(define-fun SameStoppedUpdates () Bool (= stopped_updates_next stopped_updates))
(define-fun SameErrorUpdates () Bool (= error_updates_next error_updates))
(define-fun OperationalQuit () Bool (and (= quit AppRunning) SameQuit))

(define-fun Begin () Bool
  (and
    OperationalQuit
    (Terminal ui)
    (= worker WorkerIdle)
    (not current)
    (not (= row OpenRow))
    (not metadata)
    (= start_commands 0)
    (= ui_next Starting)
    SameWorker
    (= row_next NoRow)
    (= segments_next 0)
    (= emitted_next 0)
    (= pending_events_next 0)
    finalized_next
    (not current_next)
    metadata_next
    (not recovery_next)
    (not has_error_next)
    (= start_commands_next 1)
    SameStopCommands SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun RequestStop () Bool
  (and
    OperationalQuit
    (= ui Recording)
    (= ui_next Stopping)
    SameWorker SameRow SameSegments SameEmitted SamePendingEvents SameFinalized
    SameCurrent SameMetadata SameRecovery SameError SameStartCommands
    (= stop_commands_next (+ stop_commands 1))
    SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun RequestStopWhileStarting () Bool
  (and
    OperationalQuit
    (= ui Starting)
    (= ui_next Stopping)
    SameWorker SameRow SameSegments SameEmitted SamePendingEvents SameFinalized
    SameCurrent SameMetadata SameRecovery SameError SameStartCommands
    (= stop_commands_next (+ stop_commands 1))
    SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun WorkerTakeStart () Bool
  (and
    OperationalQuit
    (= worker WorkerIdle)
    (or (= ui Starting) (= ui Stopping))
    (> start_commands 0)
    (= pending_events 0)
    (= started_updates 0)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi
    (= worker_next WorkerStarting)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError
    (= start_commands_next (- start_commands 1))
    SameStopCommands SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun WorkerStartSucceeded () Bool
  (and
    OperationalQuit
    (= worker WorkerStarting)
    (= started_updates 0)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi
    (= worker_next WorkerRunning)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError SameStartCommands SameStopCommands
    (= started_updates_next 1)
    SameStoppedUpdates SameErrorUpdates))

(define-fun WorkerBeginStartFailure () Bool
  (and
    OperationalQuit
    (= worker WorkerStarting)
    (= started_updates 0)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi
    (= worker_next WorkerFailing)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError SameStartCommands SameStopCommands
    SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun WorkerFlushFailedStartEvent () Bool
  (and
    OperationalQuit
    (= worker WorkerFailing)
    (= started_updates 0)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi SameWorker SameRow SameSegments
    (= emitted_next (+ emitted 1))
    (= pending_events_next (+ pending_events 1))
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun WorkerFinishStartFailure () Bool
  (and
    OperationalQuit
    (= worker WorkerFailing)
    (= started_updates 0)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi
    (= worker_next WorkerIdle)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError SameStartCommands SameStopCommands
    SameStartedUpdates SameStoppedUpdates
    (= error_updates_next 1)))

(define-fun WorkerEmitEvent () Bool
  (and
    OperationalQuit
    (= worker WorkerRunning)
    SameUi SameWorker SameRow SameSegments
    (= emitted_next (+ emitted 1))
    (= pending_events_next (+ pending_events 1))
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun WorkerTakeStop () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerRunning)
    (> stop_commands 0)
    SameUi
    (= worker_next WorkerStopping)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError SameStartCommands
    (= stop_commands_next (- stop_commands 1))
    SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun WorkerFlushEvent () Bool
  (and
    OperationalQuit
    (= worker WorkerStopping)
    SameUi SameWorker SameRow SameSegments
    (= emitted_next (+ emitted 1))
    (= pending_events_next (+ pending_events 1))
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun WorkerFinishStop () Bool
  (and
    OperationalQuit
    (= worker WorkerStopping)
    (= stopped_updates 0)
    (= error_updates 0)
    SameUi
    (= worker_next WorkerIdle)
    SameRow SameSegments SameEmitted SamePendingEvents SameFinalized SameCurrent
    SameMetadata SameRecovery SameError SameStartCommands SameStopCommands
    SameStartedUpdates
    (= stopped_updates_next 1)
    SameErrorUpdates))

(define-fun WorkerIgnoreStop () Bool
  (and
    OperationalQuit
    (= worker WorkerIdle)
    (> stop_commands 0)
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    SameStartCommands
    (= stop_commands_next (- stop_commands 1))
    SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun ApplyStarted () Bool
  (and
    OperationalQuit
    (> started_updates 0)
    (or (= ui Starting) (= ui Stopping))
    metadata
    (or (= worker WorkerRunning) (= worker WorkerStopping)
        (= worker WorkerIdle))
    (= ui_next (ite (= ui Stopping) Stopping Recording))
    SameWorker
    (or
      (and current_next (= row_next OpenRow))
      (and (not current_next) (= row_next NoRow)))
    SameSegments SameEmitted SamePendingEvents SameFinalized SameMetadata
    SameRecovery SameError SameStartCommands SameStopCommands
    (= started_updates_next (- started_updates 1))
    SameStoppedUpdates SameErrorUpdates))

(define-fun ApplyEvent () Bool
  (and
    OperationalQuit
    (> pending_events 0)
    (or (= ui Starting) (= ui Recording) (= ui Stopping))
    SameUi SameWorker SameRow
    (= segments_next (+ segments 1))
    SameEmitted
    (= pending_events_next (- pending_events 1))
    (not finalized_next)
    SameCurrent SameMetadata SameRecovery SameError SameStartCommands
    SameStopCommands SameStartedUpdates SameStoppedUpdates SameErrorUpdates))

(define-fun ApplyStopped () Bool
  (and
    OperationalQuit
    (> stopped_updates 0)
    (= started_updates 0)
    (= error_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    (= ui_next Idle)
    SameWorker
    (= row_next EndedRow)
    SameSegments SameEmitted SamePendingEvents
    finalized_next
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    SameError SameStartCommands SameStopCommands SameStartedUpdates
    (= stopped_updates_next (- stopped_updates 1))
    SameErrorUpdates))

(define-fun ApplyStoppedErrorRecovered () Bool
  (and
    OperationalQuit
    (> stopped_updates 0)
    (= started_updates 0)
    (= error_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    (= ui_next Failed)
    SameWorker SameSegments SameEmitted SamePendingEvents
    finalized_next
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates
    (= stopped_updates_next (- stopped_updates 1))
    SameErrorUpdates))

(define-fun ApplyStoppedErrorWithoutRecovery () Bool
  (and
    OperationalQuit
    (> stopped_updates 0)
    (= started_updates 0)
    (= error_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    (not recovery)
    (= ui_next Failed)
    SameWorker SameSegments SameEmitted SamePendingEvents
    finalized_next
    (FailureOwnership current row current_next row_next)
    SameMetadata
    (not recovery_next)
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates
    (= stopped_updates_next (- stopped_updates 1))
    SameErrorUpdates))

(define-fun ApplyErrorWithoutSegments () Bool
  (and
    OperationalQuit
    (> error_updates 0)
    (= started_updates 0)
    (= stopped_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Starting) (= ui Stopping))
    (= segments 0)
    (= ui_next Failed)
    SameWorker
    (= row_next NoRow)
    SameSegments SameEmitted SamePendingEvents
    finalized_next
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    (= error_updates_next (- error_updates 1))))

(define-fun ApplyErrorWithSegmentsSucceeded () Bool
  (and
    OperationalQuit
    (> error_updates 0)
    (= started_updates 0)
    (= stopped_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Starting) (= ui Stopping))
    (> segments 0)
    (Persistable metadata current row)
    (= ui_next Failed)
    SameWorker
    (= row_next EndedRow)
    SameSegments SameEmitted SamePendingEvents
    finalized_next
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    (= error_updates_next (- error_updates 1))))

(define-fun ApplyErrorWithSegmentsFailedRecovered () Bool
  (and
    OperationalQuit
    (> error_updates 0)
    (= started_updates 0)
    (= stopped_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Starting) (= ui Stopping))
    (> segments 0)
    (Persistable metadata current row)
    (= ui_next Failed)
    SameWorker SameSegments SameEmitted SamePendingEvents
    finalized_next
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    (= error_updates_next (- error_updates 1))))

(define-fun ApplyErrorWithSegmentsFailedWithoutRecovery () Bool
  (and
    OperationalQuit
    (> error_updates 0)
    (= started_updates 0)
    (= stopped_updates 0)
    (= worker WorkerIdle)
    (= pending_events 0)
    (or (= ui Starting) (= ui Stopping))
    (> segments 0)
    (Persistable metadata current row)
    (not recovery)
    (= ui_next Failed)
    SameWorker SameSegments SameEmitted SamePendingEvents
    finalized_next
    (FailureOwnership current row current_next row_next)
    SameMetadata
    (not recovery_next)
    has_error_next
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    (= error_updates_next (- error_updates 1))))

(define-fun RetrySucceeded () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    (= ui_next Idle)
    SameWorker
    (= row_next EndedRow)
    SameSegments SameEmitted SamePendingEvents SameFinalized
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    (not has_error_next)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun RetryFailedRecovered () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameError SameStartCommands SameStopCommands SameStartedUpdates
    SameStoppedUpdates SameErrorUpdates))

(define-fun RetryFailedRecoveryUnchanged () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun ExplicitQuitSettled () Bool
  (and
    (= quit AppRunning)
    (SettledSafe ui worker row metadata current)
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    (= quit_next ExplicitQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun ExplicitQuitWithRecovery () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameError
    (= quit_next ExplicitQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun ExplicitQuitRecoveryFailed () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    (not recovery)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun OsQuitSettled () Bool
  (and
    (= quit AppRunning)
    (SettledSafe ui worker row metadata current)
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    (= quit_next OsQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun OsQuitWithRecovery () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameError
    (= quit_next OsQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun OsQuitRecoveryFailed () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    (not recovery)
    has_error
    SameUi SameWorker SameSegments SameEmitted SamePendingEvents
    SameFinalized
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameError
    (= quit_next OsQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun OsQuitActiveTimeoutRecovered () Bool
  (and
    (= quit AppRunning)
    (or (= ui Starting) (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata
    recovery_next
    SameError
    (= quit_next OsQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun OsQuitActiveTimeoutWithoutRecovery () Bool
  (and
    (= quit AppRunning)
    (or (= ui Starting) (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    (not recovery)
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata
    (not recovery_next)
    SameError
    (= quit_next OsQuit)
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun RelaunchRecoverySucceeded () Bool
  (and
    (RelaunchCandidate quit ui worker recovery metadata current row)
    (= ui_next Idle)
    (= worker_next WorkerIdle)
    (= row_next EndedRow)
    (= segments_next 0)
    (= emitted_next 0)
    (= pending_events_next 0)
    finalized_next
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    (not has_error_next)
    (= quit_next AppRunning)
    (= start_commands_next 0)
    (= stop_commands_next 0)
    (= started_updates_next 0)
    (= stopped_updates_next 0)
    (= error_updates_next 0)))

(define-fun RelaunchRecoveryFailed () Bool
  (and
    (RelaunchCandidate quit ui worker recovery metadata current row)
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (= emitted_next segments_next)
    (= pending_events_next 0)
    finalized_next
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery
    has_error_next
    (= quit_next AppRunning)
    (= start_commands_next 0)
    (= stop_commands_next 0)
    (= started_updates_next 0)
    (= stopped_updates_next 0)
    (= error_updates_next 0)))

(define-fun AwaitUserRetry () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata SameRecovery SameError
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun QuitComplete () Bool
  (and
    (or (= quit ExplicitQuit) (= quit OsQuit))
    SameUi SameWorker SameRow SameSegments SameEmitted SamePendingEvents
    SameFinalized SameCurrent SameMetadata SameRecovery SameError SameQuit
    SameStartCommands SameStopCommands SameStartedUpdates SameStoppedUpdates
    SameErrorUpdates))

(define-fun Step () Bool
  (or Begin RequestStop RequestStopWhileStarting WorkerTakeStart
      WorkerStartSucceeded WorkerBeginStartFailure WorkerFlushFailedStartEvent
      WorkerFinishStartFailure WorkerEmitEvent WorkerTakeStop WorkerFlushEvent
      WorkerFinishStop WorkerIgnoreStop ApplyStarted ApplyEvent ApplyStopped
      ApplyStoppedErrorRecovered ApplyStoppedErrorWithoutRecovery
      ApplyErrorWithoutSegments ApplyErrorWithSegmentsSucceeded
      ApplyErrorWithSegmentsFailedRecovered
      ApplyErrorWithSegmentsFailedWithoutRecovery RetrySucceeded
      RetryFailedRecovered
      RetryFailedRecoveryUnchanged ExplicitQuitSettled
      ExplicitQuitWithRecovery ExplicitQuitRecoveryFailed OsQuitSettled
      OsQuitWithRecovery OsQuitRecoveryFailed OsQuitActiveTimeoutRecovered
      OsQuitActiveTimeoutWithoutRecovery
      RelaunchRecoverySucceeded RelaunchRecoveryFailed AwaitUserRetry
      QuitComplete))

(define-fun PreInv () Bool
  (Inv ui worker row segments emitted pending_events finalized current metadata
       recovery has_error quit start_commands stop_commands started_updates
       stopped_updates error_updates))

(define-fun PostInv () Bool
  (Inv ui_next worker_next row_next segments_next emitted_next
       pending_events_next finalized_next current_next metadata_next
       recovery_next has_error_next quit_next start_commands_next
       stop_commands_next started_updates_next stopped_updates_next
       error_updates_next))

(echo "EXPECT-ACTION-COUNT: 37")
(echo "EXPECT-ACTIONS: Begin,RequestStop,RequestStopWhileStarting,WorkerTakeStart,WorkerStartSucceeded,WorkerBeginStartFailure,WorkerFlushFailedStartEvent,WorkerFinishStartFailure,WorkerEmitEvent,WorkerTakeStop,WorkerFlushEvent,WorkerFinishStop,WorkerIgnoreStop,ApplyStarted,ApplyEvent,ApplyStopped,ApplyStoppedErrorRecovered,ApplyStoppedErrorWithoutRecovery,ApplyErrorWithoutSegments,ApplyErrorWithSegmentsSucceeded,ApplyErrorWithSegmentsFailedRecovered,ApplyErrorWithSegmentsFailedWithoutRecovery,RetrySucceeded,RetryFailedRecovered,RetryFailedRecoveryUnchanged,ExplicitQuitSettled,ExplicitQuitWithRecovery,ExplicitQuitRecoveryFailed,OsQuitSettled,OsQuitWithRecovery,OsQuitRecoveryFailed,OsQuitActiveTimeoutRecovered,OsQuitActiveTimeoutWithoutRecovery,RelaunchRecoverySucceeded,RelaunchRecoveryFailed,AwaitUserRetry,QuitComplete")

; Base case: the concrete initial state must satisfy the invariant.
(echo "EXPECT-UNSAT: base-case")
(push)
(assert
  (not (Inv Idle WorkerIdle NoRow 0 0 0 true false false false false
            AppRunning 0 0 0 0 0)))
(check-sat)
(pop)

; Anti-vacuity: every Step action has exactly one declared witness.
(echo "EXPECT-SAT: action Begin")
(push) (assert PreInv) (assert Begin) (check-sat) (pop)
(echo "EXPECT-SAT: action RequestStop")
(push) (assert PreInv) (assert RequestStop) (check-sat) (pop)
(echo "EXPECT-SAT: action RequestStopWhileStarting")
(push) (assert PreInv) (assert RequestStopWhileStarting) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerTakeStart")
(push) (assert PreInv) (assert WorkerTakeStart) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerStartSucceeded")
(push) (assert PreInv) (assert WorkerStartSucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerBeginStartFailure")
(push) (assert PreInv) (assert WorkerBeginStartFailure) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerFlushFailedStartEvent")
(push) (assert PreInv) (assert WorkerFlushFailedStartEvent) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerFinishStartFailure")
(push) (assert PreInv) (assert WorkerFinishStartFailure) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerEmitEvent")
(push) (assert PreInv) (assert WorkerEmitEvent) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerTakeStop")
(push) (assert PreInv) (assert WorkerTakeStop) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerFlushEvent")
(push) (assert PreInv) (assert WorkerFlushEvent) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerFinishStop")
(push) (assert PreInv) (assert WorkerFinishStop) (check-sat) (pop)
(echo "EXPECT-SAT: action WorkerIgnoreStop")
(push) (assert PreInv) (assert WorkerIgnoreStop) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyStarted")
(push) (assert PreInv) (assert ApplyStarted) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyEvent")
(push) (assert PreInv) (assert ApplyEvent) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyStopped")
(push) (assert PreInv) (assert ApplyStopped) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyStoppedErrorRecovered")
(push) (assert PreInv) (assert ApplyStoppedErrorRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyStoppedErrorWithoutRecovery")
(push) (assert PreInv) (assert ApplyStoppedErrorWithoutRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyErrorWithoutSegments")
(push) (assert PreInv) (assert ApplyErrorWithoutSegments) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyErrorWithSegmentsSucceeded")
(push) (assert PreInv) (assert ApplyErrorWithSegmentsSucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyErrorWithSegmentsFailedRecovered")
(push) (assert PreInv) (assert ApplyErrorWithSegmentsFailedRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action ApplyErrorWithSegmentsFailedWithoutRecovery")
(push) (assert PreInv) (assert ApplyErrorWithSegmentsFailedWithoutRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action RetrySucceeded")
(push) (assert PreInv) (assert RetrySucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action RetryFailedRecovered")
(push) (assert PreInv) (assert RetryFailedRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action RetryFailedRecoveryUnchanged")
(push) (assert PreInv) (assert RetryFailedRecoveryUnchanged) (check-sat) (pop)
(echo "EXPECT-SAT: action ExplicitQuitSettled")
(push) (assert PreInv) (assert ExplicitQuitSettled) (check-sat) (pop)
(echo "EXPECT-SAT: action ExplicitQuitWithRecovery")
(push) (assert PreInv) (assert ExplicitQuitWithRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action ExplicitQuitRecoveryFailed")
(push) (assert PreInv) (assert ExplicitQuitRecoveryFailed) (check-sat) (pop)
(echo "EXPECT-SAT: action OsQuitSettled")
(push) (assert PreInv) (assert OsQuitSettled) (check-sat) (pop)
(echo "EXPECT-SAT: action OsQuitWithRecovery")
(push) (assert PreInv) (assert OsQuitWithRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action OsQuitRecoveryFailed")
(push) (assert PreInv) (assert OsQuitRecoveryFailed) (check-sat) (pop)
(echo "EXPECT-SAT: action OsQuitActiveTimeoutRecovered")
(push) (assert PreInv) (assert OsQuitActiveTimeoutRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action OsQuitActiveTimeoutWithoutRecovery")
(push) (assert PreInv) (assert OsQuitActiveTimeoutWithoutRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action RelaunchRecoverySucceeded")
(push) (assert PreInv) (assert RelaunchRecoverySucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action RelaunchRecoveryFailed")
(push) (assert PreInv) (assert RelaunchRecoveryFailed) (check-sat) (pop)
(echo "EXPECT-SAT: action AwaitUserRetry")
(push) (assert PreInv) (assert AwaitUserRetry) (check-sat) (pop)
(echo "EXPECT-SAT: action QuitComplete")
(push) (assert PreInv) (assert QuitComplete) (check-sat) (pop)

; Coverage for the cross-generation case: a stale Stop remains after a failed
; start and a later start cancellation enqueues a second Stop.
(echo "EXPECT-SAT: coverage accumulated Stop commands")
(push)
(assert PreInv)
(assert RequestStopWhileStarting)
(assert (= stop_commands 1))
(assert (= stop_commands_next 2))
(check-sat)
(pop)

; A failed native start can preserve a flushed microphone result.
(echo "EXPECT-SAT: coverage failed start preserves flushed event")
(push)
(assert PreInv)
(assert ApplyErrorWithSegmentsSucceeded)
(assert (= segments 1))
(assert (= row NoRow))
(assert (not current))
(check-sat)
(pop)

; The retry must cover the missing-row branch, not only an existing open row.
(echo "EXPECT-SAT: coverage retry creates missing row")
(push)
(assert PreInv)
(assert RetrySucceeded)
(assert (= row NoRow))
(assert (not current))
(check-sat)
(pop)

; Row creation can commit before finalization fails, so the retry must retain
; the newly acquired open-row handle for the next attempt.
(echo "EXPECT-SAT: coverage failed retry can retain newly-created row")
(push)
(assert PreInv)
(assert RetryFailedRecovered)
(assert (= row NoRow))
(assert (not current))
(assert (= row_next OpenRow))
(assert current_next)
(check-sat)
(pop)

; A preservation counterexample would make the final query satisfiable.
; check.sh appends (get-model) to this final context if that happens.
(echo "EXPECT-UNSAT: preservation")
(assert PreInv)
(assert Step)
(assert (not PostInv))
(check-sat)
