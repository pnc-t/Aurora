<div align="center">
  <img src="docs/assets/aurora.png" alt="Aurora" width="160" />

  # Aurora

  ![Rust](https://img.shields.io/badge/Rust-2024-000000?logo=rust&logoColor=white)
  ![Tauri](https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri&logoColor=white)
  ![TypeScript](https://img.shields.io/badge/TypeScript-3178C6?logo=typescript&logoColor=white)
  ![Tailscale](https://img.shields.io/badge/Tailscale-required-242424?logo=tailscale&logoColor=white)
  ![Platform](https://img.shields.io/badge/host-Windows-0078D6?logo=windows&logoColor=white)
  ![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)

  [![CI](https://github.com/pnc-t/Aurora/actions/workflows/ci.yml/badge.svg)](https://github.com/pnc-t/Aurora/actions/workflows/ci.yml)
  [![Release](https://img.shields.io/github/v/release/pnc-t/Aurora?label=release&logo=github)](https://github.com/pnc-t/Aurora/releases/latest)
</div>

出先の PC から自宅 PC に接続して操作する、自作の軽量リモートデスクトップアプリ。
RDP / VNC / RustDesk 等を使わず、Rust でゼロから実装しています。設計目標は **「軽く・早く・快適」**。

> ⚠️ 個人プロジェクトです。**ネットワークは [Tailscale](https://tailscale.com/) (Tailnet) 直結を前提**にしており、NAT 越え・シグナリング・中継サーバは実装していません。独立した第三者によるセキュリティ監査は受けていません。自己責任で利用してください。

| | |
|--|--|
| 言語 / ランタイム | Rust 2024 edition + `tokio` |
| プラットフォーム | ホスト: Windows (主対象) / クライアント: Tauri v2 |
| ネットワーク | Tailscale (Tailnet) 直結前提 |
| ライセンス | MIT OR Apache-2.0 |

---

## ドキュメント

詳しい解説は [`docs/`](docs/README.md) にあります。

- [アーキテクチャ](docs/architecture.md) — クレート構成・パイプライン・タスク分割
- [プロトコル](docs/protocol.md) — TCP 制御 / UDP 映像・音声のワイヤフォーマット
- [セキュリティ](docs/security.md) — 暗号化・認証・脅威モデル・既知の制約
- [ビルドと実行](docs/build.md) — 必要環境・ビルド手順・CLI オプション
- [無人起動セットアップ](docs/remote-access-setup.md) — 自動起動 + WoL で出先から接続
- [トラブルシュート](docs/troubleshooting.md)

---

## ダウンロード

[![Download](https://img.shields.io/github/v/release/pnc-t/Aurora?label=Download&logo=github)](https://github.com/pnc-t/Aurora/releases/latest)

ビルド済みバイナリは [Releases](https://github.com/pnc-t/Aurora/releases/latest) から入手できます。

| 成果物 | 用途 |
|--------|------|
| `Aurora_<ver>_x64-setup.exe` | クライアント (操作する側) の NSIS インストーラ |
| `aurora-host.exe` | ホスト (操作される側) の単独実行ファイル |

`main` に `v*` タグを push すると GitHub Actions が上記を自動ビルドして Release に添付します
(まだ初回リリース未公開のため、公開後にリンクが有効になります)。ソースからビルドする場合は
[docs/build.md](docs/build.md) を参照してください。

> ⚠️ 配布バイナリは未署名です。SmartScreen / Defender に警告される場合があります
> (詳細は [docs/security.md](docs/security.md))。

---

## 特徴

- **低遅延**: 1080p / 60fps を目標に、GPU ハードウェアエンコード + WebCodecs GPU デコードで End-to-End 体感遅延 < 50ms (Tailnet 直結時) を狙う設計
- **軽量**: ホスト CPU 使用率 < 15% (GPU エンコード前提)、クライアント UI は Vanilla TypeScript でバンドル軽量
- **ゼロコピー映像パイプライン**: DXGI Desktop Duplication → Media Foundation H.264 を GPU 上で完結 (CPU コピーを避ける)
- **役割分離プロトコル**: 制御は TCP、映像/音声は UDP。遅延予算のために独自プロトコルで設計
- **エンドツーエンド暗号化**: Noise XX-PSK ハンドシェイク。PSK はパスワードから Argon2id で導出
- **付加機能**: マウス/キー入力注入、クリップボード同期、音声 (Opus)、ファイル転送、ProjFS によるドライブ投影、画質プリセット、解像度追従 (RDP 風) と仮想ディスプレイ、マルチタブ接続、自動再接続

---

## アーキテクチャ

「Port & Adapter (Hexagonal)」風の構成。OS 依存部はトレイト (Port) で定義し、OS 別実装 (Adapter) を `cfg` で切り替えます。

```
crates/
├── common/     共有: プロトコル型 (proto)、Noise 暗号化 (noise)、UDP フレーミング (framing)
├── host/       操作される側 (サーバ)
│   ├── capture/   画面キャプチャ (Win: DXGI / Mac: ScreenCaptureKit / Linux: PipeWire)
│   ├── encode/    H.264 ハードウェアエンコード (Win: Media Foundation ほか)
│   ├── input/     マウス/キー入力注入
│   └── net.rs     TCP 制御 + UDP 映像のサーバ実装
└── client/     操作する側 (Tauri v2 + Vanilla TS)
    ├── src-tauri/   Rust 側ネットワーク、Webview への NALU 受け渡し
    └── src/         フロント (core/ = ブリッジ・ランタイム、ui/ = UI コンポーネント)
```

### 技術スタック

| 領域 | 採用 |
|------|------|
| 言語 / ランタイム | Rust 2024 edition / `tokio` |
| クライアント GUI | Tauri v2 + Vanilla TypeScript + Vite (フレームワーク非採用) |
| 画面キャプチャ | DXGI Desktop Duplication (Windows) |
| 映像コーデック | H.264 ハードウェアエンコード (Media Foundation) / デコードは WebCodecs `VideoDecoder` (GPU) |
| 音声 | `cpal` (loopback) + Opus (20ms frame) |
| 暗号化 | `snow` (Noise XX-PSK) + `argon2` (PSK 導出) |
| シリアライズ | `bincode` (制御) + 生バイト (映像) |

---

## 必要環境

- **Rust** (2024 edition 対応のツールチェイン)
- **Node.js + pnpm** (クライアントのフロントエンドビルド)
- **Tauri v2 CLI** (`cargo install tauri-cli` など)
- **Windows** (ホスト/クライアントとも現状 Windows を主対象。Mac/Linux は移植途上)
- 推奨: ホスト・クライアント双方に **Tailscale** をインストール (LAN/localhost でも動作検証は可能)

---

## ビルドと実行

> Windows / PowerShell 前提。以下はクイックスタートです。CLI オプションや配布インストーラを含む詳細は [docs/build.md](docs/build.md) を参照してください。

### ホスト (操作される側)

```powershell
# 起動 (debug)。リリース計測時は --release
cargo run -p aurora-host

# 接続パスワードを設定 (初回。トレイアイコンからも設定可)
cargo run -p aurora-host -- --set-password
```

既定で TCP/UDP ともポート `7000` を待ち受けます。リリースビルドはコンソール無しでバックグラウンド + システムトレイ常駐になり、トレイアイコンからパスワード変更・終了ができます。

### クライアント (操作する側)

```powershell
# 開発モード (Tauri ウィンドウ + Vite dev サーバ)
cargo tauri dev          # crates/client/src-tauri から実行

# 本番ビルド
cargo tauri build        # crates/client/src-tauri から実行
```

クライアントのランチャーで接続先 (Tailnet IP / ホスト名 / `localhost`)、ポート、パスワードを入力して接続します。接続後、上端のツールバーから全画面・マウスキャプチャ・ファイル転送・統計 HUD・ディスプレイ切替・画質プリセットを操作できます。

### ホスト CLI オプション (主なもの)

| オプション | 説明 |
|-----------|------|
| `--port N` | 待ち受けポート (既定 7000) |
| `--bind ADDR` | 待ち受けアドレス (既定: Tailscale IP 自動 → 無ければ `127.0.0.1` loopback に fail-closed。公開は `--bind <ip>` で明示) |
| `--set-password` / `--clear-password` | 接続パスワードの設定 / 削除 (DPAPI 保存) |
| `--install` / `--uninstall` | ログオン時自動起動タスクの登録 / 解除 |
| `--fps N` | 送出 fps 上限 (0 = 無制限) |
| `--max-bitrate N` | ビットレート上限 (Mbps、0 = 無制限) |
| `--log-level LEVEL` | ログレベル (`RUST_LOG` でも指定可、例 `aurora=debug`) |

---

## ネットワークとセキュリティ

- **Tailscale (Tailnet) 直結が前提**。NAT 越え・シグナリング・中継サーバは実装しません (スコープを絞るための意図的な設計)
- **暗号化**: Noise XX-PSK でハンドシェイクし、以降のセッションを暗号化。PSK は接続パスワードから **Argon2id** (m=19MiB, t=2, p=1) で導出
- 制御チャネル (TCP) と映像/音声チャネル (UDP) を分離し、ノンスドメインでクロスチャネル注入を多重防御
- パスワードは OS の保護ストア (Windows: DPAPI) に保存し、ログには出しません
- ホストは既定で **Tailscale インターフェースのみに bind** し、LAN/インターネットへの露出を避けます
- 任意の 2 要素目として、クライアント静的鍵の許可リスト (デバイスピン留め、既定無効) を有効化できます

脅威モデルと既知の制約の詳細は [docs/security.md](docs/security.md) を参照してください。

---

## 対応プラットフォーム

| | Windows | macOS | Linux |
|--|---------|-------|-------|
| ホスト | ✅ 主対象 (キャプチャ/エンコード/入力/音声/ファイル) | 🚧 移植途上 | 🚧 移植途上 |
| クライアント | ✅ | (Tauri 依存) | (Tauri 依存) |

Windows を優先して完成させ、Mac/Linux は OS ポート (`cfg` 分岐の追加) でまとめて対応する方針です。

### 既知の制約

- **HDR ディスプレイのキャプチャ**: 現状 SDR 前提のトーンマップで取り込むため、HDR 有効なディスプレイでは映像が眩しく (白飛び気味に) なります

---

## ライセンス

以下のいずれかを選択して利用できます。

- Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

`SPDX-License-Identifier: MIT OR Apache-2.0`

アイコン画像は ChatGPT (OpenAI) で生成したものです。
