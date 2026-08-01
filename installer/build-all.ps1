# Aurora 配布インストーラ 一括ビルド
#
# client (Tauri NSIS) と host (NSIS) の両インストーラ、および
# wol (raspi 向け静的バイナリ) を生成し、すべて installer\dist\ に集約する。
#
# 出力:
#   installer\dist\Aurora_<ver>_x64-setup.exe      (client)
#   installer\dist\aurora-host-setup-<ver>.exe     (host)
#   installer\dist\aurora-wol-<ver>-aarch64        (wol / raspi。インストーラ不要、
#                                                   Pi 上で `sudo ./... install` するだけ)
#   installer\dist\Aurora_<ver>-arm64.apk           (android。-Android 指定時のみ)
#
# 前提: makensis (host 用)。client を一度 `cargo tauri build` していれば Tauri が取得した
#       NSIS (%LOCALAPPDATA%\tauri\NSIS) を自動再利用するので追加導入は不要。無ければ
#       `winget install NSIS.NSIS`。
#       wol 用に cargo-zigbuild + zig (未導入なら crates\wol\deploy-aurora-wol.ps1 が
#       自動導入する。手動なら `cargo install cargo-zigbuild` / `winget install zig.zig`)。
#       client の更新配信物 (.sig) 署名に minisign 秘密鍵 (環境変数 TAURI_SIGNING_PRIVATE_KEY)。
#       署名せず動作確認だけしたいときは -NoSign。
#       `-Android` 指定時のみ Android ツールチェーン (NDK / JBR21 / rustup target
#       aarch64-linux-android / Windows 開発者モード。詳細は docs\mobile.md) と、
#       署名用の AURORA_ANDROID_* 環境変数 4 つが必要。

param(
    # 署名なしでビルドする (client の updater artifacts を作らない)。
    # インストーラの動作確認専用。生成物は自己更新に使えないので配布しないこと。
    [switch]$NoSign,

    # Android APK もビルドする (既定 OFF)。NDK / JBR21 / rust target 4種 / Windows
    # 開発者モードが前提で他3成果物より重いため、環境が整っている場合だけ opt-in する
    # (未整備のマシンでも build-all.ps1 が従来どおり通るようにする)。
    [switch]$Android
)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$repo = Resolve-Path (Join-Path $here '..')
$dist = Join-Path $here 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

# バージョンは root Cargo.toml の [workspace.package] を単一ソースにする (host\build.ps1 と同じ)。
# wol と android の両方が使うため、ここで一度だけ抽出する。
$cargoToml = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
    throw "Cargo.toml から workspace version を取得できませんでした"
}
$version = $Matches[1]

# tauri.conf.json は createUpdaterArtifacts=true なので、鍵が無いと client ビルドは
# .sig 生成の段で失敗する。それが分かるのは 2 分超のコンパイルを終えた後なので、
# ここで先に落として待ち時間を無駄にしない。
if (-not $NoSign -and -not $env:TAURI_SIGNING_PRIVATE_KEY) {
    throw @'
署名鍵 (TAURI_SIGNING_PRIVATE_KEY) が未設定です。

  $env:TAURI_SIGNING_PRIVATE_KEY = "$HOME\.tauri\aurora-updater.key"
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "<鍵のパスワード>"

インストーラの動作確認だけで配布しないなら -NoSign を付けてください
(署名なし = client の自己更新に使えない成果物になります)。
'@
}

# Android も同じ理由で先に検査する: APK 署名鍵が無いと後段の apksigner で初めて失敗し、
# それまでの cargo tauri android build (数分) が無駄になる。
# -NoSign は Tauri updater artifacts (.sig) 専用のスイッチであり APK 署名には無関係
# (未署名 APK は端末にインストールできず「動作確認用」にすらならないため、
# APK の署名は -NoSign 指定時も含め常に必須)。
if ($Android) {
    $missing = @(
        'AURORA_ANDROID_KEYSTORE',
        'AURORA_ANDROID_KEYSTORE_PASSWORD',
        'AURORA_ANDROID_KEY_ALIAS',
        'AURORA_ANDROID_KEY_PASSWORD'
    ) | Where-Object { -not (Get-Item -Path "env:$_" -ErrorAction SilentlyContinue).Value }
    if ($missing -or -not (Test-Path $env:AURORA_ANDROID_KEYSTORE)) {
        throw @'
Android APK の署名設定が未設定です (-Android 指定時に必要)。

  $env:AURORA_ANDROID_KEYSTORE = "$HOME\.android\aurora-release.keystore"
  $env:AURORA_ANDROID_KEYSTORE_PASSWORD = "<keystore のパスワード>"
  $env:AURORA_ANDROID_KEY_ALIAS = "aurora"
  $env:AURORA_ANDROID_KEY_PASSWORD = "<キーのパスワード>"

keystore の作り方は docs\mobile.md の「リリース署名」を参照してください。
'@
    }

    # build-tools のパスは署名節でも使うのでスクリプトスコープに保持し、二重解決しない。
    $sdkRoot = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { $env:ANDROID_SDK_ROOT }
    # 文字列順ソートだと "9.0.0" が "34.0.0" より新しいと判定されるため [version] で比較する
    # (CI の `sort -V` と同じ意味)。`-as [version]` で弾いているのは "24.0.0-preview" のような
    # バージョンとして解釈できないディレクトリ名で、これをキャストするとソート自体が落ちるため。
    $buildTools = if ($sdkRoot) {
        Get-ChildItem (Join-Path $sdkRoot 'build-tools') -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -as [version] } |
            Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1 -ExpandProperty FullName
    }
    else { $null }
    if (-not $buildTools -or -not (Test-Path (Join-Path $buildTools 'zipalign.exe')) -or -not (Test-Path (Join-Path $buildTools 'apksigner.bat'))) {
        throw 'Android SDK build-tools (zipalign / apksigner) が見つかりません。$env:ANDROID_HOME または $env:ANDROID_SDK_ROOT を確認してください。'
    }
}

# --- client (Tauri NSIS) -------------------------------------------------
Write-Host '==> client インストーラをビルド (cargo tauri build)' -ForegroundColor Cyan
Push-Location (Join-Path $repo 'crates\client\src-tauri')
try {
    if ($NoSign) {
        # 署名鍵が要るのは updater artifacts (.sig) の生成だけなので、そこだけ止める。
        cargo tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
    }
    else {
        cargo tauri build
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed ($LASTEXITCODE)" }
}
finally {
    Pop-Location
}

# Tauri の NSIS 出力先は固定。生成物を dist へコピーする。
$nsisDir = Join-Path $repo 'target\release\bundle\nsis'
$clientSetup = Get-ChildItem $nsisDir -Filter '*-setup.exe' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $clientSetup) { throw "client インストーラが見つかりません: $nsisDir" }
Copy-Item $clientSetup.FullName $dist -Force

# --- host (NSIS) -----------------------------------------------------------
# host\build.ps1 は OutputDir=..\dist のため、そのまま installer\dist へ出力される。
Write-Host '==> host インストーラをビルド (build.ps1)' -ForegroundColor Cyan
& (Join-Path $here 'host\build.ps1')
if ($LASTEXITCODE -ne 0) { throw "host build.ps1 failed ($LASTEXITCODE)" }

# --- wol (raspi 向け静的バイナリ) ----------------------------------------
# インストーラは作らない: aurora-wol は install サブコマンド内蔵の単一 musl 静的
# バイナリで、Pi へ 1 ファイル持ち込んで `sudo ./aurora-wol-<ver>-aarch64 install`
# するだけで常駐化まで完結する (crates\wol\README.md 方式 B)。
Write-Host '==> wol バイナリをビルド (cargo zigbuild, aarch64-musl)' -ForegroundColor Cyan
$wolTarget = 'aarch64-unknown-linux-musl'
rustup target add $wolTarget | Out-Null
if ($LASTEXITCODE -ne 0) { throw "rustup target add $wolTarget failed ($LASTEXITCODE)" }
Push-Location $repo
try {
    cargo zigbuild -p aurora-wol --release --target $wolTarget
    if ($LASTEXITCODE -ne 0) { throw "cargo zigbuild failed ($LASTEXITCODE)。cargo-zigbuild/zig が未導入の可能性 (ヘッダの前提を参照)" }
}
finally {
    Pop-Location
}

$wolBin = Join-Path $repo "target\$wolTarget\release\aurora-wol"
if (-not (Test-Path $wolBin)) { throw "wol ビルド成果物が見つかりません: $wolBin" }
Copy-Item $wolBin (Join-Path $dist "aurora-wol-$version-aarch64") -Force

# --- android (APK / arm64-v8a) -------------------------------------------
if ($Android) {
    Write-Host '==> Android APK をビルド (cargo tauri android build)' -ForegroundColor Cyan
    # 実機はほぼ arm64-v8a なのでローカルは 1 ABI に絞る (CI は universal 4 ABI)。
    # tauri CLI は --manifest-path 非対応なので src-tauri 直下で実行する。
    Push-Location (Join-Path $repo 'crates\client\src-tauri')
    try {
        cargo tauri android build --apk --target aarch64
        if ($LASTEXITCODE -ne 0) { throw "cargo tauri android build failed ($LASTEXITCODE)" }
    }
    finally {
        Pop-Location
    }

    # 単一ターゲット指定時の出力ディレクトリ名 (arm64 等) は Tauri/Gradle 側の命名に
    # 依存するので決め打ちせず再帰検索する (既存の client インストーラ探索と同じ流儀)。
    $apkOutDir = Join-Path $repo 'crates\client\src-tauri\gen\android\app\build\outputs\apk'
    $unsigned = Get-ChildItem $apkOutDir -Recurse -Filter '*release-unsigned.apk' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $unsigned) { throw "未署名 APK が見つかりません: $apkOutDir" }

    # gen/android は再生成されるため gradle の signingConfigs に鍵を書いても消える。
    # そのためビルドは unsigned のまま出し、build-tools の zipalign / apksigner で
    # 後署名する (CI と同じ方式)。中間物は temp に置きリポジトリと dist を汚さない。
    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "aurora-apk-$PID")
    try {
        $aligned = Join-Path $tmp 'aligned.apk'
        $signed = Join-Path $tmp 'signed.apk'

        & (Join-Path $buildTools 'zipalign.exe') -p -f 4 $unsigned.FullName $aligned
        if ($LASTEXITCODE -ne 0) { throw "zipalign failed ($LASTEXITCODE)" }

        & (Join-Path $buildTools 'apksigner.bat') sign `
            --ks $env:AURORA_ANDROID_KEYSTORE `
            --ks-pass "pass:$env:AURORA_ANDROID_KEYSTORE_PASSWORD" `
            --ks-key-alias $env:AURORA_ANDROID_KEY_ALIAS `
            --key-pass "pass:$env:AURORA_ANDROID_KEY_PASSWORD" `
            --v1-signing-enabled true --v2-signing-enabled true `
            --out $signed $aligned
        if ($LASTEXITCODE -ne 0) { throw "apksigner sign failed ($LASTEXITCODE)" }

        & (Join-Path $buildTools 'apksigner.bat') verify $signed
        if ($LASTEXITCODE -ne 0) { throw "apksigner verify failed ($LASTEXITCODE)" }

        Copy-Item $signed (Join-Path $dist "Aurora_$version-arm64.apk") -Force
    }
    finally {
        Remove-Item -Recurse -Force $tmp
    }
}

# --- 結果 ---------------------------------------------------------------
Write-Host '==> 完了。installer\dist:' -ForegroundColor Green
Get-ChildItem $dist -File | ForEach-Object { Write-Host "    $($_.Name)" }
if ($NoSign) {
    Write-Host '警告: -NoSign でビルドしました。client の .sig が無いため配布しないでください。' -ForegroundColor Yellow
}
