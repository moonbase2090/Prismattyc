[CmdletBinding()]
param([switch]$AddToPath)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Run this installer on Windows.' }
$root = $PSScriptRoot
$manifest = Get-Content -LiteralPath (Join-Path $root 'manifest.json') -Raw | ConvertFrom-Json
if ($manifest.source_revision -notmatch '^[0-9a-f]{40}$') { throw 'Invalid source revision.' }
if ($manifest.version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid package version.' }
if ($manifest.kind -cne 'dogfood-preview' -or $manifest.target -cne 'x86_64-pc-windows-gnu' -or
    $manifest.native_runtime_verified -isnot [bool] -or $manifest.native_runtime_verified -or
    $manifest.signed -isnot [bool] -or $manifest.signed -or
    $manifest.cross_compiled -isnot [bool] -or -not $manifest.cross_compiled) {
    throw 'Expected an unsigned, cross-compiled, native-runtime-unverified preview.'
}
Write-Warning 'Unsigned preview. Checksums detect corruption, not publisher authenticity. Native runtime is unverified.'
$expectedBins = @('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
$requiredFiles = @('README.txt', 'SOURCE.txt', 'install.ps1', 'source.tar.gz',
    'licenses/MPL-2.0.txt', 'licenses/NOTICE.txt', 'licenses/OMARCHY-LICENSE.txt',
    'licenses/JetBrainsMonoNerdFont-OFL.txt', 'licenses/NerdFonts-LICENSES.txt',
    'licenses/NotoSansSymbols2-OFL.txt', 'licenses/DejaVuSansMono-LICENSE.txt')
$requiredFiles += @($expectedBins | ForEach-Object { "bin/$_.exe" })
$seen = @{}
foreach ($entry in $manifest.files) {
    $relative = [string]$entry.path
    if ($requiredFiles -cnotcontains $relative) {
        throw "Unexpected package path: $relative"
    }
    if ($seen.ContainsKey($relative)) { throw "Duplicate package path: $relative" }
    if ($entry.sha256 -cnotmatch '^[0-9a-f]{64}$' -or $entry.size -lt 0) {
        throw "Invalid file metadata: $relative"
    }
    $seen[$relative] = $true
    $source = Join-Path $root $relative
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing package file: $relative" }
    if ((Get-Item -LiteralPath $source).Length -ne $entry.size -or (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne $entry.sha256) {
        throw "Checksum mismatch: $relative"
    }
}
foreach ($relative in $requiredFiles) {
    if (-not $seen.ContainsKey($relative)) { throw "Missing package entry: $relative" }
}
foreach ($name in $expectedBins) {
    $probe = New-Object System.Diagnostics.Process
    $probe.StartInfo.FileName = Join-Path $root "bin/$name.exe"
    $probe.StartInfo.Arguments = '--version'
    $probe.StartInfo.UseShellExecute = $false
    $probe.StartInfo.CreateNoWindow = $true
    $probe.StartInfo.RedirectStandardOutput = $true
    $probe.StartInfo.RedirectStandardError = $true
    try {
        if (-not $probe.Start()) { throw "Could not start $name version probe." }
        $stdout = $probe.StandardOutput.ReadToEndAsync()
        $stderr = $probe.StandardError.ReadToEndAsync()
        if (-not $probe.WaitForExit(10000)) {
            $probe.Kill()
            throw "$name version probe timed out."
        }
        if (-not $stdout.Wait(1000) -or -not $stderr.Wait(1000)) {
            throw "$name version output timed out."
        }
        $text = $stdout.Result + ' ' + $stderr.Result
        if ($probe.ExitCode -ne 0) { throw "$name could not run on this Windows machine: $text" }
    } finally { $probe.Dispose() }
    $versionPattern = '(?<!\S)' + [regex]::Escape($manifest.version) + '(?!\S)'
    if ($text -notmatch $versionPattern -or $text -notmatch $manifest.source_revision.Substring(0,12)) {
        throw "Unexpected executable version: $name : $text"
    }
    Write-Host $text
}
if (-not $env:LOCALAPPDATA -or -not [IO.Path]::IsPathRooted($env:LOCALAPPDATA)) {
    throw 'LOCALAPPDATA must be an absolute path.'
}
$installRoot = Join-Path $env:LOCALAPPDATA 'Programs/Prismattyc'
$destination = Join-Path $installRoot ('windows-preview-' + $manifest.source_revision)
if (Test-Path -LiteralPath $destination) { throw "This preview is already installed: $destination" }
New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
New-Item -ItemType Directory -Path $destination | Out-Null
try {
    foreach ($entry in $manifest.files) {
        $target = Join-Path $destination $entry.path
        New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
        Copy-Item -LiteralPath (Join-Path $root $entry.path) -Destination $target
        if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $entry.sha256) {
            throw "Installed checksum mismatch: $($entry.path)"
        }
    }
    Copy-Item -LiteralPath (Join-Path $root 'manifest.json') -Destination $destination
} catch {
    Remove-Item -LiteralPath $destination -Recurse -Force
    throw
}
$binDir = Join-Path $destination 'bin'
$startMenu = [Environment]::GetFolderPath('Programs')
if ($startMenu) {
    try {
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut((Join-Path $startMenu ('Prismattyc Windows Preview ' + $manifest.source_revision + '.lnk')))
        $shortcut.TargetPath = Join-Path $binDir 'prismattyc-host.exe'
        $shortcut.WorkingDirectory = [Environment]::GetFolderPath('UserProfile')
        $shortcut.Save()
    } catch { Write-Warning "Installed binaries, but could not create the Start menu shortcut: $_" }
}
if ($AddToPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($userPath -split ';' | Where-Object { $_ })
    if ($parts -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', ((@($binDir) + $parts) -join ';'), 'User')
    }
    $env:Path = "$binDir;$env:Path"
}
Write-Host "Installed Windows preview: $destination"
Write-Host "Launch from Start: Prismattyc Windows Preview $($manifest.source_revision)"
Write-Host "Or run: $binDir\prismattyc-host.exe"
