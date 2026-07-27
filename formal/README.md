# Implementation verification

Wisp verifies the Rust lifecycle implementation directly. The old TLA+/TLC
and Z3 transition models were removed because they duplicated production
behavior and could drift from it.

The code under verification lives in
[`crates/wisp-lifecycle`](../crates/wisp-lifecycle). The desktop application
calls that crate for:

- active-session and navigation ownership guards;
- stable session-ID filtering for asynchronous updates;
- `Started` and Stop phase transitions;
- the worker-update phase reducer.

## Run locally

Install Kani once:

```bash
cargo install --locked kani-verifier
cargo kani setup
```

Then run both verification layers:

```bash
bash formal/check.sh
```

The individual commands are:

```bash
cargo kani -p wisp-lifecycle
cargo test -p wisp-lifecycle --test shuttle_lifecycle
```

## Kani proofs

The `#[cfg(kani)]` proof module in
[`src/lib.rs`](../crates/wisp-lifecycle/src/lib.rs) invokes the same public
functions used by `wisp-desktop`. Kani checks all bit-level input values for:

- an accepted update always owns the active Live transcript and matching ID;
- every reducer step preserves the ownership invariant;
- a delayed update from another session cannot mutate the reducer;
- Stop is monotonic and idempotent;
- navigation is enabled only after worker and persistence ownership settle.

Kani also checks reachable panics, arithmetic overflow, and memory safety in
those paths.

## Shuttle tests

[`shuttle_lifecycle.rs`](../crates/wisp-lifecycle/tests/shuttle_lifecycle.rs)
runs a stateful driver around the production `UpdateContext` reducer with
Shuttle's deterministic scheduler and instrumented threads, mutexes, and FIFO
channels. It explores:

- Stop racing with `Started`, `Event`, and `Stopped`;
- stale updates from another session racing with one another.

Each test explores 1,000 schedules. Shuttle is randomized rather than a proof;
failures print a reproducible schedule. Kani supplies exhaustive input
reasoning for the pure transition code, while Shuttle supplies scheduled
concurrency coverage around it.

## Extending lifecycle behavior

Put lifecycle decisions in `wisp-lifecycle` and call them from production.
Add or strengthen a Kani harness for state/input properties and add a Shuttle
scenario when ordering or shared-state interleavings matter. Avoid recreating
the production transition as a separate specification model.
