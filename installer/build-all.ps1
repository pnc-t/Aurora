# Aurora 配布インストーラ 一括ビルド
#
# client (Tauri NSIS) と host (Inno Setup) の両インストーラを生成し、
# どちらも installer\dist\ に集約する。
#
# 出力:
#   installer\dist\Aurora_<ver>_x64-setup.exe      (client)
#   installer\dist\aurora-host-setup-<ver>.exe     (host)
#
# 前提: Inno Setup 6 (host 用)。未導入なら `winget install JRSoftware.InnoSetup`。

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

# --- 結果 ---------------------------------------------------------------
Write-Host '==> 完了。installer\dist:' -ForegroundColor Green
Get-ChildItem $dist -Filter '*.exe' | ForEach-Object { Write-Host "    $($_.Name)" }
