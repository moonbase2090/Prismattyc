# Set DisplayIcon once the uninstall key exists.
# Windows Installer creates that key after the install sequence returns, so the
# sequence action starts a hidden helper and returns. The helper does not create a key.
[CmdletBinding()]
param(
    [switch]$Apply,
    [Parameter(Mandatory = $true)][string]$ProductCode,
    [Parameter(Mandatory = $true)][string]$Icon
)
$ErrorActionPreference = 'Continue'
$trace = Join-Path ([System.IO.Path]::GetTempPath()) 'prismattyc-displayicon.txt'

function Write-Trace([string]$Line) {
    Add-Content -LiteralPath $trace -Value $Line -Encoding ascii
}

if (-not $Apply) {
    Write-Trace 'spawned'
    $exe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    Start-Process -FilePath $exe -WindowStyle Hidden -ArgumentList @(
        '-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', '-ExecutionPolicy', 'Bypass',
        '-File', $PSCommandPath, '-Apply', '-ProductCode', $ProductCode, '-Icon', $Icon
    ) | Out-Null
    exit 0
}

Write-Trace 'waiting'
$deadline = (Get-Date).AddSeconds(20)
do {
    $wrote = $false
    foreach ($root in @('HKCU', 'HKLM')) {
        $base = "${root}:\Software\Microsoft\Windows\CurrentVersion\Uninstall"
        if (-not (Test-Path -LiteralPath $base)) { continue }
        foreach ($item in Get-ChildItem -LiteralPath $base) {
            $name = [string]$item.GetValue('DisplayName')
            $publisher = [string]$item.GetValue('Publisher')
            $match = ($item.PSChildName -eq $ProductCode) -or ($name -eq 'Prismattyc' -and $publisher -eq 'Moonbase2090')
            if (-not $match) { continue }
            $target = "$base\$($item.PSChildName)"
            try {
                Set-ItemProperty -LiteralPath $target -Name DisplayIcon -Value $Icon -ErrorAction Stop
                Write-Trace "wrote=$target"
                $wrote = $true
            } catch {
                Write-Trace "error=$target $($_.Exception.Message)"
            }
        }
    }
    if ($wrote) { exit 0 }
    Start-Sleep -Milliseconds 200
} while ((Get-Date) -lt $deadline)
Write-Trace 'gave-up'
exit 0
