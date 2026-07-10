# Wisp

**A fully offline recording & transcription desktop app.**

Wisp captures your microphone and system audio (the other side of a call) at the same time and transcribes both on-device. Audio and text never leave your machine.

> macOS 26 (Tahoe) is the primary supported target. Windows support is in
> preview with `Windows.Media.SpeechRecognition` and local-model setup wiring.
> Linux support is coming soon.

---

## Features

- **Fully offline** — Audio and transcripts stay on your device. Wisp works with Wi-Fi turned off.
- **On-device transcription** — Uses [`SpeechAnalyzer`](https://developer.apple.com/documentation/speech), the new API in Apple's Speech framework on macOS. Windows preview builds can use `Windows.Media.SpeechRecognition` or prepare a local model from setup.
- **System audio + microphone capture** — Uses macOS 14.4+ [Core Audio Process Taps](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps) to tap meeting-app output without prompts, mixes it with your mic input, and merges both sides into a single transcript. Windows local-model work is structured around WASAPI mic + loopback capture.
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
| `crates/wisp-audiokit` | Safe Rust wrapper around platform audio/transcription backends. |
| `crates/wisp-audiokit-sys` | Raw C ABI bindings to `WispAudioKit`. |
| `crates/wisp-storage` | Session/segment persistence on SQLite (bundled `rusqlite`). |
| `native/WispAudioKit` | Swift package handling Core Audio Process Tap capture and `SpeechAnalyzer` transcription. Linked into the Rust binary as a static library. |

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

- **macOS 26 (Tahoe)** — Wisp relies on `SpeechAnalyzer`, Core Audio Process Taps, and the new Metal Toolchain, so macOS 26 is required for now.
- **Xcode 26** — for the Swift 6.0 / macOS 26 SDK.
- **Windows 10/11 preview** — uses `Windows.Media.SpeechRecognition` for the platform recognizer route and offers a setup route to download a local Whisper-family model under `%APPDATA%\dev.mokmok.wisp\models`.
- **Rust 1.96** — pinned in `rust-toolchain.toml`.
- Microphone and system-audio recording permissions. macOS will prompt on first launch.

## Build & run

A [Nix](https://nixos.org/) flake is included, so the dev environment is one command away:

```bash
# Enter the dev shell
nix develop

# Run a debug build
cargo run -p wisp-desktop
```

If you'd rather use Rust + Xcode directly:

```bash
cargo build -p wisp-desktop --release
```

See `.github/workflows/release.yaml` for how the release `.app` bundle is produced — pushing a `v*` tag builds `Wisp.app` on a macOS 26 runner.

### Custom output directory

Set `WISP_OUTPUT_DIR` to override where recordings are written. When unset, Wisp uses `~/Library/Application Support/dev.mokmok.wisp/recordings`.

### Local MCP bridge

Enable **Local MCP Bridge** from the Wisp library screen to expose the visible transcript through a local IPC endpoint. By default it binds to `http://127.0.0.1:8765/conversation`; set `WISP_IPC_ADDR=127.0.0.1:9001` to override the address while developing. Set `WISP_IPC_TOKEN` to require `Authorization: Bearer <token>` on IPC requests.

MCP hosts should run the bundled `wisp-mcp` binary over stdio, for example `/Applications/Wisp.app/Contents/MacOS/wisp-mcp`. The bridge provides the `ask_current_conversation` tool, fetches the current Wisp transcript from the IPC endpoint, and returns context for the host LLM to answer questions such as `いまの話ってどういうこと?`.

## Roadmap

- [ ] **Windows support** — preview setup and `Windows.Media.SpeechRecognition` route are in place; WASAPI loopback + local-model transcription is the remaining hardening path.
- [ ] **Linux support** — exploring PipeWire monitor sources paired with a local Whisper-family model.
- [x] Copy transcript to clipboard and export as plain text (.txt).
- [ ] Export to Markdown / SRT / JSON.
- [ ] Speaker diarization within a single channel.

## Contributing

Issues and pull requests are welcome. Before sending a PR, please make sure `cargo fmt`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` pass under the same conditions as CI. For the Swift side, `make -C native/WispAudioKit` runs the equivalent checks.

## License

TBD (will be added before public release).
