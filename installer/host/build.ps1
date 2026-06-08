# Aurora Host インストーラ ビルドスクリプト
#
# release ビルド → Inno Setup (ISCC) でインストーラを生成する。
# 出力: installer\host\dist\aurora-host-setup-<version>.exe
#
# 前提: Inno Setup 6 (ISCC.exe)。未導入なら:
#   winget install JRSoftware.InnoSetup

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$repo = Resolve-Path (Join-Path $here '..\..')

Write-Host '==> release ビルド (host)' -ForegroundColor Cyan
Push-Location $repo
try {
    cargo build -p aurora-host --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
finally {
    Pop-Location
}

$exe = Join-Path $repo 'target\release\aurora-host.exe'
if (-not (Test-Path $exe)) { throw "ビルド成果物が見つかりません: $exe" }

# ISCC を探す (PATH → 既定インストール先)
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    foreach ($c in @(
        'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
        'C:\Program Files\Inno Setup 6\ISCC.exe'
    )) { if (Test-Path $c) { $iscc = $c; break } }
}
if (-not $iscc) {
    throw "ISCC.exe が見つかりません。Inno Setup 6 を入れてください: winget install JRSoftware.InnoSetup"
}

Write-Host "==> Inno Setup でコンパイル ($iscc)" -ForegroundColor Cyan
& $iscc (Join-Path $here 'aurora-host.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

Write-Host '==> 完了。出力:' -ForegroundColor Green
Get-ChildItem (Join-Path $here 'dist') -Filter '*.exe' | ForEach-Object { Write-Host "    $($_.FullName)" }
