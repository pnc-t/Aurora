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
- `installer\dist\Aurora_1.0.0_x64-setup.exe` (client)
- `installer\dist\aurora-host-setup-1.0.0.exe` (host)
- `installer\dist\aurora-wol-1.0.0-aarch64` (wol / raspi 用)

Android は既定ではビルドしない (前提が重いため opt-in)。含める場合:

```powershell
.\installer\build-all.ps1 -Android
```

出力に以下が加わる:
- `installer\dist\Aurora_1.0.0-arm64.apk` (android / `-Android` 指定時のみ)

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

Tauri の生出力は `target\release\bundle\nsis\Aurora_1.0.0_x64-setup.exe`
(初回は makensis / WebView2 ブートストラッパを自動取得)。`build-all.ps1` は
これを `installer\dist\` へコピーする。

アンインストール: 「アプリと機能」または同梱アンインストーラから削除。
ユーザー設定 (`%APPDATA%` の `recent_hosts.json` 等) は手動削除。

---

## host (NSIS)

`installer/host/aurora-host.nsi` がインストーラ定義。client と同じ NSIS で、
**per-machine インストール (起動時に UAC 1 回)** + Apps & features 登録の
クリーンアンインストーラを備える。per-machine にしているのは、SYSTEM サービスが
この exe を実行するため。ユーザー権限で書き換えられる場所に置くと、同一ユーザーの
非昇格コードが exe を差し替えるだけで SYSTEM 権限を得られてしまう。サービスの
登録/解除は host 自身の `--install-service` / `--uninstall-service` に委譲する。

前提ツール: makensis。**client を一度 `cargo tauri build` していれば** Tauri が
取得した NSIS (`%LOCALAPPDATA%\tauri\NSIS`) を自動再利用するので追加導入は不要。
無い場合のみ `winget install NSIS.NSIS`。

ビルド (バージョンは root Cargo.toml から自動取得):

```powershell
.\installer\host\build.ps1
```

出力: `installer\dist\aurora-host-setup-1.0.0.exe`

インストーラの挙動:
- `aurora-host.exe` を `%ProgramFiles%\Aurora Host\` に配置 (per-machine・インストーラ起動時に UAC 1 回)。
  インストール先は変更できるが、中身のある既存フォルダは選べない (前回のインストール先を除く)。
  配置先の所有者を Administrators に、ACL を SYSTEM / Administrators は変更可・Users は読み取り
  のみに固める (SYSTEM サービスが実行する exe とその DLL を一般ユーザーに差し替えさせないため)。
- コンポーネント「サービスとして常駐」(必須。サイレント更新でも毎回) → (旧版からの更新時のみ) `aurora-host --migrate-user-state`
  (旧版の設定/パスワードを **インストーラを昇格させたユーザー** の `%LOCALAPPDATA%\Aurora`
  から `%ProgramData%\Aurora\host` へ移す) → `aurora-host --install-service` で SYSTEM
  サービス "AuroraHost" を登録・開始 (旧 ONLOGON タスクは削除、受信許可のファイアウォール規則
  "Aurora Host" を exe 単位で作成。SYSTEM で動く本体には Windows の許可確認が出ず黙って遮断される
  ため)。exe を上書きする前に
  サービスを止める (`net stop AuroraHost`)。必須にしているのは、状態 (`%ProgramData%\Aurora\host`)
  が SYSTEM / 管理者専用で、非昇格で手動起動した host からは読み書きできないため。移行・登録の
  失敗は終了コードで検知し、対話インストールではメッセージで知らせる (登録に失敗したら
  `Service` フラグを書かず、従来どおり本体を直接起動する)。
- コンポーネント「ファイル転送機能を有効化」(既定 OFF) → `Client-ProjFS` を有効化
  (再起動が必要な場合あり)。不要なら OFF のまま。
- 自己更新は昇格して動く host (= サービス経由、または旧 ONLOGON タスク経由) でのみ行える。
  旧 ONLOGON 運用からのサイレント更新はサービスへ自動移行する。

**接続パスワード**はインストーラでは設定しない。インストール後にトレイアイコン
右クリック →「パスワードを変更」、または管理者の端末から `aurora-host --set-password` を
一度実行する (未設定だと起動ごとにランダム生成される)。

### クリーンアンインストール

「アプリと機能」または同梱アンインストーラから削除。痕跡を残さない:
1. `aurora-host --uninstall-service` でサービス "AuroraHost" を停止・削除 (本体とトレイも止まる)。
   ファイアウォール規則 "Aurora Host" も削除する。
2. 残った常駐プロセス (`aurora-host.exe`) を停止。
3. 旧版で自動起動を入れていた場合のみ `aurora-host --uninstall` でタスク "Aurora Host"
   を解除。
4. exe / インストール先フォルダ / スタートメニューのショートカットを削除。
5. `%ProgramData%\Aurora\host\` (設定・password.bin・ログ) を削除 (`%ProgramData%\Aurora` は VDD と
   共用なので空のときだけ消す)。`%LOCALAPPDATA%\Aurora\` は client と共用なので、host のファイル
   (旧版の設定・トレイのログ) だけを消し、フォルダは空のときだけ消す。
6. HKLM のアンインストール登録を削除。

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
sudo ./aurora-wol-1.0.0-aarch64 install
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
