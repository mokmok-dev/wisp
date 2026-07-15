; Symbolically check the whole-app linearized transition relation.  Unlike the
; bounded TLC run, `segments` is an arbitrary non-negative integer here.
(set-logic ALL)

(declare-datatypes () ((View Library Live History)))
(declare-datatypes () ((Owner NoOwner LiveOwner HistoryOwner)))
(declare-datatypes () ((Ui Idle Starting Recording Stopping Failed)))
(declare-datatypes () ((Worker WorkerIdle WorkerStarting WorkerRunning WorkerStopping)))
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

(define-fun AppInv
  ((view View) (owner Owner) (ui Ui) (worker Worker) (row Row)
   (segments Int) (current Bool) (metadata Bool) (recovery Bool)
   (viewed Bool) (has_error Bool) (cancel_pending Bool)
   (quit QuitMode)) Bool
  (and
    (>= segments 0)
    (=> (Terminal ui) (and (= worker WorkerIdle) (not cancel_pending)))
    (=> (= ui Starting)
      (and (= worker WorkerStarting) (not cancel_pending)))
    (=> (= ui Recording)
      (and (= worker WorkerRunning) (not cancel_pending)))
    (=> (= ui Stopping)
      (or
        (and cancel_pending (= worker WorkerStarting))
        (and (not cancel_pending) (= worker WorkerStopping))))
    (=> current (= row OpenRow))
    (=> (= row OpenRow) current)
    (=> current metadata)
    (=> recovery metadata)
    (=> (= row EndedRow) (and (not current) (not metadata)))
    ; An ended row whose transcript remains in Live is the symbolic projection
    ; of `linked_session_id`: the open handle is gone but transcript identity
    ; remains stable until navigation replaces the owner.
    (=> (and (= view Live) (= row EndedRow))
      (and (= owner LiveOwner) (Terminal ui) (= worker WorkerIdle)
           (not current) (not metadata)))
    (=> (= ui Idle) (and (not current) (not metadata)))
    (=> has_error (= ui Failed))
    (=> (Active ui)
      (and (= view Live) (= owner LiveOwner) metadata))
    (=> metadata
      (and (= view Live) (= owner LiveOwner)
           (or (Active ui) (= ui Failed))))
    (=> (= view Library)
      (and (= owner NoOwner) (= segments 0) (not metadata)))
    (=> (= view Live) (= owner LiveOwner))
    (=> (= view History)
      (and (Terminal ui) (= worker WorkerIdle) (= owner HistoryOwner)
           viewed (not current) (not metadata)))
    (=> current
      (and
        (= view Live)
        (= owner LiveOwner)
        (or
          (and (= ui Recording) (= worker WorkerRunning))
          (and (= ui Stopping) (= worker WorkerStopping)
               (not cancel_pending))
          (and (= ui Failed) (= worker WorkerIdle) has_error))))
    (=> (= quit ExplicitQuit)
      (QuitSafe ui worker row metadata current recovery))))

(declare-const view View)
(declare-const owner Owner)
(declare-const ui Ui)
(declare-const worker Worker)
(declare-const row Row)
(declare-const segments Int)
(declare-const current Bool)
(declare-const metadata Bool)
(declare-const recovery Bool)
(declare-const viewed Bool)
(declare-const history_available Bool)
(declare-const has_error Bool)
(declare-const setup_ready Bool)
(declare-const cancel_pending Bool)
(declare-const quit QuitMode)

(declare-const view_next View)
(declare-const owner_next Owner)
(declare-const ui_next Ui)
(declare-const worker_next Worker)
(declare-const row_next Row)
(declare-const segments_next Int)
(declare-const current_next Bool)
(declare-const metadata_next Bool)
(declare-const recovery_next Bool)
(declare-const viewed_next Bool)
(declare-const history_available_next Bool)
(declare-const has_error_next Bool)
(declare-const setup_ready_next Bool)
(declare-const cancel_pending_next Bool)
(declare-const quit_next QuitMode)

(define-fun SameView () Bool (= view_next view))
(define-fun SameOwner () Bool (= owner_next owner))
(define-fun SameUi () Bool (= ui_next ui))
(define-fun SameWorker () Bool (= worker_next worker))
(define-fun SameRow () Bool (= row_next row))
(define-fun SameSegments () Bool (= segments_next segments))
(define-fun SameCurrent () Bool (= current_next current))
(define-fun SameMetadata () Bool (= metadata_next metadata))
(define-fun SameRecovery () Bool (= recovery_next recovery))
(define-fun SameViewed () Bool (= viewed_next viewed))
(define-fun SameHistory () Bool (= history_available_next history_available))
(define-fun SameError () Bool (= has_error_next has_error))
(define-fun SameSetup () Bool (= setup_ready_next setup_ready))
(define-fun SameCancel () Bool (= cancel_pending_next cancel_pending))
(define-fun SameQuit () Bool (= quit_next quit))
(define-fun OperationalQuit () Bool (and (= quit AppRunning) SameQuit))

(define-fun CompleteSetup () Bool
  (and
    OperationalQuit
    (not setup_ready)
    setup_ready_next
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata SameRecovery SameViewed SameHistory SameError SameCancel))

(define-fun EnterNewSession () Bool
  (and
    OperationalQuit
    (Terminal ui)
    (= worker WorkerIdle)
    (not current)
    (not (= row OpenRow))
    (not metadata)
    (= view_next Live)
    (= owner_next LiveOwner)
    (= ui_next Idle)
    (= worker_next WorkerIdle)
    (= row_next NoRow)
    (= segments_next 0)
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    (not viewed_next)
    SameHistory
    (not has_error_next)
    SameSetup
    (not cancel_pending_next)))

(define-fun RequestStart () Bool
  (and
    OperationalQuit
    (Terminal ui)
    (= worker WorkerIdle)
    (not current)
    (not (= row OpenRow))
    (not metadata)
    setup_ready
    (= view_next Live)
    (= owner_next LiveOwner)
    (= ui_next Starting)
    (= worker_next WorkerStarting)
    (= row_next NoRow)
    (= segments_next 0)
    (not current_next)
    metadata_next
    (not recovery_next)
    (not viewed_next)
    SameHistory
    (not has_error_next)
    SameSetup
    (not cancel_pending_next)))

(define-fun StartStored () Bool
  (and
    OperationalQuit
    (= ui Starting)
    (= worker WorkerStarting)
    (not cancel_pending)
    SameView SameOwner
    (= ui_next Recording)
    (= worker_next WorkerRunning)
    (= row_next OpenRow)
    SameSegments
    current_next
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel))

(define-fun StartUnstored () Bool
  (and
    OperationalQuit
    (= ui Starting)
    (= worker WorkerStarting)
    (not cancel_pending)
    SameView SameOwner
    (= ui_next Recording)
    (= worker_next WorkerRunning)
    (= row_next NoRow)
    SameSegments
    (not current_next)
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel))

(define-fun StartFailureReady () Bool
  (and
    (= worker WorkerStarting)
    (or
      (and (= ui Starting) (not cancel_pending))
      (and (= ui Stopping) cancel_pending))))

(define-fun StartFailedWithoutSegments () Bool
  (and
    OperationalQuit
    StartFailureReady
    (= segments 0)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    (= row_next NoRow)
    SameSegments
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    SameViewed SameHistory
    has_error_next
    SameSetup
    (not cancel_pending_next)))

(define-fun StartFailedWithSegmentsSucceeded () Bool
  (and
    OperationalQuit
    StartFailureReady
    (> segments 0)
    (Persistable metadata current row)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    (= row_next EndedRow)
    SameSegments
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    SameViewed
    history_available_next
    has_error_next
    SameSetup
    (not cancel_pending_next)))

(define-fun StartFailedWithSegmentsFailedRecovered () Bool
  (and
    OperationalQuit
    StartFailureReady
    (> segments 0)
    (Persistable metadata current row)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameViewed SameHistory
    has_error_next
    SameSetup
    (not cancel_pending_next)))

(define-fun StartFailedWithSegmentsFailedWithoutRecovery () Bool
  (and
    OperationalQuit
    StartFailureReady
    (> segments 0)
    (Persistable metadata current row)
    (not recovery)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    (not recovery_next)
    SameViewed SameHistory
    has_error_next
    SameSetup
    (not cancel_pending_next)))

(define-fun RequestStopWhileStarting () Bool
  (and
    OperationalQuit
    (= ui Starting)
    (= worker WorkerStarting)
    (not cancel_pending)
    SameView SameOwner
    (= ui_next Stopping)
    SameWorker SameRow SameSegments SameCurrent SameMetadata SameRecovery
    SameViewed SameHistory SameError SameSetup
    cancel_pending_next))

(define-fun CanceledStartStored () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerStarting)
    cancel_pending
    SameView SameOwner SameUi
    (= worker_next WorkerStopping)
    (= row_next OpenRow)
    SameSegments
    current_next
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    (not cancel_pending_next)))

(define-fun CanceledStartUnstored () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerStarting)
    cancel_pending
    SameView SameOwner SameUi
    (= worker_next WorkerStopping)
    (= row_next NoRow)
    SameSegments
    (not current_next)
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    (not cancel_pending_next)))

(define-fun ReceiveFailedStartSegment () Bool
  (and
    OperationalQuit
    (= worker WorkerStarting)
    (or (= ui Starting) (= ui Stopping))
    metadata
    SameView SameOwner SameUi SameWorker SameRow
    (= segments_next (+ segments 1))
    SameCurrent SameMetadata SameRecovery SameViewed SameHistory SameError
    SameSetup SameCancel))

(define-fun ReceiveSegment () Bool
  (and
    OperationalQuit
    (or (= ui Recording) (= ui Stopping))
    (or (= worker WorkerRunning) (= worker WorkerStopping))
    SameView SameOwner SameUi SameWorker SameRow
    (= segments_next (+ segments 1))
    SameCurrent SameMetadata SameRecovery SameViewed SameHistory SameError
    SameSetup SameCancel))

(define-fun RequestStop () Bool
  (and
    OperationalQuit
    (= ui Recording)
    (= worker WorkerRunning)
    SameView SameOwner
    (= ui_next Stopping)
    (= worker_next WorkerStopping)
    SameRow SameSegments SameCurrent SameMetadata SameRecovery SameViewed
    SameHistory SameError SameSetup
    (not cancel_pending_next)))

(define-fun FinishStop () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerStopping)
    (not cancel_pending)
    (Persistable metadata current row)
    SameView SameOwner
    (= ui_next Idle)
    (= worker_next WorkerIdle)
    (= row_next EndedRow)
    SameSegments
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    SameViewed
    history_available_next
    SameError SameSetup SameCancel))

(define-fun FinishStopErrorRecovered () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerStopping)
    (not cancel_pending)
    (Persistable metadata current row)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameViewed SameHistory
    has_error_next
    SameSetup SameCancel))

(define-fun FinishStopErrorWithoutRecovery () Bool
  (and
    OperationalQuit
    (= ui Stopping)
    (= worker WorkerStopping)
    (not cancel_pending)
    (Persistable metadata current row)
    (not recovery)
    SameView SameOwner
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    (not recovery_next)
    SameViewed SameHistory
    has_error_next
    SameSetup SameCancel))

(define-fun RetrySucceeded () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner
    (= ui_next Idle)
    SameWorker
    (= row_next EndedRow)
    SameSegments
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    SameViewed
    history_available_next
    (not has_error_next)
    SameSetup SameCancel))

(define-fun RetryFailedRecovered () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameViewed SameHistory SameError SameSetup SameCancel))

(define-fun RetryFailedRecoveryUnchanged () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel))

(define-fun NavigateLibrary () Bool
  (and
    OperationalQuit
    (Terminal ui)
    (= worker WorkerIdle)
    (not current)
    (not (= row OpenRow))
    (not metadata)
    (= view_next Library)
    (= owner_next NoOwner)
    SameUi SameWorker SameRow
    (= segments_next 0)
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    (not viewed_next)
    SameHistory
    (not has_error_next)
    SameSetup SameCancel))

(define-fun OpenHistory () Bool
  (and
    OperationalQuit
    (= view Library)
    (Terminal ui)
    (= worker WorkerIdle)
    (not current)
    (not (= row OpenRow))
    (not metadata)
    history_available
    (= view_next History)
    (= owner_next HistoryOwner)
    SameUi SameWorker SameRow
    (= segments_next 1)
    (not current_next)
    SameMetadata SameRecovery
    viewed_next
    SameHistory SameError SameSetup SameCancel))

(define-fun ExplicitQuitSettled () Bool
  (and
    (= quit AppRunning)
    (SettledSafe ui worker row metadata current)
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel
    (= quit_next ExplicitQuit)))

(define-fun ExplicitQuitWithRecovery () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameViewed SameHistory SameError SameSetup SameCancel
    (= quit_next ExplicitQuit)))

(define-fun ExplicitQuitRecoveryFailed () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    (not recovery)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel))

(define-fun OsQuitSettled () Bool
  (and
    (= quit AppRunning)
    (SettledSafe ui worker row metadata current)
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel
    (= quit_next OsQuit)))

(define-fun OsQuitWithRecovery () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata
    recovery_next
    SameViewed SameHistory SameError SameSetup SameCancel
    (= quit_next OsQuit)))

(define-fun OsQuitRecoveryFailed () Bool
  (and
    (= quit AppRunning)
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    (not recovery)
    has_error
    SameView SameOwner SameUi SameWorker SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel
    (= quit_next OsQuit)))

(define-fun OsQuitActiveTimeoutRecovered () Bool
  (and
    (= quit AppRunning)
    (or (= ui Starting) (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata
    recovery_next
    SameViewed SameHistory SameError SameSetup
    SameCancel
    (= quit_next OsQuit)))

(define-fun OsQuitActiveTimeoutWithoutRecovery () Bool
  (and
    (= quit AppRunning)
    (or (= ui Starting) (= ui Recording) (= ui Stopping))
    (Persistable metadata current row)
    (not recovery)
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata
    (not recovery_next)
    SameViewed SameHistory SameError SameSetup SameCancel
    (= quit_next OsQuit)))

(define-fun RelaunchRecoverySucceeded () Bool
  (and
    (RelaunchCandidate quit ui worker recovery metadata current row)
    (= view_next Library)
    (= owner_next NoOwner)
    (= ui_next Idle)
    (= worker_next WorkerIdle)
    (= row_next EndedRow)
    (= segments_next 0)
    (not current_next)
    (not metadata_next)
    (not recovery_next)
    (not viewed_next)
    history_available_next
    (not has_error_next)
    SameSetup
    (not cancel_pending_next)
    (= quit_next AppRunning)))

(define-fun RelaunchRecoveryFailed () Bool
  (and
    (RelaunchCandidate quit ui worker recovery metadata current row)
    (= view_next Live)
    (= owner_next LiveOwner)
    (= ui_next Failed)
    (= worker_next WorkerIdle)
    SameSegments
    (FailureOwnership current row current_next row_next)
    SameMetadata SameRecovery
    (not viewed_next)
    SameHistory
    has_error_next
    SameSetup
    (not cancel_pending_next)
    (= quit_next AppRunning)))

(define-fun AwaitUserRetry () Bool
  (and
    OperationalQuit
    (= ui Failed)
    (= worker WorkerIdle)
    (Persistable metadata current row)
    has_error
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel))

(define-fun QuitComplete () Bool
  (and
    (or (= quit ExplicitQuit) (= quit OsQuit))
    SameView SameOwner SameUi SameWorker SameRow SameSegments SameCurrent
    SameMetadata SameRecovery SameViewed SameHistory SameError SameSetup
    SameCancel SameQuit))

(define-fun Step () Bool
  (or CompleteSetup EnterNewSession RequestStart StartStored StartUnstored
      StartFailedWithoutSegments StartFailedWithSegmentsSucceeded
      StartFailedWithSegmentsFailedRecovered
      StartFailedWithSegmentsFailedWithoutRecovery RequestStopWhileStarting
      CanceledStartStored CanceledStartUnstored ReceiveFailedStartSegment
      ReceiveSegment RequestStop FinishStop FinishStopErrorRecovered
      FinishStopErrorWithoutRecovery
      RetrySucceeded RetryFailedRecovered RetryFailedRecoveryUnchanged
      NavigateLibrary OpenHistory ExplicitQuitSettled ExplicitQuitWithRecovery
      ExplicitQuitRecoveryFailed OsQuitSettled OsQuitWithRecovery
      OsQuitRecoveryFailed OsQuitActiveTimeoutRecovered
      OsQuitActiveTimeoutWithoutRecovery RelaunchRecoverySucceeded
      RelaunchRecoveryFailed AwaitUserRetry QuitComplete))

(define-fun PreInv () Bool
  (AppInv view owner ui worker row segments current metadata recovery viewed
          has_error cancel_pending quit))

(define-fun PostInv () Bool
  (AppInv view_next owner_next ui_next worker_next row_next segments_next
          current_next metadata_next recovery_next viewed_next has_error_next
          cancel_pending_next quit_next))

(echo "EXPECT-ACTION-COUNT: 35")
(echo "EXPECT-ACTIONS: CompleteSetup,EnterNewSession,RequestStart,StartStored,StartUnstored,StartFailedWithoutSegments,StartFailedWithSegmentsSucceeded,StartFailedWithSegmentsFailedRecovered,StartFailedWithSegmentsFailedWithoutRecovery,RequestStopWhileStarting,CanceledStartStored,CanceledStartUnstored,ReceiveFailedStartSegment,ReceiveSegment,RequestStop,FinishStop,FinishStopErrorRecovered,FinishStopErrorWithoutRecovery,RetrySucceeded,RetryFailedRecovered,RetryFailedRecoveryUnchanged,NavigateLibrary,OpenHistory,ExplicitQuitSettled,ExplicitQuitWithRecovery,ExplicitQuitRecoveryFailed,OsQuitSettled,OsQuitWithRecovery,OsQuitRecoveryFailed,OsQuitActiveTimeoutRecovered,OsQuitActiveTimeoutWithoutRecovery,RelaunchRecoverySucceeded,RelaunchRecoveryFailed,AwaitUserRetry,QuitComplete")

; Base case: the concrete initial state must satisfy the invariant.
(echo "EXPECT-UNSAT: base-case")
(push)
(assert
  (not (AppInv Library NoOwner Idle WorkerIdle NoRow 0 false false false
               false false false AppRunning)))
(check-sat)
(pop)

; Anti-vacuity: every Step action has exactly one declared witness.
(echo "EXPECT-SAT: action CompleteSetup")
(push) (assert PreInv) (assert CompleteSetup) (check-sat) (pop)
(echo "EXPECT-SAT: action EnterNewSession")
(push) (assert PreInv) (assert EnterNewSession) (check-sat) (pop)
(echo "EXPECT-SAT: action RequestStart")
(push) (assert PreInv) (assert RequestStart) (check-sat) (pop)
(echo "EXPECT-SAT: action StartStored")
(push) (assert PreInv) (assert StartStored) (check-sat) (pop)
(echo "EXPECT-SAT: action StartUnstored")
(push) (assert PreInv) (assert StartUnstored) (check-sat) (pop)
(echo "EXPECT-SAT: action StartFailedWithoutSegments")
(push) (assert PreInv) (assert StartFailedWithoutSegments) (check-sat) (pop)
(echo "EXPECT-SAT: action StartFailedWithSegmentsSucceeded")
(push) (assert PreInv) (assert StartFailedWithSegmentsSucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action StartFailedWithSegmentsFailedRecovered")
(push) (assert PreInv) (assert StartFailedWithSegmentsFailedRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action StartFailedWithSegmentsFailedWithoutRecovery")
(push) (assert PreInv) (assert StartFailedWithSegmentsFailedWithoutRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action RequestStopWhileStarting")
(push) (assert PreInv) (assert RequestStopWhileStarting) (check-sat) (pop)
(echo "EXPECT-SAT: action CanceledStartStored")
(push) (assert PreInv) (assert CanceledStartStored) (check-sat) (pop)
(echo "EXPECT-SAT: action CanceledStartUnstored")
(push) (assert PreInv) (assert CanceledStartUnstored) (check-sat) (pop)
(echo "EXPECT-SAT: action ReceiveFailedStartSegment")
(push) (assert PreInv) (assert ReceiveFailedStartSegment) (check-sat) (pop)
(echo "EXPECT-SAT: action ReceiveSegment")
(push) (assert PreInv) (assert ReceiveSegment) (check-sat) (pop)
(echo "EXPECT-SAT: action RequestStop")
(push) (assert PreInv) (assert RequestStop) (check-sat) (pop)
(echo "EXPECT-SAT: action FinishStop")
(push) (assert PreInv) (assert FinishStop) (check-sat) (pop)
(echo "EXPECT-SAT: action FinishStopErrorRecovered")
(push) (assert PreInv) (assert FinishStopErrorRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action FinishStopErrorWithoutRecovery")
(push) (assert PreInv) (assert FinishStopErrorWithoutRecovery) (check-sat) (pop)
(echo "EXPECT-SAT: action RetrySucceeded")
(push) (assert PreInv) (assert RetrySucceeded) (check-sat) (pop)
(echo "EXPECT-SAT: action RetryFailedRecovered")
(push) (assert PreInv) (assert RetryFailedRecovered) (check-sat) (pop)
(echo "EXPECT-SAT: action RetryFailedRecoveryUnchanged")
(push) (assert PreInv) (assert RetryFailedRecoveryUnchanged) (check-sat) (pop)
(echo "EXPECT-SAT: action NavigateLibrary")
(push) (assert PreInv) (assert NavigateLibrary) (check-sat) (pop)
(echo "EXPECT-SAT: action OpenHistory")
(push) (assert PreInv) (assert OpenHistory) (check-sat) (pop)
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

; A failed native start can preserve a flushed microphone result.
(echo "EXPECT-SAT: coverage failed start preserves flushed event")
(push)
(assert PreInv)
(assert StartFailedWithSegmentsSucceeded)
(assert (= segments 1))
(assert (= row NoRow))
(assert (not current))
(check-sat)
(pop)

; The retry must cover Started's missing-row failure, not only an open row.
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
