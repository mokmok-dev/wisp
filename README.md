# Wisp

**A fully offline recording & transcription desktop app.**

Wisp captures your microphone and system audio (the other side of a call) at the same time and transcribes both on-device. Audio and text never leave your machine.

> macOS 26 (Tahoe) and Windows are supported. Linux support is coming soon.

---

## Features

- **Fully offline** — Audio and transcripts stay on your device. Wisp works with Wi-Fi turned off.
- **On-device transcription** — Uses [`SpeechAnalyzer`](https://developer.apple.com/documentation/speech), the new API in Apple's Speech framework. No cloud APIs.
- **System audio + microphone capture** — Uses macOS 14.4+ [Core Audio Process Taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps) to tap meeting-app output without prompts, mixes it with your mic input, and merges both sides into a single transcript.
- **Built in Rust with a GPU-rendered UI** — The UI is built on [GPUI](https://www.gpui.rs/), the framework that powers the [Zed](https://zed.dev/) editor. Native-feeling responsiveness and smooth scrolling.
- **Simple local storage** — Recordings are stored as WAV and metadata as SQLite under `~/Library/Application Support/dev.mokmok.wisp/`. Easy to export and analyze later.

## Screenshots

![Wisp session library — past sessions and a New Session entry point](docs/screenshot.png)

## Architecture

Wisp is a small Cargo workspace with cleanly separated concerns:

| Crate / target | Responsibility |
| --- | --- |
| `apps/wisp-desktop` | GPUI desktop shell. Renders recording state and the transcript view. |
| `crates/wisp-core` | Shared, platform-agnostic types (`Session`, `Segment`, IDs, `SourceLabel`). |
| `crates/wisp-audiokit` | Safe Rust wrapper around the Swift `WispAudioKit` framework. |
| `crates/wisp-audiokit-sys` | Raw C ABI bindings to `WispAudioKit`. |
| `crates/wisp-storage` | Session/segment persistence on SQLite (bundled `rusqlite`). |
| `native/WispAudioKit` | Swift package (macOS) handling Core Audio Process Tap capture and `SpeechAnalyzer` transcription. |
| `crates/wisp-audiokit-win` | Windows static library: WASAPI mic + loopback capture and Vosk on-device transcription. |

Roughly, data flows like this:

```
Core Audio Process Tap ─┐
                        ├─► WispAudioKit ─► wisp-audiokit ─► wisp-desktop (GPUI)
Microphone input ───────┘        │                              ▲
                                 └─► SpeechAnalyzer ────────────┘
                                          │
                                          └─► wisp-storage (SQLite + WAV)
```

## Requirements

### macOS

- **macOS 26 (Tahoe)** — Wisp relies on `SpeechAnalyzer`, Core Audio Process Taps, and the new Metal Toolchain.
- **Xcode 26** — for the Swift 6.0 / macOS 26 SDK.
- **Rust 1.95+** — pinned in `rust-toolchain.toml`.
- Microphone and speech-recognition permissions (macOS prompts on first launch).

### Windows

- **Windows 10/11** (x64)
- **Rust 1.95+** — pinned in `rust-toolchain.toml`.
- **[Vosk](https://alphacephei.com/vosk/)** native library on `PATH` (see [vosk-api releases](https://github.com/alphacep/vosk-api/releases); CI uses `vosk-win64-0.3.45.zip`).
- A Vosk language model under `%LOCALAPPDATA%\dev.mokmok.wisp\models\` (e.g. `vosk-model-small-ja-0.22` for Japanese), or set `WISP_VOSK_MODEL` to the model directory.
- Microphone privacy enabled for desktop apps (Settings → Privacy → Microphone).

## Build & run

A [Nix](https://nixos.org/) flake is included, so the dev environment is one command away:

```bash
# Enter the dev shell
nix develop

# Run a debug build
cargo run -p wisp-desktop
```

On **Linux** and **Windows**, `nix develop` also provides **Vosk** (`libvosk` + a small Japanese model) for the Windows audio backend. **WASAPI** is handled by the Rust `wasapi` crate at build time and does not need a separate Nix package. macOS dev shells use Xcode/Swift instead.

```bash
# Optional: build only the Vosk packages
nix build .#vosk-api .#vosk-model-small-ja
```

If you'd rather use Rust + Xcode directly:

```bash
cargo build -p wisp-desktop --release
```

See `.github/workflows/release.yaml` for release builds — pushing a `v*` tag builds `Wisp.app` on macOS and `wisp-desktop.exe` on Windows.

### Windows build

```powershell
# Install Vosk DLLs (add the extracted folder to PATH)
# Download a model, e.g. vosk-model-small-ja-0.22, into:
#   %LOCALAPPDATA%\dev.mokmok.wisp\models\vosk-model-small-ja-0.22\

cargo build -p wisp-desktop --release
```

Recordings and the SQLite database live under `%LOCALAPPDATA%\dev.mokmok.wisp\` (override with `WISP_DATA_DIR`).

### Custom output directory

Set `WISP_DATA_DIR` to override the application data root (sessions DB, recordings, Vosk models). When unset:

- macOS: `~/Library/Application Support/dev.mokmok.wisp/`
- Windows: `%LOCALAPPDATA%\dev.mokmok.wisp\`

## Roadmap

- [x] **Windows support** — WASAPI loopback + microphone capture with Vosk on-device transcription.
- [ ] **Linux support** — exploring PipeWire monitor sources paired with a local Whisper-family model.
- [x] Copy transcript to clipboard and export as plain text (.txt).
- [ ] Export to Markdown / SRT / JSON.
- [ ] Speaker diarization within a single channel.

## Contributing

Issues and pull requests are welcome. Before sending a PR, please make sure `cargo fmt`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` pass under the same conditions as CI. For the Swift side, `make -C native/WispAudioKit` runs the equivalent checks.

## License

TBD (will be added before public release).
