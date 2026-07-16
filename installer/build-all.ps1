# Aurora 配布インストーラ 一括ビルド
#
# client (Tauri NSIS) と host (Inno Setup) の両インストーラ、および
# wol (raspi 向け静的バイナリ) を生成し、すべて installer\dist\ に集約する。
#
# 出力:
#   installer\dist\Aurora_<ver>_x64-setup.exe      (client)
#   installer\dist\aurora-host-setup-<ver>.exe     (host)
#   installer\dist\aurora-wol-<ver>-aarch64        (wol / raspi。インストーラ不要、
#                                                   Pi 上で `sudo ./... install` するだけ)
#
# 前提: Inno Setup 6 (host 用)。未導入なら `winget install JRSoftware.InnoSetup`。
#       wol 用に cargo-zigbuild + zig (未導入なら crates\wol\deploy-aurora-wol.ps1 が
#       自動導入する。手動なら `cargo install cargo-zigbuild` / `winget install zig.zig`)。

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$repo = Resolve-Path (Join-Path $here '..')
$dist = Join-Path $here 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

# --- client (Tauri NSIS) -------------------------------------------------
Write-Host '==> client インストーラをビルド (cargo tauri build)' -ForegroundColor Cyan
Push-Location (Join-Path $repo 'crates\client\src-tauri')
try {
    cargo tauri build
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

# --- host (Inno Setup) ---------------------------------------------------
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
