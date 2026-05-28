# Wisp

**完全オフラインで動く、録音 & 文字起こしデスクトップアプリ。**

Wisp はマイクとシステム音声（会議相手の声）を同時に取り込み、すべてデバイス内で文字起こしを行います。録音データも文字起こし結果もネットワークには一切送信されません。

> 現在 macOS 26 (Tahoe) のみ対応。Windows / Linux は近日公開予定です。

---

## 特徴

- **完全オフライン** — 音声・テキストともに端末外には出ません。Wi-Fi を切っても動作します。
- **オンデバイス文字起こし** — macOS の [Speech](https://developer.apple.com/documentation/speech) framework に新しく加わった `SpeechAnalyzer` をそのまま利用します。クラウド API は不要です。
- **システム音声 + マイクの同時キャプチャ** — macOS 14.4 以降の [Core Audio Process Tap](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps) を使い、会議アプリの出力を権限プロンプトなしでタップします。マイク入力と合成し、相手と自分の発話を 1 本のトランスクリプトに統合します。
- **Rust 製、GPU レンダリング UI** — UI は [Zed](https://zed.dev/) エディタを支える [GPUI](https://www.gpui.rs/) で実装。ネイティブ並みの応答性と滑らかなスクロールを実現します。
- **シンプルなローカル保存** — 録音は WAV、メタデータは SQLite (`~/Library/Application Support/dev.mokmok.wisp/`) に保存されます。後から自由にエクスポート・解析できます。

## スクリーンショット

_（後日追加予定）_

## アーキテクチャ

Wisp は小さな workspace に分かれており、責務をはっきり分離しています。

| クレート / ターゲット | 役割 |
| --- | --- |
| `apps/wisp-desktop` | GPUI で書かれたデスクトップシェル。録音状態と転写ビューを描画。 |
| `crates/wisp-core` | クレート横断で使う型（`Session`、`Segment`、ID、`SourceLabel` など）。プラットフォーム非依存。 |
| `crates/wisp-audiokit` | Swift 製 `WispAudioKit` を包む安全な Rust ラッパ。 |
| `crates/wisp-audiokit-sys` | `WispAudioKit` の C ABI を直接バインド。 |
| `crates/wisp-storage` | SQLite (rusqlite, bundled) を使ったセッション / セグメント永続化。 |
| `native/WispAudioKit` | Swift パッケージ。`Core Audio Process Tap` でのキャプチャと `SpeechAnalyzer` での転写を担う。静的ライブラリとして Rust 側にリンクされる。 |

データの流れはおおむね次のとおりです。

```
Core Audio Process Tap ─┐
                        ├─► WispAudioKit ─► wisp-audiokit ─► wisp-desktop (GPUI)
マイク入力 ─────────────┘        │                              ▲
                                 └─► SpeechAnalyzer ────────────┘
                                          │
                                          └─► wisp-storage (SQLite + WAV)
```

## 動作要件

- **macOS 26 (Tahoe)** — `SpeechAnalyzer`、`Core Audio Process Tap`、Metal Toolchain の新 API を利用しているため、現状 macOS 26 が必須です。
- **Xcode 26** — Swift 6.0 / macOS 26 SDK を含むバージョン。
- **Rust 1.95** — `rust-toolchain.toml` で固定されています。
- マイクとシステム音声録音の権限。初回起動時に macOS が確認します。

## ビルド & 実行

[Nix](https://nixos.org/) flake を同梱しているので、開発環境はワンコマンドで揃います。

```bash
# 開発シェルに入る
nix develop

# デバッグビルドで起動
cargo run -p wisp-desktop
```

Nix を使わず Rust + Xcode を直接使う場合：

```bash
cargo build -p wisp-desktop --release
```

リリース時の `.app` バンドル化は `.github/workflows/release.yaml` を参照してください。タグ `v*` を push すると macOS 26 ランナーで `Wisp.app` が生成されます。

### 出力先のカスタマイズ

`WISP_OUTPUT_DIR` 環境変数で録音ファイルの保存先を上書きできます。未指定時は `~/Library/Application Support/dev.mokmok.wisp/recordings` に保存されます。

## ロードマップ

- [ ] **Windows 対応** — WASAPI ループバック + Windows.Media.SpeechRecognition / ローカルモデルの組み合わせを検証中。
- [ ] **Linux 対応** — PipeWire のモニタソース + ローカル Whisper 系モデルを検討。
- [ ] エクスポート機能（Markdown / SRT / JSON）。
- [ ] 話者分離（同一チャネル内）。

## コントリビュート

Issue / Pull Request 歓迎です。`cargo fmt`・`cargo clippy --workspace --all-targets`・`cargo test --workspace` が CI と同じ条件で通ることを確認してから送ってください。Swift 側は `make -C native/WispAudioKit` で同じチェックが走ります。

## ライセンス

未定（公開時に追記）。
