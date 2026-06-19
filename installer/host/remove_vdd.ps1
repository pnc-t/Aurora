# VDD (MttVDD) driver full-removal script.
# Run elevated (admin) during aurora-host uninstall, or manually.
#
# NOTE: ASCII-only on purpose. This is launched via powershell.exe (Windows
# PowerShell 5.1), which decodes BOM-less files as ANSI -- Japanese text would
# be mojibake and break string parsing. Keep this file ASCII.

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
        & pnputil /remove-device $dev.InstanceId /subtree
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
            & pnputil /delete-driver $p.Driver /uninstall /force
        }
    } else {
        Write-Host "MttVDD driver package not found (already removed)"
    }
} catch {
    # Fallback: search pnputil /enum-drivers directly.
    $lines = & pnputil /enum-drivers
    $oemName = $null
    foreach ($line in $lines) {
        if ($line -match 'oem\d+\.inf') {
            $oemName = ($line -replace '.*?(oem\d+\.inf).*', '$1').Trim()
        }
        if ($oemName -and $line -match 'mttvdd') {
            Write-Host "Removing driver (fallback): $oemName"
            & pnputil /delete-driver $oemName /uninstall /force
            $oemName = $null
        }
    }
}

# 3. Remove C:\VirtualDisplayDriver (includes settings file).
if (Test-Path 'C:\VirtualDisplayDriver') {
    Remove-Item -Recurse -Force 'C:\VirtualDisplayDriver'
    Write-Host "Removed C:\VirtualDisplayDriver"
}

Write-Host "VDD removal complete"
