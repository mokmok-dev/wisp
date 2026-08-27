# Wisp

**A privacy-first recording & transcription desktop app for macOS.**

Wisp captures your microphone and system audio (the other side of a call) at
the same time and transcribes both on-device with Apple's `SpeechAnalyzer`.

---

## Features

- **Offline-first** — recordings and transcripts stay on your device. Nothing
  is uploaded; there is no online transcription service involved.
- **On-device transcription** — Uses [`SpeechAnalyzer`](https://developer.apple.com/documentation/speech), the new API in Apple's Speech framework. Both the microphone and the far side of the call are transcribed locally.
- **System audio + microphone capture** — Uses macOS 14.2+ [Core Audio Process Taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps). Each source is captured as a separate track.
- **Built in Rust with a GPU-rendered UI** — The UI is built on [GPUI](https://www.gpui.rs/), the framework that powers the [Zed](https://zed.dev/) editor. Native-feeling responsiveness and smooth scrolling.
- **Simple local storage** — Recordings are stored as Ogg/Opus and metadata as SQLite under `$WISP_DATA_DIR`, or `$HOME/Library/Application Support/dev.mokmok.wisp/` when the override is unset. Completed transcripts can be copied as plain text or exported as Markdown.

## Screenshots

![Wisp session library — past sessions and a New Session entry point](docs/screenshot.png)

## Architecture

Wisp is a small Cargo workspace with cleanly separated concerns:

| Crate / target | Responsibility |
| --- | --- |
| `apps/wisp-desktop` | GPUI desktop shell. Handles setup, recording controls, session history, and transcript export. |
| `crates/wisp-core` | Shared, platform-agnostic types (`Session`, `Segment`, IDs, `SourceLabel`). |
| `crates/wisp-audiokit` | macOS audio/transcription backends and the backend-neutral session orchestrator. |
| `crates/wisp-audiokit-sys` | Raw C ABI bindings to the macOS `WispAudioKit` library. |
| `crates/wisp-lifecycle` | Session lifecycle state machine used by the runtime and formal verification. |
| `crates/wisp-storage` | Session/segment persistence on SQLite (bundled `rusqlite`). |
| `native/WispAudioKit` | macOS Swift package handling Core Audio Process Tap capture and `SpeechAnalyzer` transcription. Linked into the Rust binary as a static library. |

Roughly, data flows like this:

```
Core Audio Process Tap ─┐
                        ├─► WispAudioKit ─► wisp-audiokit ─► wisp-desktop (GPUI)
Microphone input ───────┘        │                              ▲
                                 ├─► SpeechAnalyzer ─────────────┘
                                 └─► recordings (Ogg/Opus)      │
                                                                └─► wisp-storage (SQLite)
```

### Cross-platform audio boundary

- `wisp-core` owns stable `TrackId`/`SourceKind`, device-native
  `AudioFormat`/`AudioFrame`, `CaptureEvent`, and partial/final
  `TranscriptEvent` contracts. The existing `SourceLabel::Mic` and
  `SourceLabel::System` map to fixed track IDs, so storage and UI behavior stay
  compatible while future application/process tracks remain possible.
- `wisp-audiokit` owns capability probes, `CaptureBackend` and
  `TranscriberBackend`, and `SessionOrchestrator`. macOS production capture and
  transcription run through concrete `MacosCaptureBackend` and
  `MacosTranscriberBackend` adapters managed by `SessionOrchestrator`. Swift
  sends typed mic/system PCM into a bounded, nonblocking capture queue while
  retaining Ogg/Opus recording. Capture PCM reaches Rust first;
  `MacosTranscriberBackend::push` then submits only frames accepted by
  `SessionOrchestrator` back to `SpeechAnalyzer`. `MacosCaptureBackend` also
  has an independent recording-only constructor so another transcriber could
  consume the exposed PCM without requesting speech permission. Transcript and
  compatibility callbacks retain their original ordering. The legacy `Session`
  API remains available but is no longer the macOS desktop path.
- Real-time capture producers use a bounded, non-blocking frame queue.
  Overflow is reported as a `CaptureEvent` with the affected track and dropped
  PCM frame count (not packet count). The separate control queue is also
  bounded and cannot carry sample payloads. Microphone and system audio remain
  separate tracks.

## Requirements

- **macOS 26 (Tahoe)** — Wisp relies on `SpeechAnalyzer`, Core Audio Process Taps, and the new Metal Toolchain, so macOS 26 is required.
- **Xcode 26** — for the Swift 6.0 / macOS 26 SDK.
- **Rust 1.97.1** — pinned in `rust-toolchain.toml`.
- Microphone and speech-recognition permissions. macOS will prompt on first launch.

## Setup and usage

On macOS, first launch shows the microphone and speech-recognition permissions
needed for capture and on-device transcription. The desktop currently requests
the `ja-JP` transcription locale for each session.

To record and review a session:

1. Select **New Session**, then **Record**. Wisp keeps microphone and system
   audio as separate tracks.
2. Use **Mute mic** when needed, then **Stop** to drain and finalize the
   recording.
3. Open a saved session from the library to review its transcript.
4. Use **Copy** for `[MIC]` / `[SYS]` plain text, or **Export** for a Markdown
   file with YAML frontmatter.

The application menu also provides these shortcuts:

| Action | Shortcut |
| --- | --- |
| Start / stop recording | <kbd>⌘R</kbd> |
| Copy transcript | <kbd>⌘⇧C</kbd> |
| Export transcript | <kbd>⌘⇧E</kbd> |

## Build & run

A [Nix](https://nixos.org/) flake is included, so the dev environment is one command away:

```bash
# Enter the dev shell
nix develop

# Run a debug build
cargo run -p wisp-desktop
```

The `default` dev shell is turnkey on macOS: it provides the pinned Rust
toolchain, `sccache`, the `treefmt` formatter, the `cachix` CLI, applies the
macOS Xcode/`DEVELOPER_DIR` handling automatically, and installs the project's
pre-commit git hooks (`treefmt` + `clippy`) on entry. If you use
[direnv](https://direnv.net/), the committed `.envrc` (`use flake`) does all of
this on `cd`.

The flake exposes the `wisp-desktop` package and the portable CI checks:

```bash
nix build .#wisp-desktop

# Run the checks available for the current Nix platform
nix flake check
```

On Linux, `nix flake check` runs the unified `treefmt` formatting check plus
Crane-backed Clippy and tests for the workspace excluding `wisp-desktop`. On
macOS, it runs `treefmt` and evaluates the `wisp-desktop` package; use the
explicit Cargo commands under [Contributing](#contributing) for workspace-wide
lint and test coverage.

Formatting is unified through [treefmt-nix](https://github.com/numtide/treefmt-nix):
`nix fmt` (or `treefmt` inside the dev shell) formats Nix (`nixfmt`), Rust
(`rustfmt`, pinned to the workspace toolchain), and Swift (`swiftformat`) in one
pass, and the same configuration backs the `treefmt` flake check.

### Build caching

The flake declares the [nix-community](https://nixos.org/manual/nix/stable/command-ref/conf-file#conf-substituters)
binary cache in `nixConfig`, which (for trusted users) supplies prebuilt
ancillary tooling from the wider Nix ecosystem — treefmt-nix, git-hooks.nix,
and similar dependencies. It does **not** host this project's own Crane build
outputs; to cache and share those, publish them to a project
[Cachix](https://www.cachix.org/) cache. The default dev shell ships the
`cachix` CLI, so once a cache exists you can opt in with:

```bash
cachix use <cache-name>
```

Within a single `nix flake check`, Crane reuses one `buildDepsOnly`
(`cargoArtifacts`) derivation across the package, Clippy, and test checks, so
the workspace dependencies are compiled once and reused.

If you'd rather use Rust + Xcode directly:

```bash
cargo build -p wisp-desktop --release
```

### Custom data directory

Set `WISP_DATA_DIR` to override where `sessions.db` and the `recordings/`
directory are stored. When unset, Wisp uses
`$HOME/Library/Application Support/dev.mokmok.wisp`. `HOME` must be available
unless `WISP_DATA_DIR` is set. Settings are stored as `settings.json`.

If a completed transcript cannot be committed to SQLite, Wisp writes an
atomic `transcript-recovery.json` beside that session's Ogg files, blocks a
new recording, and retries reconciliation immediately or on the next launch.
Wisp exits before recording if the durable database cannot be opened; it never
treats an in-memory fallback as successful persistence.

### Formal verification

The session worker protocol and navigation/session guards are checked against
the production Rust implementation with Kani and Shuttle:

```bash
bash formal/check.sh
```

See [`formal/README.md`](formal/README.md) for setup, verified properties, and
the extension workflow.

See `.github/workflows/release.yaml` for how the release `.app` bundle is produced — pushing a `v*` tag builds `Wisp.app` on a macOS 26 runner.

## Roadmap

- [x] Copy transcript to clipboard (plain text) and export as Markdown (`.md`) with a lightweight, CloudEvents-inspired YAML frontmatter envelope (`id`, `type`, `source`, `time`, `subject`, …).
- [ ] Export to SRT / JSON.
- [ ] Speaker diarization within a single channel.

## Contributing

Issues and pull requests are welcome. Before sending a PR, run the checks that
apply to your platform:

```bash
# Nix checks (the check set is platform-dependent; see Build & run)
nix flake check

# Complete Rust workspace checks on a supported host
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets

# Formal lifecycle checks (requires cargo-kani)
bash formal/check.sh

# Swift formatting (available in the Nix CI shell)
nix develop .#ci --command swiftformat --lint \
  native/WispAudioKit/Sources native/WispAudioKit/Tests

# macOS 26 only
swift test --package-path native/WispAudioKit
```

## License

TBD (will be added before public release).
