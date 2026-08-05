# VDD (MttVDD) installer step, run ELEVATED by the NSIS installer (SecVDD).
# Replaces the old fragile cmd "&"-chain with an observable, verifiable script.
# Files (MttVDD.dll/.inf/.cat, nefconw.exe, vdd_settings.xml, this script) are
# extracted side-by-side by NSIS into $PSScriptRoot.
# ASCII only (launched via powershell.exe = PS5.1, which decodes BOM-less files
# as ANSI -- Japanese would mojibake and break parsing).
#
# -HostExe: full path to aurora-host.exe. After the VDD device is created we invoke it
# with --align-vdd to set friendlyname to the REAL capture GPU (step 6).
param([string]$HostExe = "")

$ErrorActionPreference = 'Continue'
$src = $PSScriptRoot
# Aurora-owned settings/staging folder. We deliberately do NOT use the shared default
# 'C:\VirtualDisplayDriver': keeping Aurora's vdd_settings.xml + install log under our own
# namespace means a coexisting MttVDD (another app) is not mixed in, and uninstall can delete
# this whole folder without clobbering someone else's settings. The MttVDD.dll reads its
# settings folder from HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH (falling back to
# C:\VirtualDisplayDriver only when unset), so we point VDDPATH here (step 1c) and the host's
# VDD_XML_PATH const is set to the same path.
$dst = 'C:\ProgramData\Aurora\vdd'
# reg.exe form (no PS drive colon). We must write this via reg.exe /reg:64, NOT the PS registry
# provider: NSIS is 32-bit so this elevated powershell is 32-bit too, and its HKLM\SOFTWARE writes
# are WOW64-redirected to WOW6432Node -- but the 64-bit MttVDD.dll reads the NATIVE key, so a
# redirected write leaves VDDPATH invisible to the driver and it silently falls back to
# C:\VirtualDisplayDriver. (Same WOW64 trap the pnputil resolution below already dodges.)
$vddRegPath = 'HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver'
$log = Join-Path $dst 'install_log.txt'

New-Item -ItemType Directory -Force $dst | Out-Null
function Log($m) { $line = "$(Get-Date -Format o)  $m"; Write-Host $line; Add-Content -Path $log -Value $line }
Log "=== Aurora VDD install start (src=$src, dst=$dst) ==="

# Resolve the REAL 64-bit pnputil.exe. NSIS is a 32-bit process, so the elevated
# powershell.exe it launches is 32-bit too; in it a bare 'pnputil' resolves under
# System32 which WOW64 redirects to SysWOW64 -- where pnputil.exe does NOT exist.
# The call then silently does nothing ($LASTEXITCODE stays blank, no output), which
# is exactly why earlier installs created the Root\MttVDD node (nefconw was called by
# full path) but never added the driver to the store. 'Sysnative' is the virtual path
# a 32-bit process uses to reach the true System32; on 64-bit PS it does not exist, so
# fall back to System32.
$pnputil = Join-Path $env:SystemRoot 'Sysnative\pnputil.exe'
if (-not (Test-Path $pnputil)) { $pnputil = Join-Path $env:SystemRoot 'System32\pnputil.exe' }
Log "pnputil = $pnputil (PS $($PSVersionTable.PSVersion), 64bit=$([Environment]::Is64BitProcess))"

# 1) Stage driver files + rich settings into C:\VirtualDisplayDriver.
# nefconw.exe は昇格したまま実行するため、標準ユーザーが書き込めない $dst に置いたまま
# 実行する (step 3)。$src ($TEMP\AuroraVDDSetup) は非昇格の NSIS が書いた user-writable な
# フォルダなので、そこから昇格実行してはいけない (aurora-host.nsi の「$TEMP はユーザー権限で
# 書き込み可能」参照)。$dst が安全なのは step 1c で Users:Modify を vdd_settings.xml 単体に
# 絞ったため (ProgramData のサブフォルダは既定で標準ユーザー読み取り専用)。
foreach ($f in @('MttVDD.dll','MttVDD.inf','mttvdd.cat','nefconw.exe','vdd_settings.xml')) {
    $s = Join-Path $src $f
    if (Test-Path $s) { Copy-Item $s $dst -Force; Log "copied $f" }
    else { Log "MISSING $f (cannot continue)"; }
}
if (-not (Test-Path (Join-Path $dst 'MttVDD.inf'))) { Log "FATAL: MttVDD.inf not staged"; exit 1 }

# 1c) Point the MttVDD driver at OUR settings folder and let the (non-elevated) host write to it.
#  - VDDPATH tells MttVDD.dll where vdd_settings.xml lives (see $dst comment). Set it BEFORE the
#    device is created so the driver reads the right folder on first start.
#  - ProgramData subfolders grant standard users read-only by default, but the host appends client
#    resolutions / rewrites <friendlyname> in vdd_settings.xml at runtime WITHOUT elevation, so we
#    grant Users Modify -- but ONLY on vdd_settings.xml itself, not the whole folder with /T.
#    フォルダ全体を広げると nefconw.exe / MttVDD.* まで標準ユーザーが書き換えられるようになり、
#    step 3 で昇格実行するまでの間に差し替えられる (= 管理者権限で任意コード実行)。実行時に
#    非昇格で書かれるのは vdd_settings.xml だけなので、付与もそれ 1 ファイルに限定する。
& reg.exe add $vddRegPath /v VDDPATH /t REG_SZ /d $dst /reg:64 /f 2>&1 | ForEach-Object { Log "reg: $_" }
Log "set (64-bit) $vddRegPath\VDDPATH = $dst (exit=$LASTEXITCODE)"
& icacls (Join-Path $dst 'vdd_settings.xml') /grant '*S-1-5-32-545:(M)' 2>&1 | ForEach-Object { Log "icacls: $_" }
Log "icacls grant Users:Modify on vdd_settings.xml exit=$LASTEXITCODE"

# 1b) friendlyname (VDD render GPU) is intentionally left at the shipped default here.
# Picking it via WMI VRAM-max is WRONG on muxless Optimus laptops: it selects the dGPU
# (e.g. NVIDIA), but the host captures on the iGPU (Intel). The VDD then renders on a
# different GPU than the one duplicating the desktop, and Windows bridges them with a
# cross-adapter keyed-mutex surface that gets abandoned -> AcquireNextFrame returns
# 0x887A0026 forever (virtual desktop freezes to black, never recovers). Only the host's
# actual DuplicateOutput probe knows the true capture GPU, so we defer to it: step 6
# invokes `aurora-host --align-vdd` after the device exists, which writes the correct
# friendlyname and restarts the VDD device so it applies immediately (no reboot).

# 2) Remove any pre-existing MttVDD nodes so we don't pile up duplicates.
$old = @(Get-PnpDevice | Where-Object { $_.HardwareID -match 'MttVDD' -or $_.HardwareID -match 'MTT1337' })
foreach ($d in $old) { Log "removing stale node $($d.InstanceId)"; & $pnputil /remove-device $d.InstanceId /subtree | Out-Null }

# 3) Create the Root\MttVDD device node (pnputil /add-driver alone does NOT create it).
# nefconw.exe は昇格したまま実行するので、実行元は標準ユーザーが書き込めない場所でなければ
# ならない。旧版は step 1c で $dst 全体に Users:Modify (/T) を与えた直後にここを実行しており、
# その間 (step 2 のノード削除で数百ms〜数秒) に標準ユーザーが exe を差し替えれば管理者権限で
# 任意コードを実行できた。1c の付与を vdd_settings.xml 単体に絞ったことで $dst は標準ユーザー
# 読み取り専用のままになり、この窓は閉じている。
$nefconw = Join-Path $dst 'nefconw.exe'
if (-not (Test-Path $nefconw)) { Log "FATAL: nefconw.exe not staged at $nefconw"; exit 1 }
Log "creating device node Root\MttVDD"
& $nefconw --create-device-node --hardware-id Root\MttVDD --class-name Display --class-guid 4d36e968-e325-11ce-bfc1-08002be10318
Log "nefconw exit=$LASTEXITCODE"

# 4) Add the driver to the store and bind it to the present node (/install binds).
& $pnputil /add-driver (Join-Path $dst 'MttVDD.inf') /install 2>&1 | ForEach-Object { Log "pnputil: $_" }
Log "pnputil /add-driver exit=$LASTEXITCODE"

# 5) Verify the MTT1337 monitor showed up (retry: enumeration can lag a moment).
$ok = $false
for ($i = 0; $i -lt 5; $i++) {
    Start-Sleep -Seconds 2
    $mon = Get-PnpDevice | Where-Object { $_.HardwareID -match 'MTT1337' -or $_.FriendlyName -match 'VDD by MTT' }
    if ($mon) { $ok = $true; break }
}
if ($ok) { Log "SUCCESS: MTT1337 monitor present" }
else {
    Log "WARNING: MTT1337 monitor not present yet; trying one more add-driver /install"
    & $pnputil /add-driver (Join-Path $dst 'MttVDD.inf') /install 2>&1 | ForEach-Object { Log "pnputil(retry): $_" }
    Start-Sleep -Seconds 3
    $mon = Get-PnpDevice | Where-Object { $_.HardwareID -match 'MTT1337' -or $_.FriendlyName -match 'VDD by MTT' }
    if ($mon) { Log "SUCCESS after retry: MTT1337 present" } else { Log "STILL MISSING: see this log" }
}
# 6) Align the VDD render GPU to the host's REAL capture GPU (see 1b). The host probes
# DuplicateOutput the same way it does at runtime, writes friendlyname, and restarts the
# VDD device so the fix applies now instead of only on the next reboot. It re-execs to
# reach the iGPU on hybrid laptops, so launch it fire-and-forget (it finishes in a few
# seconds on its own); its result is written to the host log, not this one.
if ($HostExe -and (Test-Path $HostExe)) {
    Log "aligning VDD render GPU to capture GPU via: $HostExe --align-vdd"
    Start-Process -FilePath $HostExe -ArgumentList '--align-vdd' -WindowStyle Hidden
} else {
    Log "host exe not provided ($HostExe); skipping GPU align (host self-heals on next run + reboot)"
}

Log "=== Aurora VDD install end ==="
