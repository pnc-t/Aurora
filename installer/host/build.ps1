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

# VDD ドライバファイルを installer/host/vdd/ にコピーする (NSIS が File 命令で同梱)。
# VDD_SOURCE は環境変数で上書き可能。未指定時はユーザーの既知パスを探す。
$vddSource = if ($env:VDD_SOURCE) { $env:VDD_SOURCE } else {
    # よくある配置場所を順に探す。
    @(
        "$env:USERPROFILE\vdd\VirtualDisplayDriver",
        'C:\VirtualDisplayDriver',
        "$env:LOCALAPPDATA\VirtualDisplayDriver"
    ) | Where-Object { Test-Path (Join-Path $_ 'MttVDD.inf') } | Select-Object -First 1
}
if (-not $vddSource) {
    Write-Warning "VDD ドライバが見つかりません。SecVDD セクションはスキップしてください。`n  VDD_SOURCE 環境変数でパスを指定できます (例: MttVDD.inf が入ったフォルダ)"
} else {
    Write-Host "==> VDD ファイルをコピー中 ($vddSource)" -ForegroundColor Cyan
    $vddDest = Join-Path $here 'vdd'
    New-Item -ItemType Directory -Force $vddDest | Out-Null
    # ドライバ本体のみ上流からコピーする。vdd_settings.xml は**上流の既定 5 モード版を使わず**、
    # Aurora 同梱のリッチ版 (案A) で置き換える (下記)。上流 XML を取り込むと黒帯が残る。
    foreach ($f in @('MttVDD.dll', 'MttVDD.inf', 'mttvdd.cat')) {
        $src = Join-Path $vddSource $f
        if (Test-Path $src) {
            Copy-Item $src $vddDest -Force
        } else {
            Write-Warning "  見つかりません: $src"
        }
    }

    # 案A: リッチな vdd_settings.xml (主要アスペクト比 + 一般的解像度を網羅) を同梱する。
    # 反映はインストール時の管理者デバイス登録で行われる (新規ノード作成時に読まれる)。
    # 上流 XML を使うと 5 モードのみで 1920x1200 すら拒否され黒帯が残るため、必ず置き換える。
    $richXml = Join-Path $here 'vdd_settings.rich.xml'
    if (-not (Test-Path $richXml)) { throw "リッチ XML が見つかりません: $richXml" }
    Copy-Item $richXml (Join-Path $vddDest 'vdd_settings.xml') -Force
    Write-Host "==> リッチ vdd_settings.xml を同梱 ($richXml)" -ForegroundColor Cyan

    # nefconw.exe を同梱する。MttVDD は Root\MttVDD の root 列挙デバイスで、pnputil /add-driver
    # だけではデバイスノードが作られない (= 仮想ディスプレイ 0 個)。インストーラ (SecVDD) が
    # nefconw でノードを作るため、ここで取得しておく。NEFCON_SOURCE (nefconw.exe があるフォルダ)
    # で上書き可、無ければ nefarius/nefcon の固定リリースから x64 を取得する。
    $nefconDest = Join-Path $vddDest 'nefconw.exe'
    if (Test-Path $nefconDest) {
        Write-Host "==> nefconw.exe は既に存在 (再取得しない)" -ForegroundColor Cyan
    }
    elseif ($env:NEFCON_SOURCE -and (Test-Path (Join-Path $env:NEFCON_SOURCE 'nefconw.exe'))) {
        Copy-Item (Join-Path $env:NEFCON_SOURCE 'nefconw.exe') $nefconDest -Force
        Write-Host "==> nefconw.exe をコピー ($env:NEFCON_SOURCE)" -ForegroundColor Cyan
    }
    else {
        $nefconVer = 'v1.17.40'
        $nefconUrl = "https://github.com/nefarius/nefcon/releases/download/$nefconVer/nefcon_$nefconVer.zip"
        Write-Host "==> nefconw.exe を取得中 ($nefconUrl)" -ForegroundColor Cyan
        $nefTmp = Join-Path $env:TEMP "nefcon_$nefconVer"
        New-Item -ItemType Directory -Force $nefTmp | Out-Null
        $nefZip = Join-Path $nefTmp 'nefcon.zip'
        try {
            Invoke-WebRequest -Uri $nefconUrl -OutFile $nefZip -Headers @{ 'User-Agent' = 'Aurora-Installer' }
            Expand-Archive -Path $nefZip -DestinationPath $nefTmp -Force
            Copy-Item (Join-Path $nefTmp 'x64\nefconw.exe') $nefconDest -Force
        } finally {
            Remove-Item $nefTmp -Recurse -Force -ErrorAction SilentlyContinue
        }
        if (-not (Test-Path $nefconDest)) { throw "nefconw.exe の取得に失敗しました" }
    }
}

Write-Host "==> NSIS でコンパイル ($makensis)" -ForegroundColor Cyan
& $makensis "/DAPPVERSION=$version" (Join-Path $here 'aurora-host.nsi')
if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE)" }

Write-Host '==> 完了。出力:' -ForegroundColor Green
Get-ChildItem (Join-Path $here '..\dist') -Filter 'aurora-host-*.exe' | ForEach-Object { Write-Host "    $($_.FullName)" }
