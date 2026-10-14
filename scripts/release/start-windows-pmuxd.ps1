# Start the installed pmuxd when the default socket is down.
# The MSI runs this after the files are copied and at the next user logon.
# pmux up leaves a daemon that is already running alone.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Write-StartWarning([string]$Message) {
    # The MSI treats this action's exit code as a warning. Say why pmuxd is down.
    [Console]::Error.WriteLine("WARNING: Prismattyc is installed, but pmuxd did not start. $Message")
}

$bin = Split-Path -Parent $PSCommandPath
$pmux = Join-Path $bin 'pmux.exe'
if (-not (Test-Path -LiteralPath $pmux -PathType Leaf)) {
    Write-StartWarning "Missing $pmux"
    exit 1
}

$start = New-Object System.Diagnostics.ProcessStartInfo
$start.FileName = $pmux
$start.Arguments = 'up'
$start.WorkingDirectory = $bin
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
try {
    $process = [System.Diagnostics.Process]::Start($start)
} catch {
    Write-StartWarning "pmux up did not start: $_"
    exit 1
}
if (-not $process.WaitForExit(30000)) {
    try { $process.Kill() } catch { }
    Write-StartWarning 'pmux up timed out. Sign in again, or run pmux up from the cmd directory.'
    exit 1
}
if ($process.ExitCode -ne 0) {
    Write-StartWarning "pmux up exited $($process.ExitCode). Sign in again, or run pmux up from the cmd directory."
    exit $process.ExitCode
}
exit 0
