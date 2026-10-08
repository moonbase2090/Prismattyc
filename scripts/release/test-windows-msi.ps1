# Install, upgrade, and uninstall the per-user Prismattyc MSI.
# Run on Windows with nothing in %LOCALAPPDATA%\Programs\Prismattyc\bin executing.
# The MSI refuses to replace a locked executable and does not stop pmuxd.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Msi,
    [string]$UpgradeFrom = '',
    [string]$ExpectedVersion = '',
    [string]$ExpectedProductVersion = '',
    [switch]$AddToPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Run this test on Windows.' }

function Invoke-Msi {
    param([Parameter(Mandatory = $true)][string[]]$ArgumentList)
    $process = Start-Process -FilePath msiexec.exe -ArgumentList $ArgumentList -Wait -PassThru -NoNewWindow
    if ($process.ExitCode -ne 0) {
        throw "msiexec $($ArgumentList -join ' ') exited $($process.ExitCode)"
    }
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
            $props = Get-ItemProperty -LiteralPath $item.PSPath
            if ($props.DisplayName -eq 'Prismattyc' -and $props.Publisher -eq 'Moonbase2090') {
                return [pscustomobject]@{
                    ProductCode    = $item.PSChildName
                    DisplayVersion = [string]$props.DisplayVersion
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
    $parts = @($userPath -split ';' | Where-Object { $_ })
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
    if ($userPath -split ';' -contains $install.Bin) { throw 'User PATH still contains the install bin.' }
    foreach ($path in $Survivors) {
        if (-not (Test-Path -LiteralPath $path)) { throw "Uninstall removed preserved data: $path" }
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

$installArgs = @('/i', $msiPath, '/qn', '/norestart')
if ($AddToPath) { $installArgs += 'ADDTOPATH=1' } else { $installArgs += 'ADDTOPATH=0' }

if ($UpgradeFrom) {
    $older = (Resolve-Path -LiteralPath $UpgradeFrom).Path
    Invoke-Msi -ArgumentList @('/i', $older, '/qn', '/norestart', 'ADDTOPATH=0')
}
Invoke-Msi -ArgumentList $installArgs
Assert-Installed -VersionText $ExpectedVersion -ProductVersion $ExpectedProductVersion -PathExpected ([bool]$AddToPath)

$installed = Get-PrismattycUninstallKey
Invoke-Msi -ArgumentList @('/x', $installed.ProductCode, '/qn', '/norestart')
Assert-Removed -Survivors $preserved
Write-Host "MSI install, upgrade, and uninstall checks passed."
