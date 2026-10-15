# 配布インストーラ

Aurora は配布物が 4 つに分かれる。生成物はすべて **`installer\dist\`** に集約する。

| 配布物 | 中身 | インストーラ | 生成方法 |
|--------|------|--------------|----------|
| **client** | Tauri アプリ (`aurora-client`) | NSIS (per-user) | `cargo tauri build` |
| **host** | 素のバイナリ (`aurora-host`) | NSIS (per-machine) | `installer\host\build.ps1` |
| **wol** | raspi 向け静的バイナリ (`aurora-wol`) | 不要 (`install` サブコマンド内蔵) | `cargo zigbuild` (build-all 内) |
| **android** | Tauri Android アプリ (APK) | 不要 (APK 直接インストール) | `cargo tauri android build` (build-all の `-Android`) |

## 一括ビルド (推奨)

すべて生成して `installer\dist\` にまとめる:

```powershell
.\installer\build-all.ps1
```

出力:
- `installer\dist\Aurora_0.1.0_x64-setup.exe` (client)
- `installer\dist\aurora-host-setup-0.1.0.exe` (host)
- `installer\dist\aurora-wol-0.1.0-aarch64` (wol / raspi 用)

Android は既定ではビルドしない (前提が重いため opt-in)。含める場合:

```powershell
.\installer\build-all.ps1 -Android
```

出力に以下が加わる:
- `installer\dist\Aurora_0.1.0-arm64.apk` (android / `-Android` 指定時のみ)

個別に作る場合は以下の各節を参照 (出力先は同じ `installer\dist\`)。

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
# tauri CLI は --manifest-path 非対応。src-tauri 直下で実行する
pushd crates/client/src-tauri; cargo tauri build; popd
```

Tauri の生出力は `target\release\bundle\nsis\Aurora_0.1.0_x64-setup.exe`
(初回は makensis / WebView2 ブートストラッパを自動取得)。`build-all.ps1` は
これを `installer\dist\` へコピーする。

アンインストール: 「アプリと機能」または同梱アンインストーラから削除。
ユーザー設定 (`%APPDATA%` の `recent_hosts.json` 等) は手動削除。

---

## host (NSIS)

`installer/host/aurora-host.nsi` がインストーラ定義。client と同じ NSIS で、
**per-machine インストール (起動時に UAC 1 回)** + Apps & features 登録の
クリーンアンインストーラを備える。per-machine にしているのは、自動起動タスクが
`/RL HIGHEST` (昇格) で exe を毎ログオン実行するため。ユーザー権限で書き換えられる
場所に置くと、同一ユーザーの非昇格コードが exe を差し替えるだけで UAC 無しに管理者
権限を得られてしまう。自動起動タスクの登録/解除は host 自身の
`--install` / `--uninstall` に委譲する。

前提ツール: makensis。**client を一度 `cargo tauri build` していれば** Tauri が
取得した NSIS (`%LOCALAPPDATA%\tauri\NSIS`) を自動再利用するので追加導入は不要。
無い場合のみ `winget install NSIS.NSIS`。

ビルド (バージョンは root Cargo.toml から自動取得):

```powershell
.\installer\host\build.ps1
```

出力: `installer\dist\aurora-host-setup-0.1.0.exe`

インストーラの挙動:
- `aurora-host.exe` を `%ProgramFiles%\Aurora Host\` に配置 (per-machine・インストーラ起動時に UAC 1 回)。
- コンポーネント「ログオン時に自動起動」(既定 ON) → `aurora-host --install` で
  ONLOGON / RL HIGHEST のスケジュールタスク "Aurora Host" を、**インストーラを昇格させた
  ユーザー**に登録する (標準ユーザーが別の管理者アカウントで昇格するとそちらに登録される)。
- コンポーネント「ファイル転送機能を有効化」(既定 OFF) → `Client-ProjFS` を有効化
  (再起動が必要な場合あり)。不要なら OFF のまま。
- 自己更新は昇格して動く host (= 自動起動タスク経由) でのみ行える。スタートメニューから
  非昇格で起動した host は更新要求をログに残して中止する。

**接続パスワード**はインストーラでは設定しない。インストール後にトレイアイコン
右クリック →「パスワード変更」、または端末から `aurora-host --set-password` を
一度実行する (未設定だと起動ごとにランダム生成される)。

### クリーンアンインストール

「アプリと機能」または同梱アンインストーラから削除。痕跡を残さない:
1. 常駐プロセス (`aurora-host.exe`) を停止。
2. 自動起動を入れていた場合のみ `aurora-host --uninstall` でタスク "Aurora Host"
   を解除。
3. exe / インストール先フォルダ / スタートメニューのショートカットを削除。
4. `%LOCALAPPDATA%\Aurora\` (port / password.bin) を削除。
5. HKLM のアンインストール登録を削除。

ProjFS (`Client-ProjFS`) は他用途でも使う共有システム機能のため、
アンインストールでは**無効化しない**。手動で戻す場合:

```powershell
Disable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
```

---

## wol (raspi 向け静的バイナリ)

Windows インストーラは作らない。`aurora-wol-<ver>-aarch64` は musl 静的リンクの
単一 Linux バイナリで、インストーラ相当の処理 (`install` / `uninstall` サブコマンド)
を内蔵している。raspi へ 1 ファイル持ち込んで:

```bash
sudo ./aurora-wol-0.1.0-aarch64 install
```

するだけで systemd 常駐化まで完結する (ファイル名は任意。install が
`/usr/local/bin/aurora-wol` へ配置する)。詳細は `crates/wol/README.md` の方式 B を参照。
開発機から直接入れる場合は `crates\wol\deploy-aurora-wol.ps1` (方式 A) のほうが早い。

前提ツール: cargo-zigbuild + zig (`cargo install cargo-zigbuild` /
`winget install zig.zig`。`deploy-aurora-wol.ps1` を一度使っていれば導入済み)。

---

## android (APK)

Tauri Android アプリを APK としてビルドし、`installer\dist\` へ署名済みで出力する。

前提ツール: Android SDK / NDK、JBR21、rustup target `aarch64-linux-android`、
Windows 開発者モード。導入手順は `docs\mobile.md` を参照。

署名用の環境変数 (4 つとも必須):
- `AURORA_ANDROID_KEYSTORE` (keystore ファイルのパス)
- `AURORA_ANDROID_KEYSTORE_PASSWORD`
- `AURORA_ANDROID_KEY_ALIAS`
- `AURORA_ANDROID_KEY_PASSWORD`

CI 版との違い: ローカルビルド (`-Android`) は実機で使われる arm64-v8a 単体、
CI (GitHub Release) は universal 4 ABI。**配布する APK は CI 版を使うこと**。

keystore ファイルはリポジトリ外に置き、絶対にコミットしないこと。
