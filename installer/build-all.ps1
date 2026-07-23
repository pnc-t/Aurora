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
#
# 前提: makensis (host 用)。client を一度 `cargo tauri build` していれば Tauri が取得した
#       NSIS (%LOCALAPPDATA%\tauri\NSIS) を自動再利用するので追加導入は不要。無ければ
#       `winget install NSIS.NSIS`。
#       wol 用に cargo-zigbuild + zig (未導入なら crates\wol\deploy-aurora-wol.ps1 が
#       自動導入する。手動なら `cargo install cargo-zigbuild` / `winget install zig.zig`)。
#       client の更新配信物 (.sig) 署名に minisign 秘密鍵 (環境変数 TAURI_SIGNING_PRIVATE_KEY)。
#       署名せず動作確認だけしたいときは -NoSign。

param(
    # 署名なしでビルドする (client の updater artifacts を作らない)。
    # インストーラの動作確認専用。生成物は自己更新に使えないので配布しないこと。
    [switch]$NoSign
)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$repo = Resolve-Path (Join-Path $here '..')
$dist = Join-Path $here 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

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

# バージョンは root Cargo.toml の [workspace.package] を単一ソースにする (host\build.ps1 と同じ)。
$cargoToml = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
    throw "Cargo.toml から workspace version を取得できませんでした"
}
$wolVersion = $Matches[1]
$wolBin = Join-Path $repo "target\$wolTarget\release\aurora-wol"
if (-not (Test-Path $wolBin)) { throw "wol ビルド成果物が見つかりません: $wolBin" }
Copy-Item $wolBin (Join-Path $dist "aurora-wol-$wolVersion-aarch64") -Force

# --- 結果 ---------------------------------------------------------------
Write-Host '==> 完了。installer\dist:' -ForegroundColor Green
Get-ChildItem $dist -File | ForEach-Object { Write-Host "    $($_.Name)" }
if ($NoSign) {
    Write-Host '警告: -NoSign でビルドしました。client の .sig が無いため配布しないでください。' -ForegroundColor Yellow
}
