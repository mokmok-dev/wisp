# AGENTS.md

## Cursor Cloud specific instructions

This is a macOS desktop app (on-device meeting transcription) with two main components:

### Components

| Component | Language | Path | Cloud Agent Scope |
|---|---|---|---|
| Rust workspace (`wisp-desktop`, `wisp-core`, `wisp-storage`) | Rust 1.95.0 | `/` (root `Cargo.toml`) | Full lint/test/build/run |
| WispAudioKit + wispctl | Swift 6.0 | `native/WispAudioKit/` | Lint only (`swiftformat`); build requires macOS 26.0+ |

### Rust (full CI parity)

Standard CI commands from `.github/workflows/rust.yaml`:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace --all-targets`
- `cargo run -p wisp-desktop` — runs the skeleton desktop app

### Swift (lint only on Linux)

The Swift code (`native/WispAudioKit/`) targets macOS 26.0+ and cannot be compiled on Linux. Only formatting lint is possible:

- `swiftformat --lint native/WispAudioKit/Sources`

The `swiftformat` binary (v0.61.1) must be installed separately — see update script. Config is in `.swiftformat` at the repo root.

### Non-obvious notes

- The project uses Nix flakes + direnv (`.envrc`) for local dev on macOS. Cloud Agents skip Nix and install Rust via `rustup` (already present) and `swiftformat` from GitHub releases.
- `rust-toolchain.toml` pins Rust 1.95.0; `rustup` auto-resolves this.
- Workspace lints are strict: `clippy::all` + `clippy::pedantic` at warn level, `unsafe_code` denied. See `Cargo.toml` `[workspace.lints]`.
- `.rustfmt.toml` uses edition 2024 style with `fn_params_layout = "Vertical"`.
