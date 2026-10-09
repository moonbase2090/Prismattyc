# Embedded in the installer. The system account runs this. It is not installed
# under the user profile, and it does not load a file from the install directory.
# The command line carries PRISMATTYC_ARP_DATA=product|install root|temp.
# This sets DisplayIcon on an existing machine-scope uninstall key. It does not create a key.
$ErrorActionPreference = 'Continue'
$proc = Get-CimInstance -ClassName Win32_Process -Filter ("ProcessId=" + $PID)
$line = [string]$proc.CommandLine
$marker = 'PRISMATTYC_ARP_DATA='
$i = $line.IndexOf($marker)
$data = ''
if ($i -ge 0) {
    $data = $line.Substring($i + $marker.Length).Trim().Trim('"')
}
$parts = @()
if ($data) { $parts = $data.Split([char]'|') }
$ProductCode = ''
$Root = ''
$TraceDir = ''
if ($parts.Length -ge 3) {
    $ProductCode = $parts[0]
    $Root = $parts[1]
    $TraceDir = $parts[2].Trim().Trim('"')
}
$trace = $null
if ($TraceDir) { $trace = Join-Path $TraceDir 'prismattyc-displayicon.txt' }
function Write-Trace([string]$Line) {
    if (-not $trace) { return }
    Add-Content -LiteralPath $trace -Value $Line -Encoding ascii
}
if (-not $ProductCode -or -not $Root) {
    Write-Trace 'gave-up root=HKLM missing-data'
    exit 1
}
Write-Trace 'waiting root=HKLM'
$icon = (Join-Path (Join-Path $Root.TrimEnd('\') 'bin') 'prismattyc-host.exe') + ',0'
$deadline = (Get-Date).AddSeconds(20)
$base = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
do {
    $wrote = $false
    if (Test-Path -LiteralPath $base) {
        foreach ($item in Get-ChildItem -LiteralPath $base) {
            $name = [string]$item.GetValue('DisplayName')
            $publisher = [string]$item.GetValue('Publisher')
            $match = ($item.PSChildName -eq $ProductCode) -or ($name -eq 'Prismattyc' -and $publisher -eq 'Moonbase2090')
            if (-not $match) { continue }
            $target = "$base\$($item.PSChildName)"
            if (-not (Test-Path -LiteralPath $target)) { continue }
            try {
                Set-ItemProperty -LiteralPath $target -Name DisplayIcon -Value $icon -ErrorAction Stop
                Write-Trace "wrote=$target"
                $wrote = $true
            } catch {
                Write-Trace "error=$target $($_.Exception.Message)"
                exit 1
            }
        }
    }
    if ($wrote) { exit 0 }
    Start-Sleep -Milliseconds 200
} while ((Get-Date) -lt $deadline)
Write-Trace 'gave-up root=HKLM'
exit 0
