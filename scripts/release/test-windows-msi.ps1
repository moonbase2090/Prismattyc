# Install, upgrade, and uninstall the per-user Prismattyc MSI.
# Run on Windows with nothing in %LOCALAPPDATA%\Programs\Prismattyc\bin executing.
# The MSI does not stop pmuxd. This test locks one installed executable and
# records the msiexec exit code instead of treating Restart Manager as proof.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Msi,
    [string]$UpgradeFrom = '',
    [string]$ExpectedVersion = '',
    [string]$ExpectedProductVersion = '',
    [switch]$AddToPath,
    [switch]$FormatArguments
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Format-ProcessArgument {
    param([Parameter(Mandatory = $true)][string]$Value)
    # Start-Process joins an argument array with spaces and drops the quoting.
    # msiexec receives one command line, so each argument is quoted here.
    return '"' + $Value.Replace('"', '""') + '"'
}

function New-MsiCommandLine {
    param([Parameter(Mandatory = $true)][string[]]$ArgumentList)
    return (($ArgumentList | ForEach-Object { Format-ProcessArgument $_ }) -join ' ')
}

function Get-MsiProofPhases {
    $phases = @('install')
    if ($UpgradeFrom) { $phases += 'upgrade' }
    if ($ExpectedVersion -or $ExpectedProductVersion) { $phases += 'version' }
    $phases += @('locked-file', 'uninstall')
    return $phases
}

function Get-MsiTail {
    $tail = @('/qn', '/norestart')
    if ($AddToPath) { $tail += 'ADDTOPATH=1' } else { $tail += 'ADDTOPATH=0' }
    # Keep the runner from rebooting. This does not claim the copy failed.
    $tail += 'REBOOT=ReallySuppress'
    return $tail
}

if ($FormatArguments) {
    $tail = Get-MsiTail
    if ($UpgradeFrom) {
        Write-Output ('upgrade=' + (New-MsiCommandLine (@('/i', $UpgradeFrom) + $tail)))
    }
    Write-Output ('install=' + (New-MsiCommandLine (@('/i', $Msi) + $tail)))
    Write-Output ('phases=' + ((Get-MsiProofPhases) -join ','))
    exit 0
}

if ($env:OS -ne 'Windows_NT') { throw 'Run this test on Windows.' }

function Start-Msi {
    param(
        [Parameter(Mandatory = $true)][string]$CommandLine,
        [int]$TimeoutMilliseconds = 300000
    )
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = Join-Path $env:SystemRoot 'System32\msiexec.exe'
    $info.Arguments = $CommandLine
    $info.UseShellExecute = $false
    $started = New-Object System.Diagnostics.Process
    $started.StartInfo = $info
    if (-not $started.Start()) { throw "msiexec did not start: $CommandLine" }
    if (-not $started.WaitForExit($TimeoutMilliseconds)) {
        try { $started.Kill() } catch { }
        throw "msiexec timed out: $CommandLine"
    }
    return $started.ExitCode
}

function Invoke-Msi {
    param([Parameter(Mandatory = $true)][string[]]$ArgumentList)
    $commandLine = New-MsiCommandLine $ArgumentList
    $code = Start-Msi -CommandLine $commandLine
    if ($code -ne 0) { throw "msiexec $commandLine exited $code" }
}

function Get-PrismattycInstall {
    $local = [Environment]::GetFolderPath('LocalApplicationData')
    [pscustomobject]@{
        Root = Join-Path $local 'Programs\Prismattyc'
        Bin  = Join-Path $local 'Programs\Prismattyc\bin'
    }
}

function Get-PrismattycUninstallKey {
    $roots = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($item in Get-ChildItem -LiteralPath $root) {
            if ($item.GetValue('DisplayName') -eq 'Prismattyc' -and $item.GetValue('Publisher') -eq 'Moonbase2090') {
                return [pscustomobject]@{
                    ProductCode    = $item.PSChildName
                    DisplayVersion = [string]$item.GetValue('DisplayVersion')
                }
            }
        }
    }
    return $null
}

function Assert-Installed {
    param([string]$VersionText, [string]$ProductVersion, [bool]$PathExpected)
    $install = Get-PrismattycInstall
    foreach ($name in @('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')) {
        $path = Join-Path $install.Bin "$name.exe"
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing $path" }
    }
    foreach ($name in @('README.txt', 'VERSION', 'licenses\MPL-2.0.txt', 'licenses\NOTICE.txt')) {
        $path = Join-Path $install.Root $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing $path" }
    }
    if ($VersionText) {
        $versionFile = (Get-Content -LiteralPath (Join-Path $install.Root 'VERSION') -Raw).Trim()
        if ($versionFile -ne $VersionText) { throw "VERSION file is $versionFile, expected $VersionText" }
        $probe = & (Join-Path $install.Bin 'pmux.exe') --version
        if ($probe -notmatch [regex]::Escape($VersionText) -and $probe -notmatch [regex]::Escape(($VersionText -split '-', 2)[0])) {
            throw "pmux --version did not report ${VersionText}: $probe"
        }
    }
    $shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Prismattyc.lnk'
    if (-not (Test-Path -LiteralPath $shortcut -PathType Leaf)) { throw "Missing Start menu shortcut $shortcut" }
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($shortcut)
    $expectedTarget = Join-Path $install.Bin 'prismattyc-host.exe'
    if ($link.TargetPath -ne $expectedTarget) {
        throw "Shortcut target $($link.TargetPath) is not $expectedTarget"
    }
    $arp = Get-PrismattycUninstallKey
    if (-not $arp) { throw 'Prismattyc is missing from Apps & features.' }
    if ($ProductVersion -and $arp.DisplayVersion -ne $ProductVersion) {
        throw "ARP version $($arp.DisplayVersion) is not $ProductVersion"
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($userPath -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') })
    $listed = $parts -contains $install.Bin
    if ($PathExpected -xor $listed) {
        throw "PATH opt-in is $listed; expected $PathExpected"
    }
}

function Assert-Removed {
    param([string[]]$Survivors)
    $install = Get-PrismattycInstall
    if (Test-Path -LiteralPath $install.Bin) { throw "Bin directory remains: $($install.Bin)" }
    $shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Prismattyc.lnk'
    if (Test-Path -LiteralPath $shortcut) { throw "Start menu shortcut remains: $shortcut" }
    if (Get-PrismattycUninstallKey) { throw 'ARP entry remains after uninstall.' }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (@($userPath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -contains $install.Bin) { throw 'User PATH still contains the install bin.' }
    foreach ($path in $Survivors) {
        if (-not (Test-Path -LiteralPath $path)) { throw "Uninstall removed preserved data: $path" }
    }
}

function Get-Sha256Hex {
    param([byte[]]$Bytes)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([System.BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Read-AllBytes {
    param([Parameter(Mandatory = $true)][System.IO.FileStream]$Stream)
    $Stream.Position = 0
    $buffer = New-Object byte[] $Stream.Length
    $read = 0
    while ($read -lt $buffer.Length) {
        $count = $Stream.Read($buffer, $read, $buffer.Length - $read)
        if ($count -le 0) { throw 'short read of locked executable' }
        $read += $count
    }
    return $buffer
}

function Assert-LockedExecutableSurvivesReinstall {
    param([Parameter(Mandatory = $true)][string]$CommandLine)
    $install = Get-PrismattycInstall
    $target = Join-Path $install.Bin 'pmux.exe'
    $stream = [System.IO.File]::Open(
        $target,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::None)
    try {
        $before = Get-Sha256Hex (Read-AllBytes $stream)
        $code = Start-Msi -CommandLine $CommandLine
        $after = Get-Sha256Hex (Read-AllBytes $stream)
        if ($after -ne $before) { throw 'locked executable bytes changed during msiexec' }
        if (-not $stream.CanRead) { throw 'lock did not survive msiexec' }
        Write-Output "locked-file msiexec exit: $code"
        Write-Output 'locked-file bytes unchanged'
    } finally {
        $stream.Dispose()
    }
}

$msiPath = (Resolve-Path -LiteralPath $Msi).Path
$install = Get-PrismattycInstall
$preserved = @(
    (Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'prismattyc-msi-preserve.txt'),
    (Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Prismattyc\msi-preserve.txt')
)
New-Item -ItemType Directory -Path (Split-Path -Parent $preserved[1]) -Force | Out-Null
Set-Content -LiteralPath $preserved[0] -Value 'keep' -Encoding ascii
Set-Content -LiteralPath $preserved[1] -Value 'keep' -Encoding ascii

$tail = Get-MsiTail
$installArgs = @('/i', $msiPath) + $tail
if ($UpgradeFrom) {
    $older = (Resolve-Path -LiteralPath $UpgradeFrom).Path
    Invoke-Msi -ArgumentList (@('/i', $older) + $tail)
}
Invoke-Msi -ArgumentList $installArgs
Assert-Installed -VersionText $ExpectedVersion -ProductVersion $ExpectedProductVersion -PathExpected ([bool]$AddToPath)
Assert-LockedExecutableSurvivesReinstall -CommandLine (New-MsiCommandLine $installArgs)

$installed = Get-PrismattycUninstallKey
Invoke-Msi -ArgumentList (@('/x', $installed.ProductCode, '/qn', '/norestart', 'REBOOT=ReallySuppress'))
Assert-Removed -Survivors $preserved
Write-Output ("MSI {0} checks passed." -f ((Get-MsiProofPhases) -join ', '))
