# Verification

A listed check is a locator. It becomes evidence only when an executed result
is recorded beside it, with the host and the date. A test that exists proves
nothing about the device it never ran on.

## What each gate proves

Every check is a `nix flake check` attribute. That command is the single entry
point; there is no wrapper script.

| `nix flake check` attribute | Proves | Host |
| --- | --- | --- |
| `checks.aarch64-darwin.treefmt` | Every Nix file matches `nixfmt` and every `Sources/`, `Tests/`, and `App/` Swift file matches stock `swift-format` | Any with Nix |
| `checks.aarch64-darwin.build` | The whole package type-checks, including the SwiftUI app and the audio targets | macOS 26 with the SDK |
| `checks.aarch64-darwin.tests` | Core types, settings, the meeting catalog and detection state machine, the lifecycle reducer, and storage behaviour | macOS 26 with Xcode (XCTest) |
| `checks.aarch64-darwin.pre-commit` | The configured pre-commit hooks evaluate and run | Any with Nix |
| Desktop rendering | Nothing. The tests cover model and state logic, not pixels | n/a |
| Recording/transcription | Nothing below the `WispSession` seam; capture and the speech engine need real hardware | n/a |

`.github/workflows/ci.yaml` runs `nix flake check` on `macos-26` with Xcode
installed, so the XCTest suites execute there. That is the only workflow that
runs these checks.

`swift test` needs XCTest, which Apple ships with Xcode and not with the Command
Line Tools. `checks.aarch64-darwin.tests` resolves the macOS SDK platform with
`xcrun --sdk macosx --show-sdk-platform-path`, then type-checks a one-line
`import XCTest` against it. A bare probe would report a false negative on a host
that has Xcode, because Apple puts the module under the platform rather than the
toolchain.

When the probe fails with "no such module 'XCTest'", the check **fails**. It
records `skipped: XCTest is not available to the Swift compiler` in
`$out/status` and exits 1 unless the caller opts in:

```bash
WISP_ALLOW_SKIPPED_TESTS=1 nix build --impure .#checks.aarch64-darwin.tests
```

The opt-out is evaluated by Nix and handed to the derivation through its `env`,
because a builder does not inherit the invoking shell's environment. That is why
`--impure` is part of the command: a pure `nix flake check` reads the variable as
empty and keeps the fail-closed default. A skip is not evidence that the suites
pass, and `nix flake check` is the only entry point, so the default has to be a
failure. Every other failure — a compile error, a failing test, a crash, or a run
that executes no suite — fails the check regardless of the opt-out.

## Current record

2026-11-16, macOS 26 (Apple Silicon), Swift 6.4.2 / SDK 27.2, Xcode 27.2 at
`/Applications/Xcode.app`. `xcode-select -p` reports
`/Library/Developer/CommandLineTools`, but inside the Nix build sandbox no
`DEVELOPER_DIR` is set, so `xcrun` resolves the Xcode developer directory and
XCTest is reachable.

- `nix flake check` **passed** on this host, all four gates: `treefmt`, `build`,
  `tests`, and `pre-commit`.
- `checks.aarch64-darwin.build` really compiled: its log ends in
  `Build complete! (17.26 sec)` covering `WispCore`, `WispLifecycle`,
  `WispStorage`, `WispAudioKit`, `WispDesktop`, and `wispctl`.
- `checks.aarch64-darwin.tests` really executed every XCTest bundle:
  `WispStorageTests` 44, `WispLifecycleTests` 11, `WispDesktopTests` 88,
  `WispCoreTests` 50, and `WispAudioKitTests` 29 — 222 tests, 0 failures. Its
  output records `passed: XCTest suites executed`.
- The skip branch was exercised deliberately, in a scratch copy of the flake
  with `DEVELOPER_DIR=/Library/Developer/CommandLineTools` pinned so the XCTest
  probe fails. Without the opt-out it failed the check; with
  `WISP_ALLOW_SKIPPED_TESTS=1 nix build --impure …` it wrote
  `skipped: XCTest is not available to the Swift compiler` and exited 0.
- The gate's failure paths were also exercised in scratch copies. A type error in
  a test file failed the check, and so did a runtime assertion failure
  (`XCTAssertEqual failed`, `Some test targets reported failures`,
  `nix build` exit 1). Neither was converted into a skip.
- `swift-format lint --recursive Sources Tests App` reported no
  findings of any severity, using the 6.4.0 the `treefmt` check runs. The
  repository commits no `.swift-format`, so the tool's stock style — 2-space
  indentation, 100 columns — is what the tree matches.
- On 2026-11-17, macOS 27.2 (Apple Silicon), Swift 6.4.2 / SDK 27.2,
  `nix flake check` **passed** on all four gates. The `tests` gate executed 268
  XCTest cases with 0 failures: `WispStorageTests` 44, `WispLifecycleTests` 11,
  `WispDesktopTests` 120, `WispCoreTests` 64, `WispAudioKitTests` 29, and it
  recorded `passed: XCTest suites executed`. `swift-format lint --recursive
  Sources Tests App` reported nothing.
- Dictation was exercised against the signed `nix build .#Wisp-debug` bundle on
  the same host. With Google Chrome frontmost, the chord opened the HUD over
  Chrome without changing the frontmost app across a sampled run, live partial
  text appeared with the caret, and finishing wrote the transcript to the
  clipboard with no newline inserted mid-sentence. Starting a recording and then
  pressing the chord produced the in-progress notice. The panel measured 520
  points wide and grew upward as the text wrapped.
- Still unverified by automation: the chord failing to register because another
  app owns it, the recorder field accepting a new chord, and the HUD on a second
  display. Each needs a person, and the checklist above names them.
- Recording, transcription, meeting detection against real apps, and every
  other rendering path remain unverified. The tests cover model and state logic
  only.

## Needs a person on macOS 26

Run these before the next release and record the result and date here.

- Launch the app built by `nix build` (`open result/Wisp.app`) on a host with
  full Xcode and confirm the window opens.
- Open Settings with <kbd>⌘,</kbd> while a recording is running. Confirm the
  live transcript keeps updating and Back returns to the recording screen.
- Choose English (US), record a session, and confirm the transcript is English.
  Choose a language the Speech framework rejects and confirm the failure lands
  in the status strip instead of crashing.
- Enable meeting detection, join a Zoom or Teams call, and confirm one prompt
  appears after about two seconds. Dismiss it and confirm the same call is not
  offered again until the app has been off the microphone for a minute.
- Set Zoom to Auto-record and join a call. Confirm exactly one session starts,
  and that a call detected while a session is already recording is ignored.
- Join Google Meet in Safari or Chrome. Confirm the browser prompt waits about
  twenty seconds, and a short voice note in the same browser does not prompt.
- Press the dictation chord while another app is focused. Confirm the panel
  appears, the other app keeps its insertion point, and recognized text lands
  in the panel as you speak.
- Press the chord again. Confirm the panel shows that the transcript was
  copied, closes within about two seconds, and `⌘V` pastes the text with no
  sentence broken across a line.
- Start a recording, then press the dictation chord. Confirm the panel reports
  that a recording is in progress and that the recording keeps running.
- Dictate while a recording is not running, then press <kbd>⌘R</kbd>. Confirm
  no recording starts and the panel reports the conflict.
- Clear the chord in Settings, and confirm the chord no longer opens the panel.
  Set a chord another app already owns and confirm Settings names the conflict
  and keeps the previous chord working.
- Quit and relaunch, then confirm the chosen language and every tool policy
  survive in `settings.json`.
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
- Open a database written by the previous release. Confirm the library, the
  transcript, and a two-character Japanese search all still work, and that
  `PRAGMA user_version` stays at 2.
