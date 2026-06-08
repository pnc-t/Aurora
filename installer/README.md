# 配布インストーラ

Aurora は配布物が 2 つに分かれる。

| 配布物 | 中身 | インストーラ | 生成方法 |
|--------|------|--------------|----------|
| **client** | Tauri アプリ (`aurora-client`) | NSIS (per-user) | `cargo tauri build` |
| **host** | 素のバイナリ (`aurora-host`) | Inno Setup (per-machine) | `installer\host\build.ps1` |

共通の前提:
- **Tailscale は同梱しない**。利用者が事前に導入・ログイン済みであること。
- インストーラは未署名。Windows SmartScreen / Defender に警告される場合がある
  (リモート操作 + 入力注入 + 画面キャプチャという性質上、誤検知もあり得る)。

---

## client (Tauri / NSIS)

設定は `crates/client/src-tauri/tauri.conf.json` の `bundle`:
- `targets: ["nsis"]` … NSIS のみ生成 (アンインストーラ同梱でクリーンに削除可能)。
- `windows.nsis.installMode: "currentUser"` … 管理者不要のユーザーインストール。
- `windows.webviewInstallMode: downloadBootstrapper` … WebView2 を**オンライン取得**
  (実行環境がネット接続できる前提)。

ビルド:

```powershell
cargo tauri build --manifest-path crates/client/src-tauri/Cargo.toml
```

出力: `target\release\bundle\nsis\Aurora_0.1.0_x64-setup.exe`
(初回は Tauri が makensis / WebView2 ブートストラッパを自動取得する)。

アンインストール: 「アプリと機能」または同梱アンインストーラから削除。
ユーザー設定 (`%APPDATA%` の `recent_hosts.json` 等) は手動削除。

---

## host (Inno Setup)

`installer/host/aurora-host.iss` がインストーラ定義。host 自身の
`--install` / `--uninstall` (ONLOGON スケジュールタスク登録) に委譲しているので、
インストーラは「exe 配置 + タスク登録 + 任意で ProjFS 有効化」だけを行う。

前提ツール (初回のみ):

```powershell
winget install JRSoftware.InnoSetup
```

ビルド:

```powershell
.\installer\host\build.ps1
```

出力: `installer\host\dist\aurora-host-setup-0.1.0.exe`

インストーラの挙動 (管理者権限):
- `aurora-host.exe` を `C:\Program Files\Aurora Host\` に配置。
- タスク「ログオン時に自動起動」(既定 ON) → `aurora-host --install` で
  ONLOGON / RL HIGHEST のスケジュールタスク "Aurora Host" を登録。
- タスク「ファイル転送機能を有効化」(既定 OFF) → `Client-ProjFS` を有効化
  (再起動が必要な場合あり)。不要なら OFF のまま。

**接続パスワード**はインストーラでは設定しない。インストール後にトレイアイコン
右クリック →「パスワード変更」、または端末から `aurora-host --set-password` を
一度実行する (未設定だと起動ごとにランダム生成される)。

### クリーンアンインストール

アンインストーラは痕跡を残さず削除する:
1. 常駐プロセス (`aurora-host.exe`) を停止。
2. `aurora-host --uninstall` でスケジュールタスク "Aurora Host" を解除。
3. exe とインストール先フォルダを削除。
4. `%LOCALAPPDATA%\Aurora\` (port / password.bin) を削除。

ProjFS (`Client-ProjFS`) は他用途でも使う共有システム機能のため、
アンインストールでは**無効化しない**。手動で戻す場合:

```powershell
Disable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
```
