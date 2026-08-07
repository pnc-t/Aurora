# モバイル (Android) クライアント

デスクトップ版クライアントを Tauri v2 の Android ターゲットとして同梱ビルドしたものです。
本項ではモバイル版の**入手・インストール**と**ビルド**のみを扱います。パッド操作・
キーボード切り替え・ジェスチャ等の UI 操作説明はここには書きません (アプリ内の表示に譲ります)。

## 対応状況

- **Android**: 7.0 (API 24, minSdk) 以上に対応
- **iOS**: 未対応です。ビルドに macOS + Xcode が必須のため未着手です

---

## 使う人向け: 入手とインストール

### 1. APK の入手

[Releases](https://github.com/pnc-t/Aurora/releases/latest) から `Aurora_<ver>.apk` を
Android 端末で直接ダウンロードします。この APK は universal ビルドで、`arm64-v8a` /
`armeabi-v7a` / `x86` / `x86_64` の 4 アーキテクチャを 1 ファイルに同梱しています。
端末の CPU に応じて別ファイルを選ぶ必要はありません。

### 2. インストール

Play ストア外配布のため、初回は「提供元不明のアプリのインストール」を許可する必要があります。
ダウンロードした APK をタップすると端末側でこの許可を求めるダイアログが出るので、そこから
許可してインストールしてください。

> ⚠️ Play ストア外配布のため、インストール時に Play Protect の警告が出ることがあります。
> これはデスクトップ版インストーラが SmartScreen に警告されるのと同じ位置づけで、コード署名が
> 一般的な配布経路 (ストア/CA署名) を通っていないために出る警告です。詳細は
> [security.md](security.md) を参照してください。

### 3. Tailscale への参加

Android 版 [Tailscale アプリ](https://tailscale.com/) を入れて、ホスト PC と同じ Tailnet に
参加させます。ホスト PC 側で `tailscale ip -4` を実行し、`100.x.x.x` の IP を控えておきます。

### 4. 接続

ランチャーの ＋ からホストの `100.x.x.x` / ポート (既定 7000) / パスワードを登録して接続します。
この手順はルート [README](../README.md) の「はじめかた」と同じです。

### 自動アップデートは非対応

デスクトップ版は `tauri-plugin-updater` が新版を検知してワンクリック更新しますが、Android は
この仕組みの対象外です。新版が出た場合は Releases から APK を取り直し、上書きインストールして
ください。

### バックアップ対象外

アプリのデータは Android のバックアップ / 端末間転送の対象外にしてあります。接続先の
Tailscale IP や WoL の MAC アドレスが平文でクラウドへ同期されるのを避けるためです。

結果として、機種変更や再インストールではホスト一覧・WoL 設定・アプリ設定は引き継がれず、
登録し直しになります。ただしパスワードと端末ID鍵はもともと Android Keystore で端末に紐づいて
おり、バックアップがあっても復元できません (＝引き継ぎで失われる実利は元から小さいです)。

### モバイルで使えない機能

以下はデスクトップ版のみの機能で、モバイル版では非対応です。

| 機能 | 状態 |
|------|------|
| ファイル転送 | 非対応 |
| 統計 HUD | 非対応 |
| 解像度切替 | 非対応 |
| マルチタブ接続 | 非対応 (1 接続のみ) |
| クリップボード同期 | host → 端末の一方向のみ (端末 → host は非対応) |

---

## ビルドする人向け

### 前提環境

| 項目 | 内容 |
|------|------|
| Android SDK | `cargo tauri android init` が要求するバージョンに追従 |
| NDK | **27.2.12479018** 固定 |
| `JAVA_HOME` | Android Studio 同梱の JBR21 (`%LOCALAPPDATA%\Programs\Android Studio\jbr`)。**既定の JDK 25 は Gradle が未対応**でビルドに失敗します |
| Rust ターゲット | `aarch64-linux-android` / `armv7-linux-androideabi` / `i686-linux-android` / `x86_64-linux-android` の 4 種 |
| Windows 開発者モード | **有効化が必須**。無効だとシンボリックリンク制限で `cargo tauri android build` が jniLibs のリンク段で失敗します |

```powershell
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

### ビルドコマンド

tauri CLI は `--manifest-path` 非対応のため `crates/client/src-tauri` 直下で実行します。

```powershell
# 初回のみ (gen/android が無いとき)
cargo tauri android init

# release ビルド (署名は別途。下記「リリース署名」参照)
cargo tauri android build --apk

# debug ビルド (実機での動作確認用)
cargo tauri android build --apk --debug

# 署名済み APK を installer\dist\ へ出力する (署名設定は下記「リリース署名」)
.\installer\build-all.ps1 -Android
```

`build-all.ps1 -Android` の出力は `installer\dist\Aurora_<ver>-arm64.apk` です。
ローカルビルドは実機で使われる arm64-v8a 単体、CI (GitHub Release) は universal
4 ABI という違いがあります。

出力先は `gen/android/app/build/outputs/apk/` 配下です。

```powershell
adb install -r <apk のパス>
```

で実機に投入できます。

### デバッグ

実機ログは `adb logcat` で確認します。加えてクライアントのファイルログは、Android では
`common::paths::data_dir()` が使えないため `setup()` フック内で Tauri の `app_log_dir()` を
使って初期化しており、その配下に出力されます。

### 静的解析の注意

`cargo clippy --workspace` はデスクトップ向け cfg しか通らず、モバイル分岐 (`cfg(target_os =
"android")`) はチェックされません。モバイル分岐の静的解析は別途以下を実行してください。

```powershell
cargo clippy -p aurora-client --target aarch64-linux-android -- -D warnings
```

### リリース署名

keystore の作成例:

```powershell
keytool -genkeypair -v -keystore aurora-release.keystore -alias aurora -keyalg RSA -keysize 2048 -validity 10000
```

ローカルで `installer\build-all.ps1 -Android` を使う場合は、keystore ファイルへの
パスを直接指す以下 4 つの環境変数を設定してください。

| 環境変数 | 内容 |
|------|------|
| `AURORA_ANDROID_KEYSTORE` | keystore ファイルのパス |
| `AURORA_ANDROID_KEYSTORE_PASSWORD` | keystore のパスワード |
| `AURORA_ANDROID_KEY_ALIAS` | キーのエイリアス |
| `AURORA_ANDROID_KEY_PASSWORD` | キーのパスワード |

CI (release ワークフロー) は同じ keystore を、ファイルではなく base64 化した文字列として
以下 4 つの GitHub Secrets から復元して署名します (ランナーにファイルを直接置けないため)。

| Secret | 内容 |
|------|------|
| `ANDROID_KEYSTORE_BASE64` | keystore ファイルを base64 化した文字列 |
| `ANDROID_KEYSTORE_PASSWORD` | keystore のパスワード |
| `ANDROID_KEY_ALIAS` | キーのエイリアス |
| `ANDROID_KEY_PASSWORD` | キーのパスワード |

> この署名鍵は host/client の自動アップデートで使っている minisign 鍵とは**別物**です。
> minisign 鍵は更新配信物の真正性検証 (client/host が自身で検証) が目的ですが、こちらは
> Android アプリパッケージの署名 (Android OS / Play Protect が検証) が目的で、用途も検証者も
> 異なります。

keystore ファイル自体は**絶対にコミットしないでください** (`.gitignore` の `*.key` 等と同じ
精神です)。

### `gen/android` の管理方針

`gen/android` は `cargo tauri android init` で再生成される Tauri のテンプレート生成物のため、
原則 git 管理外です。ただし以下の 6 ファイルだけは `.gitignore` を階層ごとに `!` で開けて
追跡しています (再生成のたびに手編集内容やビルド設定が失われるのを防ぐため)。

| ファイル | 追跡理由 |
|------|------|
| `app/src/main/res/values/themes.xml`<br>`app/src/main/res/values-night/themes.xml` | システムバー配色の手編集。再生成で消えるとライトモード端末で時計が黒文字になり暗い UI に埋もれる (実機 SM-X230 で確認) |
| `app/src/main/java/com/aurora/remote/MainActivity.kt` | ホスト → 端末のクリップボード書き込み用 JNI エントリポイント (`setClipboardText`) とシステムバー配色の手編集。`generated/` 配下の自動生成ファイルとは別物 |
| `app/proguard-aurora.pro` | release ビルドの ProGuard keep ルール。Rust 側がクリップボード書き込みを JNI で `MainActivity` / `setClipboardText` と**名前で**解決するため、難読化されると無言で壊れる。ファイル冒頭のコメントに理由を書いてある |
| `app/src/main/AndroidManifest.xml` | `allowBackup="false"` / `dataExtractionRules` の指定。再生成で消えるとアプリデータが黙って OS のバックアップ対象に戻り、`recent_hosts.json` の Tailscale IP や `wol_targets.json` の MAC アドレスが平文でクラウドへ同期される |
| `app/src/main/res/xml/data_extraction_rules.xml` | 上記の実体 (API 31+)。クラウドバックアップと端末間転送の両方を除外する |

---

## 関連ドキュメント

- [build.md](build.md) — デスクトップ版のビルド・自動アップデート
- [troubleshooting.md](troubleshooting.md) — 接続できない・映像が来ない等の切り分け
- [security.md](security.md) — 暗号化・認証・既知の制約
