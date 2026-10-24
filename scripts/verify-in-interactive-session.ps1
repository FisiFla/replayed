<#
.SYNOPSIS
    Run `cargo xtask verify` on this Windows machine, in its interactive desktop session,
    from a shell that does not have one (an SSH session, a service, a CI runner).

.DESCRIPTION
    The capture path needs an interactive desktop session: Windows Graphics Capture
    captures a session's desktop, and a process started by a service or over plain SSH
    without `-t` either has no window station/desktop of its own or captures session 0's
    invisible one. So the harness has to be started *inside* the session the human is
    logged into.

    This script does that with a one-shot scheduled task:

      1. registers a task whose principal is the current user with `-LogonType
         Interactive`, so Windows runs it in that user's interactive session;
      2. starts it and waits for the marker file the worker writes when the harness has
         finished;
      3. reads the exit marker, prints where the report is, and leaves it in place;
      4. unregisters the task — in a `finally`, so it happens whether the run succeeded,
         failed or timed out.

    It creates no windows, sends no keystrokes and needs nothing from the human: the
    clip the harness takes is triggered from inside the application
    (`buffer --self-test-clip-after`), never by synthesising input. That is deliberate —
    on a machine running kernel-level anti-cheat, synthetic input is hostile behaviour,
    and a verification that gets the machine banned is not verification.

    The report lands on the *box* (by default under <repo>\target\verify\); copy it back
    with `scp`/`Type` or attach it from there.

.PARAMETER Repo
    The checkout to verify. Defaults to this script's own repository root.

.PARAMETER Report
    Where the Markdown report goes. Defaults to <Repo>\target\verify\report-<timestamp>.md.

.PARAMETER Seconds
    How much media the buffer accumulates before the self-test clip (the harness's
    --seconds). Default 45.

.PARAMETER DevSoftwareEncoder
    Pass --dev-software-encoder (libx264, no GPU encoder needed). Only useful for a
    development host; on a real Windows box leave it off so the hardware path is verified.

.PARAMETER WaitSeconds
    How long to wait for the harness before giving up and cleaning up. Default 2700 (45
    minutes — the release build is the slow part on a cold machine).

.PARAMETER Worker
    Internal: the mode the scheduled task runs. Not meant to be typed by a human.

.EXAMPLE
    # from an SSH session on the Windows box:
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\verify-in-interactive-session.ps1
#>
[CmdletBinding()]
param(
    [string] $Repo = '',
    [string] $Report = '',
    [int] $Seconds = 45,
    [switch] $DevSoftwareEncoder,
    [int] $WaitSeconds = 2700,
    [switch] $Worker
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# The checkout this script lives in, when the caller did not name one.
if (-not $Repo) {
    $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
}
if (-not $Report) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $Report = Join-Path $Repo ("target\verify\report-$stamp.md")
}

# ---------------------------------------------------------------------------------------
# Worker: runs inside the interactive session, started by the scheduled task.
# ---------------------------------------------------------------------------------------
if ($Worker) {
    # A scheduled task gets the machine and profile PATH, which usually includes cargo —
    # but rustup installs into the user profile, and that is where it lives on most
    # developer machines. Adding it defensively costs nothing.
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }

    Set-Location -LiteralPath $Repo
    $cargoArgs = @('xtask', 'verify', '--seconds', "$Seconds", '--report', $Report)
    if ($DevSoftwareEncoder) {
        $cargoArgs += '--dev-software-encoder'
    }

    Write-Host "verify: running cargo $($cargoArgs -join ' ') in session $((Get-Process -Id $PID).SessionId)"
    & cargo @cargoArgs
    $exitCode = $LASTEXITCODE

    # The marker is how the waiting shell knows the run is over; the number in it is the
    # harness's own verdict (0 = every check that ran passed).
    Set-Content -LiteralPath "$Report.exit" -Value $exitCode -Encoding ascii
    exit $exitCode
}

# ---------------------------------------------------------------------------------------
# Outer: runs wherever the human invoked this script from.
# ---------------------------------------------------------------------------------------
$marker = "$Report.exit"
Remove-Item -LiteralPath $marker -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Report) | Out-Null

$selfSession = (Get-Process -Id $PID).SessionId
if ($selfSession -eq 0) {
    Write-Host "note: this shell is in session 0 (no desktop). The task below runs in the"
    Write-Host "      logged-on user's interactive session instead, which is what capture needs."
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$taskName = "replayed-verify-$stamp"
$workerArgs = "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker " +
              "-Repo `"$Repo`" -Report `"$Report`" -Seconds $Seconds"
if ($DevSoftwareEncoder) {
    $workerArgs += ' -DevSoftwareEncoder'
}

$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $workerArgs -WorkingDirectory $Repo
# `Interactive` is the whole point: the task runs in the current user's desktop session,
# so the capture backend has the desktop the human is looking at. An elevated run level is
# not needed (and would be a second thing to get wrong), so it is explicitly `Limited`.
#
# The principal is named by **SID**, not by `DOMAIN\user`. An SSH session on a workgroup
# machine reports `USERDOMAIN=WORKGROUP`, and `WORKGROUP\flavio` does not resolve: registering
# the task then fails with "No mapping between account names and security IDs was done"
# (0x80070534) before anything runs. The SID is what the account *is*; the scheduling service
# resolves it without a domain lookup. Found by running this script on the box.
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit (New-TimeSpan -Seconds ($WaitSeconds + 900))

$exitCode = 1
try {
    Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Write-Host "verify: '$taskName' started; waiting for the report (up to $WaitSeconds s)"

    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    while ((Get-Date) -lt $deadline -and -not (Test-Path -LiteralPath $marker)) {
        Start-Sleep -Seconds 5
    }

    if (Test-Path -LiteralPath $marker) {
        $exitCode = [int](Get-Content -LiteralPath $marker -Raw).Trim()
        Write-Host ''
        Write-Host "verify: the harness exited with $exitCode (0 = every check that ran passed)"
        Write-Host "verify: report: $Report"
        if (Test-Path -LiteralPath $Report) {
            # Enough of the report to see the verdict without pulling the whole file.
            Write-Host ''
            Get-Content -LiteralPath $Report | Select-Object -First 12 | ForEach-Object { Write-Host "  $_" }
        } else {
            Write-Host "verify: the harness wrote no report; see the run's own output above."
        }
    } else {
        Write-Error ("verify: no report after $WaitSeconds s. The usual cause is that nobody " +
            "is logged on to this machine right now (a task with -LogonType Interactive " +
            "cannot run without a session), or that the checkout does not build. Check " +
            "`Get-ScheduledTaskInfo -TaskName $taskName` and the task's history.")
    }
} finally {
    # Always: the task exists only for this one run. A leftover task would be a second,
    # silent thing capturing on this machine.
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
}

exit $exitCode
