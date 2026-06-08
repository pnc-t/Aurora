# Aurora Host インストーラ ビルドスクリプト
#
# release ビルド → NSIS (makensis) でインストーラを生成する。
# 出力: installer\dist\aurora-host-setup-<version>.exe
#
# makensis は client (Tauri NSIS) ビルド時に取得済みの Tauri キャッシュを再利用する。
# 無ければ PATH / winget の NSIS を探す (winget install NSIS.NSIS)。

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$repo = Resolve-Path (Join-Path $here '..\..')

# workspace バージョンを単一ソースにする (root Cargo.toml の [workspace.package])。
$cargoToml = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
    throw "Cargo.toml から workspace version を取得できませんでした"
}
$version = $Matches[1]

Write-Host "==> release ビルド (host) v$version" -ForegroundColor Cyan
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

# makensis を探す (Tauri キャッシュ → PATH → 既定インストール先)
$makensis = $null
foreach ($c in @(
    (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe'),
    (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source,
    'C:\Program Files (x86)\NSIS\makensis.exe',
    'C:\Program Files\NSIS\makensis.exe'
)) { if ($c -and (Test-Path $c)) { $makensis = $c; break } }
if (-not $makensis) {
    throw "makensis.exe が見つかりません。client を一度 `cargo tauri build` するか、NSIS を入れてください: winget install NSIS.NSIS"
}

Write-Host "==> NSIS でコンパイル ($makensis)" -ForegroundColor Cyan
& $makensis "/DAPPVERSION=$version" (Join-Path $here 'aurora-host.nsi')
if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE)" }

Write-Host '==> 完了。出力:' -ForegroundColor Green
Get-ChildItem (Join-Path $here '..\dist') -Filter 'aurora-host-*.exe' | ForEach-Object { Write-Host "    $($_.FullName)" }
