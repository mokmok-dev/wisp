# AGENTS.md

## Cursor Cloud specific instructions

macOS 向けオンデバイス会議文字起こしアプリ。開発環境は Nix flake で管理。

### Nix daemon の起動

update script が `nix-daemon` を起動するが、万が一動いていなければ:

```sh
sudo systemctl start nix-daemon.socket
```

### 開発シェル

全てのコマンドは `nix develop .#default` 経由で実行する（Rust toolchain + sccache）:

```sh
nix develop .#default --quiet --command <cmd>
```

### コンポーネント

| コンポーネント | 言語 | パス | Cloud Agent スコープ |
|---|---|---|---|
| Rust workspace (`wisp-desktop`, `wisp-core`, `wisp-storage`) | Rust 1.95.0 | `/` (root `Cargo.toml`) | Full lint/test/build/run |
| WispAudioKit + wispctl | Swift 6.0 | `native/WispAudioKit/` | Lint のみ; ビルドは macOS 26.0+ 必須 |

### Rust（CI 同等）

`.github/workflows/rust.yaml` 参照:

```sh
nix develop .#default --quiet --command cargo fmt --all -- --check
nix develop .#default --quiet --command cargo clippy --workspace --all-targets -- -D warnings
nix develop .#default --quiet --command cargo test --workspace --all-targets
nix develop .#default --quiet --command cargo run -p wisp-desktop
```

### Swift lint（Linux では lint のみ）

Swift コード (`native/WispAudioKit/`) は macOS 26.0+ 専用。Linux ではフォーマット lint のみ可能。`swiftformat` は `.#ci` シェルに含まれる:

```sh
nix develop .#ci --quiet --command swiftformat --lint native/WispAudioKit/Sources
```

### Nix フォーマットチェック

```sh
nix develop .#ci --quiet --command nixfmt --check flake.nix
```

### 注意点

- `.#default` シェルは `RUSTC_WRAPPER=sccache` を自動設定する。
- `.#ci` シェルは `rustToolchain` + `nixfmt` + `swiftformat` を提供する（sccache なし）。
- Workspace lint は厳格: `clippy::all` + `clippy::pedantic` を warn、`unsafe_code` を deny。`Cargo.toml` `[workspace.lints]` 参照。
- `.rustfmt.toml` は edition 2024 スタイル、`fn_params_layout = "Vertical"`。
