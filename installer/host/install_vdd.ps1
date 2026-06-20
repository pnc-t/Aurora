# VDD (MttVDD) installer step, run ELEVATED by the NSIS installer (SecVDD).
# Replaces the old fragile cmd "&"-chain with an observable, verifiable script.
# Files (MttVDD.dll/.inf/.cat, nefconw.exe, vdd_settings.xml, this script) are
# extracted side-by-side by NSIS into $PSScriptRoot.
# ASCII only (launched via powershell.exe = PS5.1, which decodes BOM-less files
# as ANSI -- Japanese would mojibake and break parsing).
$ErrorActionPreference = 'Continue'
$src = $PSScriptRoot
$dst = 'C:\VirtualDisplayDriver'
$log = Join-Path $dst 'install_log.txt'

New-Item -ItemType Directory -Force $dst | Out-Null
function Log($m) { $line = "$(Get-Date -Format o)  $m"; Write-Host $line; Add-Content -Path $log -Value $line }
Log "=== Aurora VDD install start (src=$src) ==="

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
foreach ($f in @('MttVDD.dll','MttVDD.inf','mttvdd.cat','nefconw.exe','vdd_settings.xml')) {
    $s = Join-Path $src $f
    if (Test-Path $s) { Copy-Item $s $dst -Force; Log "copied $f" }
    else { Log "MISSING $f (cannot continue)"; }
}
if (-not (Test-Path (Join-Path $dst 'MttVDD.inf'))) { Log "FATAL: MttVDD.inf not staged"; exit 1 }

# 1b) Pin the VDD to the host's real GPU. With gpu=default, IddCx can place the
# virtual display on a render-only adapter whose output is NOT exposed through DXGI
# EnumOutputs, so the host cannot Desktop-Duplicate it (capture hangs / black screen,
# and modes degrade). Binding it to the physical GPU's model name makes the virtual
# output enumerate on that adapter (verified: appears as adapter[0] output[N], full
# mode list). The driver matches <friendlyname> as a substring of the DXGI adapter
# description, so we strip the vendor prefix (AMD's DXGI desc has a double space that a
# WMI single-spaced full name would NOT substring-match; the model portion does).
$xmlPath = Join-Path $dst 'vdd_settings.xml'
try {
    $gpu = Get-CimInstance Win32_VideoController -ErrorAction Stop |
        Where-Object { $_.Name -and $_.Name -notmatch 'Basic|Virtual|Idd|Meta|Parsec|Microsoft|Remote|VDD' } |
        Sort-Object -Property AdapterRAM -Descending | Select-Object -First 1
    if ($gpu -and (Test-Path $xmlPath)) {
        $name = ($gpu.Name -replace '^(AMD|NVIDIA|Intel(\(R\))?|Advanced Micro Devices(, Inc\.)?)\s+', '').Trim()
        if ($name) {
            $xml = Get-Content $xmlPath -Raw
            $xml = $xml -replace '<friendlyname>.*?</friendlyname>', "<friendlyname>$name</friendlyname>"
            # BOM-less UTF-8: the driver/PS5.1 read this file as ANSI; a BOM would corrupt it.
            [System.IO.File]::WriteAllText($xmlPath, $xml, (New-Object System.Text.UTF8Encoding($false)))
            Log "pinned VDD GPU friendlyname='$name' (from '$($gpu.Name)')"
        } else { Log "GPU name empty after strip ('$($gpu.Name)'); leaving gpu=default" }
    } else { Log "no physical GPU found via WMI; leaving gpu=default" }
} catch { Log "GPU pin failed: $($_.Exception.Message); leaving gpu=default" }

# 2) Remove any pre-existing MttVDD nodes so we don't pile up duplicates.
$old = @(Get-PnpDevice | Where-Object { $_.HardwareID -match 'MttVDD' -or $_.HardwareID -match 'MTT1337' })
foreach ($d in $old) { Log "removing stale node $($d.InstanceId)"; & $pnputil /remove-device $d.InstanceId /subtree | Out-Null }

# 3) Create the Root\MttVDD device node (pnputil /add-driver alone does NOT create it).
Log "creating device node Root\MttVDD"
& (Join-Path $dst 'nefconw.exe') --create-device-node --hardware-id Root\MttVDD --class-name Display --class-guid 4d36e968-e325-11ce-bfc1-08002be10318
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
Log "=== Aurora VDD install end ==="
