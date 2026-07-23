# ビルドと実行

> Windows / PowerShell を前提に書いています。性能計測は必ず `--release` で行ってください
> (debug ビルドのフレームレートは信用しない)。

## 必要環境

- **Rust** (2024 edition 対応のツールチェイン)
- **Node.js + pnpm** (クライアントのフロントエンドビルド)
- **Tauri v2 CLI** (`cargo install tauri-cli` など)
- **Windows** (ホスト/クライアントとも現状 Windows を主対象。Mac/Linux は移植途上)
- 推奨: ホスト・クライアント双方に **Tailscale** (LAN/localhost でも動作検証は可能)

## ワークスペース

```powershell
cargo build --workspace                 # 全クレートビルド
cargo build --workspace --release       # リリース (パフォーマンス計測時は必須)
cargo test --workspace                  # テスト
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                         # 整形 (コミット前)
```

## ホスト (操作される側)

```powershell
# 起動 (debug)。リリース計測時は --release
cargo run -p aurora-host

# 接続パスワードを設定 (初回。トレイアイコンからも設定可)
cargo run -p aurora-host -- --set-password
```

既定で TCP/UDP ともポート `7000` を待ち受けます。bind 先は既定で Tailscale インターフェース
のみ (未導入時は `127.0.0.1` loopback に fail-closed。LAN/インターネットへは到達不能で、公開は
`--bind <ip>` で明示)。リリースビルドはコンソール無しでバックグラウンド + システム
トレイ常駐になり、トレイアイコンからパスワード変更・終了ができます。

無人起動 (自動起動 + 固定パスワード + WoL) の構成は
[remote-access-setup.md](remote-access-setup.md) を参照してください。

### ホスト CLI オプション

| オプション | 説明 |
|------|------|
| `--port N` | 待ち受けポート (既定 7000) |
| `--bind ADDR` | 待ち受けアドレス。未指定なら Tailscale IP を自動採用し、無ければ `127.0.0.1` (loopback) に fail-closed。LAN テストや公開は明示指定 (LAN IP、または全 NIC なら `--bind 0.0.0.0`) |
| `--set-password` / `--clear-password` | 接続パスワードの設定 / 削除 (DPAPI 保存)。接続中は拒否・現在のパスワード照合あり |
| `--install` / `--uninstall` | ログオン時自動起動タスク "Aurora Host" の登録 / 解除 (`--install` は他の引数をタスクへ転送) |
| `--fps N` | 送出 fps 上限 (0 = 無制限) |
| `--max-bitrate N` | ビットレート上限 (Mbps、0 = 無制限) |
| `--encode-gpu NAME` | エンコードに使う GPU を名前の部分一致で指定 (ハイブリッド GPU ノートでキャプチャ/エンコード GPU がずれる場合に使う)。`auto` で保存値を消し既定 (キャプチャと同一 GPU) に戻す。明示指定は永続化される |
| `--align-vdd` | 仮想ディスプレイ (VDD) の描画 GPU をキャプチャ GPU に一致させ、必要ならデバイスを再起動して終了する。インストーラが VDD 登録直後に呼ぶワンショットコマンド (通常は手動実行不要) |
| `--perf` | エンコード遅延・ソースビットレートを 1 秒窓で info ログに出す |
| `--log-level LEVEL` | ログレベル (`debug` 等、または `aurora=debug,snow=warn` の完全指定)。`RUST_LOG` でも可 |

診断用の追加オプションとして `--list-displays` (DXGI アダプタ/出力の列挙) もあります。

> ログは既定で `aurora=info`。優先順位は `--log-level` > `RUST_LOG` > 既定。トレイ常駐
> (コンソールなし) では `%LOCALAPPDATA%\Aurora\logs\host.<日付>.log` に日次ローテ
> (7 世代保持) で書きます。

## クライアント (操作する側)

```powershell
# 開発モード (Tauri ウィンドウ + Vite dev サーバ)
# tauri CLI は --manifest-path 非対応。src-tauri 直下で実行する
pushd crates/client/src-tauri; cargo tauri dev; popd

# 本番ビルド (NSIS インストーラ生成)
# 出力: target/release/bundle/nsis/Aurora_<ver>_x64-setup.exe
pushd crates/client/src-tauri; cargo tauri build; popd
```

接続はランチャーで接続先 (Tailnet IP / ホスト名 / `localhost`)、ポート、パスワードを入力して
行います。接続後、上端のツールバーから全画面・マウスキャプチャ・ファイル転送・統計 HUD・
ディスプレイ切替・画質プリセットを操作できます。

### フロントエンド単体 (UI 開発時)

```powershell
# crates/client/ で
pnpm install      # 初回 or 依存追加後
pnpm dev          # Vite 単体起動 (Tauri なし)
pnpm build
```

## Wake-on-LAN リレー (aurora-wol)

出先から自宅 PC の電源を入れたい場合は、自宅 LAN の常時稼働機 (Raspberry Pi 等) で
`crates/wol` (`aurora-wol`) を常駐させます。ビルド・デプロイ・運用の詳細は
[`crates/wol/README.md`](../crates/wol/README.md) を参照してください。

```powershell
# Windows 開発機から raspi へワンコマンドでクロスビルド・配備・systemd 化
cd crates/wol
.\deploy-aurora-wol.ps1 -Target pi@100.x.x.x
```

## 配布インストーラ

client (Tauri/NSIS) と host (NSIS) の 2 つを生成し `installer\dist\` に集約します。
一括ビルドは:

```powershell
.\installer\build-all.ps1
```

詳細・前提ツール・クリーンアンインストール手順は [`installer/README.md`](../installer/README.md)
を参照してください。

## 自動アップデート

host/client とも GitHub Releases から自動アップデートします。

- **client**: 公式 `tauri-plugin-updater` を使い、`latest.json` をエンドポイントとして
  チェックします。契機は**起動時**と**6 時間ごと**。新版があればランチャーに更新バナーが出て、
  ワンクリックでダウンロード → インストール → 再起動まで行います。
- **host**: `latest-host.json` を取得して semver 比較し、新版検知でトレイ通知 + トレイ
  右クリックメニューに更新項目を出します。クライアント接続中は適用を保留し、切断後
  (アイドル時) に実行します。ダウンロードした setup は **minisign 署名を検証**してから
  サイレント実行 (`/S`) します。
- host / client は**同一の minisign 鍵**で署名・検証しており、方式も共通です。この署名は
  更新配信物の真正性検証が目的で、配布インストーラ自体の Authenticode コード署名とは
  別物です (未署名のままなので SmartScreen / Defender の警告は従来どおり出ます)。

## Tailscale 接続確認

```powershell
tailscale status        # 双方がオンラインか
tailscale ip -4         # 自端末の 100.x.x.x
```

Tailscale 未導入の環境では LAN/localhost で動作確認 → 動いたら Tailnet IP で再検証、の
二段構えが手堅いです。
