# Start the installed pmuxd when the default socket is down.
# The MSI runs this after the files are copied and at the next user logon.
# pmux up leaves a daemon that is already running alone.
# Each run records its result in %LOCALAPPDATA%\Prismattyc\pmuxd-start.txt,
# because a quiet install and a logon start have no console to show it.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Write-StartStatus([int]$Code, [string]$Result) {
    if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) { return }
    try {
        $dir = Join-Path $env:LOCALAPPDATA 'Prismattyc'
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $text = "time: $([DateTime]::UtcNow.ToString('o'))`r`nexit: $Code`r`nresult: $Result`r`n"
        [System.IO.File]::WriteAllText((Join-Path $dir 'pmuxd-start.txt'), $text)
    } catch {
        [Console]::Out.WriteLine("WARNING: could not record the pmuxd start result: $_")
    }
}

function Exit-Start([int]$Code, [string]$Message) {
    if ($Code -eq 0) {
        [Console]::Out.WriteLine('pmuxd is running.')
        Write-StartStatus 0 'pmuxd is running'
    } else {
        # The MSI treats this action's exit code as a warning and copies this
        # line into its log. Say why pmuxd is down.
        [Console]::Out.WriteLine("WARNING: Prismattyc is installed, but pmuxd did not start. $Message")
        Write-StartStatus $Code "pmuxd did not start. $Message"
    }
    exit $Code
}

$bin = Split-Path -Parent $PSCommandPath
$pmux = Join-Path $bin 'pmux.exe'
if (-not (Test-Path -LiteralPath $pmux -PathType Leaf)) {
    Exit-Start 1 "Missing $pmux"
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
    Exit-Start 1 "pmux up did not start: $_"
}
if (-not $process.WaitForExit(30000)) {
    try { $process.Kill() } catch { }
    Exit-Start 1 'pmux up timed out. Sign in again, or run pmux up from the cmd directory.'
}
if ($process.ExitCode -ne 0) {
    Exit-Start $process.ExitCode "pmux up exited $($process.ExitCode). Sign in again, or run pmux up from the cmd directory."
}
Exit-Start 0 ''
