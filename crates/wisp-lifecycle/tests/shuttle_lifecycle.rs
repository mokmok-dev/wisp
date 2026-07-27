use shuttle::sync::{Arc, Mutex, mpsc};
use shuttle::thread;
use wisp_lifecycle::{Phase, Reducer, WorkerUpdate};

#[test]
fn stop_and_fifo_worker_updates_preserve_the_live_owner() {
    shuttle::check_random(
        || {
            let session_id = 41;
            let reducer = Arc::new(Mutex::new(Reducer::new(session_id)));
            let (update_tx, update_rx) = mpsc::channel();

            let worker = thread::spawn(move || {
                update_tx
                    .send((session_id, WorkerUpdate::Started))
                    .expect("update pump remains connected");
                update_tx
                    .send((session_id, WorkerUpdate::Event))
                    .expect("update pump remains connected");
                update_tx
                    .send((session_id, WorkerUpdate::Stopped))
                    .expect("update pump remains connected");
            });

            let stop_reducer = Arc::clone(&reducer);
            let stop = thread::spawn(move || {
                stop_reducer
                    .lock()
                    .expect("reducer lock is available")
                    .request_stop();
            });

            for _ in 0..3 {
                let (id, update) = update_rx.recv().expect("worker sends three updates");
                let mut state = reducer.lock().expect("reducer lock is available");
                let _ = state.apply(id, update);
                assert!(state.context().invariant_holds());
                drop(state);
            }
            worker.join().expect("worker completes");
            stop.join().expect("stop request completes");

            let state = *reducer.lock().expect("reducer lock is available");
            assert!(matches!(
                state.context().phase,
                Phase::Idle | Phase::Stopping
            ));
            assert!(state.applied_events() <= 1);
            assert!(state.terminal_updates() <= 1);
            assert!(state.context().invariant_holds());
        },
        1_000,
    );
}

#[test]
fn stale_session_updates_are_ignored_under_concurrency() {
    shuttle::check_random(
        || {
            let current_id = 7;
            let reducer = Arc::new(Mutex::new(Reducer::new(current_id)));
            let mut joins = Vec::new();
            for update in [
                WorkerUpdate::Started,
                WorkerUpdate::Event,
                WorkerUpdate::Stopped,
            ] {
                let reducer = Arc::clone(&reducer);
                joins.push(thread::spawn(move || {
                    let accepted = reducer
                        .lock()
                        .expect("reducer lock is available")
                        .apply(current_id + 1, update);
                    assert!(!accepted);
                }));
            }
            for join in joins {
                join.join().expect("stale update task completes");
            }

            let state = *reducer.lock().expect("reducer lock is available");
            assert_eq!(state, Reducer::new(current_id));
        },
        1_000,
    );
}
