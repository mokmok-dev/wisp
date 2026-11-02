# Verification

A listed check is a locator. It becomes evidence only when an executed result
is recorded beside it, with the host and the date. A test that exists proves
nothing about the device it never ran on.

## What each gate proves

| Gate | Command | Proves | Host |
| --- | --- | --- | --- |
| Formatting | `./scripts/check` (stage 1) | The tree parses and matches `rustfmt` | Any |
| Lints | `./scripts/check` (stage 2) | Clippy's deny set holds for the whole workspace | macOS with Xcode |
| Rust tests | `./scripts/check` (stage 3) | Model, storage, lifecycle, and pure audio logic | macOS with Xcode |
| Pure subset | `./scripts/check --pure` | The same for `wisp-core`, `wisp-lifecycle`, and `wisp-storage` | Any |
| Desktop rendering | none | Nothing. The desktop tests cover model and state logic, not pixels | n/a |
| Swift tests | `swift test --package-path native/WispAudioKit` | Capture, recording, and transcription adapters | macOS 26 |
| Formal | `bash formal/check.sh` | Kani proofs and Shuttle schedules for the lifecycle protocol | macOS 26 |
| Mutation | `./scripts/check --full` | Every mutant in the pure-logic crates is killed | macOS with Xcode |

CI runs the macOS jobs in `.github/workflows/ci.yaml` and `formal.yaml`. The
`swift.yaml` job runs `swiftformat --lint` only, so Swift tests stay a local
step.

## Current record

2026-11-02, Linux x86_64 (NixOS), Rust 1.99.1:

- `./scripts/check --pure` passed. That covers formatting, clippy, and 70
  tests across `wisp-core`, `wisp-lifecycle`, and `wisp-storage`, including the
  trigram search and the v1 to v2 migration.
- The desktop crate was type-checked and its 58 unit tests passed on the same
  Linux host against a compile-only stub of the macOS backend. That stub lives
  outside the repository. The real macOS backend and every rendering path
  remain unverified.
- `swift test`, `formal/check.sh`, and `cargo mutants` were not run.

## Needs a person on macOS 26

Run these before the next release and record the result and date here.

- First launch with both permissions undetermined, then grant them. Confirm
  the onboarding rows update without a restart.
- Deny Speech Recognition and start a recording. Confirm the library screen
  shows "Recording without transcription" and the audio still saves.
- Force a failed start (for example, revoke the microphone while the live view
  is idle), then confirm the failure banner names the cause and Retry starts a
  new session.
- Search a Japanese transcript for a two-character word and a three-character
  word. Confirm both return the expected session.
- Delete a session with the row button. Confirm the second click removes the
  library row and its `recordings/` directory, and Cancel leaves both alone.
- Copy and export with an empty transcript. Confirm the error notification, and
  confirm a real export writes the file.
