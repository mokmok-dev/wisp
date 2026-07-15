# Formal verification

Wisp uses two complementary formal-method tools:

- **TLA+ / TLC** exhaustively explores bounded interleavings of the UI,
  recording worker, update pump, navigation, and session persistence.
- **Z3** checks inductive safety for two symbolic transition abstractions. The
  whole-app proof leaves transcript length unbounded; the session proof also
  keeps unbounded emitted/applied/pending event counters and pending Stop
  commands.

Run the complete suite from the repository root:

```bash
nix develop .#formal --command bash formal/check.sh
```

`nix flake check --print-build-logs` runs the same suite through the
`checks.formal` derivation. `TLC_WORKERS` defaults to `2` and can be overridden
for a larger local run.

## Models

### `SessionLifecycle`

[`tla/SessionLifecycle.tla`](tla/SessionLifecycle.tla) models the asynchronous
session protocol in detail:

- UI phases: `Idle`, `Starting`, `Recording`, `Stopping`, `Failed`
- worker phases: `Idle`, `Starting`, `Failing`, `Running`, `Stopping`
- FIFO `Start` / `Stop` commands
- FIFO `Started` / `Event` / `Stopped` / `Error` updates
- projected `Event` steps create new persistable segments; `Log` updates and
  same-segment partial revisions are omitted as stuttering steps
- retained launch/output metadata when a pre-allocated row later disappears
- bounded failed-start microphone flushes delivered as FIFO `Event*`, `Error`
- missing-row creation during stop/retry and idempotent transactional release
- atomic recovery-sidecar success/failure, retry, and relaunch reconciliation
- guarded explicit quit, best-effort OS quit, and active-state OS timeout
- transcript-event delivery and terminal finalization

Its liveness properties use weak fairness only for the in-process worker and
UI update pump. They do **not** assume that a user eventually presses Stop.
While the app is running, `Starting` and `Stopping` must either resolve or be
cut short by a non-vetoable OS quit.

### `ApplicationLifecycle`

[`tla/ApplicationLifecycle.tla`](tla/ApplicationLifecycle.tla) linearizes the
queue protocol and composes it with the rest of the central `AppModel`
workflow:

- setup gating
- `Library`, `Live`, and `History` navigation
- ownership of the visible transcript
- an open persistence handle, a separately retained live-transcript link, and
  the viewed history handle
- successful, failed, and initially unavailable storage
- graceful-quit cancellation while native start is in flight
- partial-transcript persistence when native start fails after capturing audio
- transactional finalization, retry success/failure, and durable recovery state
- automatic relaunch reconciliation of a validated recovery snapshot
- explicit-quit durability guard and non-vetoable best-effort OS quit,
  including the five-second active-stop timeout

The key whole-app invariant is:

```text
active session => Live view && live transcript owner
```

This prevents live events from being displayed or persisted as a historical
session. Navigation and restart actions are enabled only while both UI and
worker are settled and neither a failed-finalization handle nor retained
launch metadata remains. Production allocates the database row and stable
`SessionId` before sending Start to the worker. Every worker update carries
that ID, so delayed events cannot mutate a newer transcript. Finalization
releases the open persistence handle but retains a separate
`linked_session_id` for IPC/export until navigation replaces the Live
transcript. Stop or recovery can still recreate a row removed after
pre-allocation.

### Z3 transition proofs

The two files under [`z3/`](z3/) ask Z3 for a counterexample in which a valid
state takes one valid step into an invalid state. The application proof
symbolically encodes the linearized actions while leaving segment count
unbounded. The session proof is deliberately different from the exact-FIFO
TLA+ model: it tracks a bounded Start count, bounded lifecycle-update counts,
an unbounded pending Stop count, and the unbounded causal identity

```text
emitted events = applied segments + pending event updates
```

This identity ranges only over the projected events that create a new
persistable segment. Real `Update::Event` values that are logs or revisions of
an existing segment do not increase the segment vector and are stuttering
steps in both formal projections. The identity therefore proves causality for
new-segment events, not that every implementation event grows the vector. It
does not prove FIFO ordering; TLC checks both that ordering and the bounded
form of the same event-debt identity in `SessionLifecycle`.

Each Z3 file contains three kinds of core query. The concrete initial state must
satisfy the invariant (`unsat` for its negation), every declared action must
have an invariant-satisfying witness (`sat` anti-vacuity checks), and the
one-step counterexample query must be `unsat`. Each proof declares an exact
ordered action set and count; the runner cross-checks that declaration against
the `Step` definition and rejects missing, extra, or duplicate witnesses. It
also validates targeted coverage such as accumulation of
multiple pending Stop commands, retry from a missing-row state, and persistence
of an event flushed during failed-start cleanup. `unknown` is a failure, and a
model is printed when final preservation unexpectedly returns `sat`.

## Implementation map

| Formal action / state | Implementation |
| --- | --- |
| UI phase and top-level view | `apps/wisp-desktop/src/app.rs`: `SessionState`, `View`, `AppModel` |
| `RequestStart`, `RequestStop` | `apps/wisp-desktop/src/main.rs`: `toggle_recording` |
| command and update FIFO, stable update identity | `apps/wisp-desktop/src/session_runner.rs`: `SessionStart`, `Command`, `Update`, `worker_loop` |
| worker start, event pump, flush, stop | `apps/wisp-desktop/src/session_runner.rs`: `run_session` |
| `ApplyStarted/Event/Stopped/Error` | `apps/wisp-desktop/src/session_updates.rs`: `apply_update` |
| open row, retained transcript link, end/rollback/retry | `apps/wisp-desktop/src/app.rs`, `apps/wisp-desktop/src/session_updates.rs`, `apps/wisp-desktop/src/library.rs`, `crates/wisp-storage/src/lib.rs` |
| recovery sidecar and relaunch scan | `apps/wisp-desktop/src/session_updates.rs`: recovery snapshot/reconciliation helpers |
| explicit/OS quit policy | `apps/wisp-desktop/src/app_menu.rs`: graceful stop and quit callbacks |
| navigation and transcript ownership | `AppModel::show_library`, `show_new_session`, `show_history` |
| native lifecycle | `native/WispAudioKit/Sources/WispAudioKit/WispSession.swift` |

The mapping is reviewable rather than generated. Changes to any mapped state,
event, queue, or guard must update the corresponding TLA+ action and Z3 step in
the same pull request. The dedicated workflow runs on every pull request, even
when only implementation files changed.

## What the first model found

Before the navigation guards were added, TLC's whole-app invariant had this
short counterexample:

```text
Live/Recording -> Back -> Library/Recording
  -> New Session/Idle while worker remains Running
  -> Start is ignored by the already-running worker
```

A related path started a new recording from `History(old)` through the global
record shortcut, producing live segments while the IPC snapshot still named
the old session. `AppModel` now rejects navigation during active phases, the
live Back control is hidden while active, and global start first normalizes the
view to `Live`.

## Bounds, assumptions, and deliberate omissions

- CI uses `MaxEvents = 2` and `MaxSegments = 2`. These are data bounds, not
  path-depth bounds; TLC still explores the complete finite state graph.
- Z3 proves one-step inductive preservation, not temporal liveness. Its
  session command/update counters abstract queue contents. Pending Stop and
  event-debt counters are unbounded, while exact FIFO order is covered only by
  the bounded TLA+ exploration.
- Native `start()` and `stop()` are assumed to return eventually on the normal
  in-process branch. `OsQuitActiveTimeoutRecovered` and
  `OsQuitActiveTimeoutWithoutRecovery` represent the actual app-quit callback
  reaching its five-second stop timeout. Because that OS quit cannot be
  vetoed, it preserves `Starting`, `Recording`, or `Stopping` while entering
  the OS terminal state. The recovered branch writes a durable sidecar that a
  later launch can reconcile; the other branch permits transcript loss. No
  fairness assumption forces the environment to request that quit.
- Permission and recognizer detail is collapsed into `setupReady`.
  Local-model download and local MCP bridge lifecycles are independent
  extension candidates.
- Production fails closed if the initial row cannot be created, before native
  audio starts. The model's broader `*WithoutStorage` branches conservatively
  cover a row disappearing after pre-allocation and recovery snapshots whose
  row is already missing. Transactional finalization failure is represented.
  A finalization failure rolls back atomically, enters `Failed`, and retains
  launch metadata plus either an open-row handle or a missing-row state.
  When missing-row creation succeeds before finalization fails, the newly
  acquired open-row handle is retained for the next idempotent attempt.
  Retried reconciliation is idempotent. Recovery JSON is atomically replaced;
  explicit quit commits only after persistence settles or that snapshot is
  durable. The OS callback can only make a best-effort write, and its active
  timeout-without-recovery branch may terminate before either persistence or
  recovery succeeds.
- On a later launch, the implementation scans and validates recovery sidecars,
  reconciles them until the first database failure, and restores that first
  transcript into guarded `Live/Failed` for a later retry. If the matching row
  is already ended, the database transaction had committed and only stale
  sidecar cleanup is retried; that case never becomes a live handle. The model
  represents one selected valid pending snapshot only; ordering across
  multiple sidecars and continuation of the directory-wide scan are outside
  the model, as are invalid/untrusted sidecars and SQLite/WAL crash durability
  below the transaction boundary.
- Rendering, transcript text, timestamps, and audio buffers are data-plane
  concerns and are not represented.

## Extending the model

When adding an app transition:

1. Add its state variables and action to the relevant `.tla` module.
2. State the intended invariant or temporal property explicitly in its `.cfg`.
3. Add the corresponding symbolic step to the Z3 proof when it affects a
   safety invariant.
4. Update the implementation map and add a concrete Rust/Swift regression
   test for the same boundary.
5. Run `formal/check.sh` and inspect any TLC trace before changing the
   invariant. A counterexample is often an implementation design bug, not a
   reason to weaken the property.
