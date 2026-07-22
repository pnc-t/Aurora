# VDD (MttVDD) driver full-removal script.
# Run elevated (admin) during aurora-host uninstall, or manually.
#
# NOTE: ASCII-only on purpose. This is launched via powershell.exe (Windows
# PowerShell 5.1), which decodes BOM-less files as ANSI -- Japanese text would
# be mojibake and break string parsing. Keep this file ASCII.

# Resolve the REAL 64-bit pnputil.exe. NSIS uninstall is 32-bit, so the elevated
# powershell.exe is 32-bit; a bare 'pnputil' there resolves under System32 which WOW64
# redirects to SysWOW64 (no pnputil) and silently no-ops -- that would leave the driver
# package behind. 'Sysnative' reaches the true System32 from a 32-bit process; on 64-bit
# PowerShell it does not exist, so fall back to System32.
$pnputil = Join-Path $env:SystemRoot 'Sysnative\pnputil.exe'
if (-not (Test-Path $pnputil)) { $pnputil = Join-Path $env:SystemRoot 'System32\pnputil.exe' }

# 1. Remove devices (matched by hardware id / friendly name).
#    MTT1337 = virtual monitor (child). Root\MttVDD (HardwareID 'MttVDD') =
#    the parent node created by nefconw. Remove both so no empty node is left.
# Try a couple of passes: removing the parent Root\MttVDD node with /subtree also
# takes its MTT1337 monitor child, but device removal can transiently fail if the
# display was just in use (this is why an uninstall-time run could silently leave
# the device behind). Do NOT suppress errors; retry once after a short wait.
for ($pass = 1; $pass -le 2; $pass++) {
    $devs = @(Get-PnpDevice | Where-Object {
        $_.HardwareID -match 'MTT1337' -or $_.HardwareID -match 'MttVDD' -or $_.FriendlyName -match 'VDD by MTT'
    })
    if ($devs.Count -eq 0) { break }
    foreach ($dev in $devs) {
        Write-Host "Removing device (pass $pass): $($dev.FriendlyName) ($($dev.InstanceId))"
        & $pnputil /remove-device $dev.InstanceId /subtree
        if ($LASTEXITCODE -ne 0) { Write-Host "  pnputil /remove-device exit=$LASTEXITCODE" }
    }
    Start-Sleep -Seconds 2
}
$left = @(Get-PnpDevice | Where-Object {
    $_.HardwareID -match 'MTT1337' -or $_.HardwareID -match 'MttVDD' -or $_.FriendlyName -match 'VDD by MTT'
})
if ($left.Count -gt 0) { Write-Host "WARNING: $($left.Count) VDD device(s) still present after removal attempts" }

# 2. Remove the driver package from the driver store.
#    Get-WindowsDriver is locale-independent and returns OriginalFileName.
try {
    $pkg = Get-WindowsDriver -Online -ErrorAction Stop |
        Where-Object { $_.OriginalFileName -match 'MttVDD' }
    if ($pkg) {
        foreach ($p in $pkg) {
            Write-Host "Removing driver: $($p.Driver) (orig: $($p.OriginalFileName))"
            & $pnputil /delete-driver $p.Driver /uninstall /force
        }
    } else {
        Write-Host "MttVDD driver package not found (already removed)"
    }
} catch {
    # Fallback: search pnputil /enum-drivers directly.
    $lines = & $pnputil /enum-drivers
    $oemName = $null
    foreach ($line in $lines) {
        if ($line -match 'oem\d+\.inf') {
            $oemName = ($line -replace '.*?(oem\d+\.inf).*', '$1').Trim()
        }
        if ($oemName -and $line -match 'mttvdd') {
            Write-Host "Removing driver (fallback): $oemName"
            & $pnputil /delete-driver $oemName /uninstall /force
            $oemName = $null
        }
    }
}

# 3. Remove Aurora's own VDD settings folder (settings + install log + staged files).
#    We only ever installed into this Aurora-namespaced path, so deleting it cannot clobber a
#    coexisting MttVDD that keeps its settings under the shared C:\VirtualDisplayDriver.
$dst = 'C:\ProgramData\Aurora\vdd'
if (Test-Path $dst) {
    Remove-Item -Recurse -Force $dst
    Write-Host "Removed $dst"
}

# 4. Drop VDDPATH only if it still points at OUR folder. Leaving a VDDPATH that references the
#    now-deleted folder would send a future/other MttVDD to a missing settings path; but if the
#    user re-pointed it elsewhere (another app), do not touch it.
#    Use reg.exe /reg:64 for the same reason install_vdd.ps1 does: this runs from the 32-bit NSIS
#    uninstaller, so the PS registry provider would hit WOW6432Node, not the native key the
#    installer wrote (and the driver reads).
$vddRegPath = 'HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver'
$cur = (& reg.exe query $vddRegPath /v VDDPATH /reg:64 2>$null | Select-String 'VDDPATH').ToString()
if ($cur -match 'REG_SZ\s+(.+?)\s*$') {
    $curVal = $Matches[1]
    if ($curVal -eq $dst) {
        & reg.exe delete $vddRegPath /v VDDPATH /reg:64 /f 2>&1 | Out-Null
        Write-Host "Removed VDDPATH (was $curVal)"
    } else {
        Write-Host "VDDPATH points elsewhere ($curVal); left as-is"
    }
} else {
    Write-Host "VDDPATH not set (nothing to remove)"
}

Write-Host "VDD removal complete"
