# Prepare a Windows install before files are copied, and again before the
# installed daemon starts. Grant SYSTEM and Administrators access on the
# install tree, stop pmuxd when its executable is in that tree or the update
# store, and delete windows-current.json. A logon start does not run this.
# -Classify prints stop or keep for one executable path and changes nothing.
[CmdletBinding()]
param(
    [switch]$Classify,
    [string]$Image = '',
    [string]$InstallRoot = '',
    [string]$UpdatesRoot = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Test-PrismattycImageUnderRoot {
    param(
        [string]$ImagePath,
        [string]$Root
    )
    if ([string]::IsNullOrEmpty($ImagePath) -or [string]::IsNullOrEmpty($Root)) {
        return $false
    }
    $full = [System.IO.Path]::GetFullPath($ImagePath)
    $rootFull = [System.IO.Path]::GetFullPath($Root)
    $separator = [System.IO.Path]::DirectorySeparatorChar
    $prefix = $rootFull.TrimEnd($separator)
    if ($full.Equals($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    return $full.StartsWith($prefix + $separator, [System.StringComparison]::OrdinalIgnoreCase)
}

function Get-PrismattycUpdateRoot {
    # Match prismattyc_mux::platform::data_home: a nonempty XDG_DATA_HOME wins,
    # including on Windows. An empty value falls through. Do not trim, so a
    # whitespace-only value stays nonempty the same way the Rust check does.
    $xdg = $env:XDG_DATA_HOME
    $data = if (-not [string]::IsNullOrEmpty($xdg)) {
        $xdg
    } else {
        $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
        if (-not $localAppData) { throw 'LocalApplicationData is unset.' }
        $localAppData
    }
    return Join-Path (Join-Path $data 'prismattyc') 'updates'
}

function Get-PrismattycInstallRoot {
    $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
    if (-not $localAppData) { throw 'LocalApplicationData is unset.' }
    return Join-Path $localAppData 'Programs\Prismattyc'
}

function Grant-PrismattycInstallerAccess {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][bool]$Directory
    )
    $acl = Get-Acl -LiteralPath $Path
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $identifier = New-Object System.Security.Principal.SecurityIdentifier $sid
        if ($Directory) {
            $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
                $identifier, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        } else {
            $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
                $identifier, 'FullControl', 'Allow')
        }
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Repair-PrismattycInstallAcls {
    param([Parameter(Mandatory = $true)][string]$Root)
    if (-not (Test-Path -LiteralPath $Root)) { return }
    $top = Get-Item -LiteralPath $Root -Force
    if (($top.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "install directory is a reparse point: $Root"
    }
    $pending = New-Object 'System.Collections.Generic.Queue[string]'
    $pending.Enqueue($top.FullName)
    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            continue
        }
        Grant-PrismattycInstallerAccess -Path $item.FullName -Directory $item.PSIsContainer
        if (-not $item.PSIsContainer) { continue }
        foreach ($child in @(Get-ChildItem -LiteralPath $item.FullName -Force)) {
            if (($child.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                continue
            }
            $pending.Enqueue($child.FullName)
        }
    }
}

function Stop-StalePrismattycDaemons {
    param(
        [string]$InstallDirectory,
        [string]$UpdateDirectory
    )
    $found = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'pmuxd.exe'" -ErrorAction SilentlyContinue)
    foreach ($proc in $found) {
        $image = [string]$proc.ExecutablePath
        $stale = (Test-PrismattycImageUnderRoot $image $InstallDirectory) -or (Test-PrismattycImageUnderRoot $image $UpdateDirectory)
        if (-not $stale) { continue }
        Stop-Process -Id $proc.ProcessId -Force -ErrorAction SilentlyContinue
        $still = Get-Process -Id $proc.ProcessId -ErrorAction SilentlyContinue
        if ($still -and -not $still.HasExited) {
            throw "pmuxd $($proc.ProcessId) is still running from $image"
        }
    }
}

function Clear-PrismattycUpdatePointer {
    param([Parameter(Mandatory = $true)][string]$UpdateDirectory)
    $pointer = Join-Path $UpdateDirectory 'windows-current.json'
    if (Test-Path -LiteralPath $pointer) {
        Remove-Item -LiteralPath $pointer -Force
    }
}

if ($Classify) {
    $decision = (Test-PrismattycImageUnderRoot $Image $InstallRoot) -or (Test-PrismattycImageUnderRoot $Image $UpdatesRoot)
    if ($decision) { Write-Output 'stop' } else { Write-Output 'keep' }
    exit 0
}

$installDirectory = if (-not [string]::IsNullOrEmpty($InstallRoot)) { $InstallRoot } else { Get-PrismattycInstallRoot }
$updateDirectory = if (-not [string]::IsNullOrEmpty($UpdatesRoot)) { $UpdatesRoot } else { Get-PrismattycUpdateRoot }
Repair-PrismattycInstallAcls -Root $installDirectory
Stop-StalePrismattycDaemons -InstallDirectory $installDirectory -UpdateDirectory $updateDirectory
Clear-PrismattycUpdatePointer -UpdateDirectory $updateDirectory
Write-Output 'Prismattyc install prepared.'
