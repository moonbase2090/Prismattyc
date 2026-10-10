# Remove Prismattyc Windows preview directories this project created.
# A directory junction is left in place. Remove-Item -Recurse on a junction
# would delete the junction target, so this script never walks a reparse point.
# It does not stop any process, and it does not touch %APPDATA% or
# %LOCALAPPDATA%\Prismattyc data.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function ConvertTo-ComparablePath {
    param([string]$Entry)
    if ([string]::IsNullOrWhiteSpace($Entry)) { return '' }
    return $Entry.Trim().TrimEnd('/\').Replace('/', '\').ToLowerInvariant()
}

function Test-ReparsePoint {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    $item = Get-Item -LiteralPath $LiteralPath -Force -ErrorAction SilentlyContinue
    if (-not $item) { return $false }
    return (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)
}

function Remove-PreviewDirectory {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    if (Test-ReparsePoint $LiteralPath) { return }
    foreach ($child in @(Get-ChildItem -LiteralPath $LiteralPath -Force)) {
        if ($child.PSIsContainer) {
            if (Test-ReparsePoint $child.FullName) {
                [System.IO.Directory]::Delete($child.FullName)
            } else {
                Remove-PreviewDirectory $child.FullName
            }
        } else {
            Remove-Item -LiteralPath $child.FullName -Force
        }
    }
    Remove-Item -LiteralPath $LiteralPath -Force
}

function Get-PreviewDirectoryFromEntry {
    param([string]$Entry)
    $trimmed = $Entry.Trim().TrimEnd('/\')
    if (-not $trimmed) { return $null }
    $segments = @($trimmed.Replace('/', '\') -split '\\')
    for ($i = 0; $i -lt $segments.Count; $i++) {
        if ($segments[$i] -like 'windows-preview-*') {
            return ($segments[0..$i] -join '\')
        }
    }
    return $null
}

$localAppData = [Environment]::GetFolderPath('LocalApplicationData')
if ([string]::IsNullOrWhiteSpace($localAppData)) { exit 0 }
$programs = Join-Path $localAppData 'Programs\Prismattyc'
$installBin = Join-Path $programs 'bin'
$installCmd = Join-Path $programs 'cmd'

if (Test-Path -LiteralPath $programs) {
    foreach ($child in @(Get-ChildItem -LiteralPath $programs -Force -Directory)) {
        if ($child.Name -notlike 'windows-preview-*') { continue }
        if (Test-ReparsePoint $child.FullName) { continue }
        Remove-PreviewDirectory $child.FullName
    }
}

$startMenu = [Environment]::GetFolderPath('Programs')
if ($startMenu -and (Test-Path -LiteralPath $startMenu)) {
    foreach ($shortcut in @(Get-ChildItem -LiteralPath $startMenu -Filter 'Prismattyc Windows Preview *.lnk' -File -Force -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $shortcut.FullName -Force
    }
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($null -eq $userPath) { $userPath = '' }
$binKey = ConvertTo-ComparablePath $installBin
$cmdKey = ConvertTo-ComparablePath $installCmd
$kept = New-Object System.Collections.Generic.List[string]
foreach ($part in @($userPath.Split([char[]]@(';'), [System.StringSplitOptions]::RemoveEmptyEntries))) {
    $key = ConvertTo-ComparablePath $part
    # The cmd shim directory is the PATH entry this install adds. Leave it.
    if ($key -eq $cmdKey) {
        $kept.Add($part)
        continue
    }
    # Drop the install bin directory left by an older install or a preview.
    if ($key -eq $binKey) { continue }
    $previewRoot = Get-PreviewDirectoryFromEntry $part
    if ($previewRoot) {
        if (-not (Test-Path -LiteralPath $previewRoot)) { continue }
        if (Test-ReparsePoint $previewRoot) {
            $kept.Add($part)
            continue
        }
    }
    $kept.Add($part)
}
$updated = ($kept -join ';')
if ($updated -cne $userPath) {
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
}
