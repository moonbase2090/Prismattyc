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
    # msiexec parses one command line. Quote values that contain spaces.
    # A quoted switch such as "/qn" is not recognized, and the UI then waits.
    if ($Value -match '[\s"]') {
        return '"' + $Value.Replace('"', '""') + '"'
    }
    return $Value
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
        [int]$TimeoutMilliseconds = 180000
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
    $log = Join-Path ([System.IO.Path]::GetTempPath()) ('prismattyc-msi-{0}.log' -f [guid]::NewGuid().ToString('n'))
    $commandLine = New-MsiCommandLine ($ArgumentList + @('/l*v', $log))
    $code = Start-Msi -CommandLine $commandLine
    if ($code -ne 0) {
        $detail = ''
        if (Test-Path -LiteralPath $log) {
            $matched = Select-String -LiteralPath $log -Pattern 'Error [0-9]|Custom action|Return value|SetArpDisplayIcon' | Select-Object -Last 30
            if ($matched) { $detail = ($matched | ForEach-Object { $_.Line }) -join "`n" }
        }
        Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
        throw "msiexec $commandLine exited $code`n$detail"
    }
    Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
}

function Get-PrismattycInstall {
    $local = [Environment]::GetFolderPath('LocalApplicationData')
    [pscustomobject]@{
        Root = Join-Path $local 'Programs\Prismattyc'
        Bin  = Join-Path $local 'Programs\Prismattyc\bin'
        Cmd  = Join-Path $local 'Programs\Prismattyc\cmd'
    }
}

function Get-PrismattycUninstallKeys {
    $roots = @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($item in Get-ChildItem -LiteralPath $root) {
            if ($item.GetValue('DisplayName') -eq 'Prismattyc' -and $item.GetValue('Publisher') -eq 'Moonbase2090') {
                Write-Output ([pscustomobject]@{
                    ProductCode    = $item.PSChildName
                    DisplayVersion = [string]$item.GetValue('DisplayVersion')
                    DisplayIcon    = [string]$item.GetValue('DisplayIcon')
                    Hive           = $root
                })
            }
        }
    }
}

function Get-HklmPrismattycIcons {
    $root = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
    if (-not (Test-Path -LiteralPath $root)) { return }
    foreach ($item in Get-ChildItem -LiteralPath $root) {
        if ($item.GetValue('DisplayName') -eq 'Prismattyc' -and $item.GetValue('Publisher') -eq 'Moonbase2090') {
            Write-Output ([pscustomobject]@{
                ProductCode = $item.PSChildName
                DisplayIcon = [string]$item.GetValue('DisplayIcon')
            })
        }
    }
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
    foreach ($name in @('prismattyc', 'pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp')) {
        $shim = Join-Path $install.Cmd "$name.cmd"
        if (-not (Test-Path -LiteralPath $shim -PathType Leaf)) { throw "Missing $shim" }
    }
    if (Test-Path -LiteralPath (Join-Path $install.Cmd 'prismattyc-host.cmd')) {
        throw 'prismattyc-host.cmd is installed'
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
    $entries = @(Get-PrismattycUninstallKeys)
    if ($entries.Count -ne 1) {
        $listed = ($entries | ForEach-Object { "$($_.Hive) $($_.ProductCode) $($_.DisplayVersion)" }) -join '; '
        throw "ARP has $($entries.Count) Prismattyc entries: $listed"
    }
    if ($ProductVersion -and $entries[0].DisplayVersion -ne $ProductVersion) {
        throw "ARP version $($entries[0].DisplayVersion) is not $ProductVersion"
    }
    $expectedIcon = "$(Join-Path $install.Bin 'prismattyc-host.exe'),0"
    # The helpers set DisplayIcon after msiexec creates the key and returns.
    # A per-user key must not hide an empty machine-scope key.
    $deadline = (Get-Date).AddSeconds(10)
    $machine = @(Get-HklmPrismattycIcons)
    do {
        $badMachine = @($machine | Where-Object { $_.DisplayIcon -ne $expectedIcon })
        $entryOk = ($entries.Count -eq 1 -and $entries[0].DisplayIcon -eq $expectedIcon)
        if ($entryOk -and $badMachine.Count -eq 0) { break }
        if ((Get-Date) -ge $deadline) { break }
        Start-Sleep -Milliseconds 200
        $entries = @(Get-PrismattycUninstallKeys)
        $machine = @(Get-HklmPrismattycIcons)
    } while ($true)
    $trace = Join-Path ([System.IO.Path]::GetTempPath()) 'prismattyc-displayicon.txt'
    $extra = ''
    if (Test-Path -LiteralPath $trace) {
        $extra = "`n" + ((Get-Content -LiteralPath $trace -Raw).Trim())
    }
    $badMachine = @($machine | Where-Object { $_.DisplayIcon -ne $expectedIcon })
    if ($badMachine.Count -ne 0) {
        $shown = "$($badMachine[0].ProductCode) '$($badMachine[0].DisplayIcon)'"
        throw "HKLM DisplayIcon is not $expectedIcon ($shown)$extra"
    }
    if ($entries.Count -ne 1 -or $entries[0].DisplayIcon -ne $expectedIcon) {
        $shown = ''
        if ($entries.Count -eq 1) { $shown = "$($entries[0].Hive)\$($entries[0].ProductCode) '$($entries[0].DisplayIcon)'" }
        throw "ARP DisplayIcon is not $expectedIcon ($shown)$extra"
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $parts = @($userPath -split ';' | Where-Object { $_ } | ForEach-Object { $_.Trim().TrimEnd('\') })
    $binListed = @($parts | Where-Object { $_ -eq $install.Bin.TrimEnd('\') }).Count -gt 0
    $cmdListed = @($parts | Where-Object { $_ -eq $install.Cmd.TrimEnd('\') }).Count -gt 0
    if ($binListed) { throw 'User PATH still contains the install bin.' }
    if ($PathExpected -xor $cmdListed) {
        throw "PATH opt-in is $cmdListed; expected $PathExpected"
    }
    if ($PathExpected) {
        $env:Path = @(
            [Environment]::GetEnvironmentVariable('Path', 'Machine'),
            [Environment]::GetEnvironmentVariable('Path', 'User')
        ) -join ';'
        $resolved = @(Get-Command -Name prismattyc -CommandType Application -ErrorAction SilentlyContinue) | Select-Object -First 1
        $expectedShim = Join-Path $install.Cmd 'prismattyc.cmd'
        $shown = if ($resolved) { [string]$resolved.Source } else { '<none>' }
        if (-not $resolved -or [string]::Compare($shown, $expectedShim, $true) -ne 0) {
            throw "Get-Command prismattyc is $shown, not $expectedShim"
        }
        Write-Output "Get-Command prismattyc=$shown"
    }
}

function Assert-Removed {
    param([string[]]$Survivors)
    $install = Get-PrismattycInstall
    if (Test-Path -LiteralPath $install.Bin) { throw "Bin directory remains: $($install.Bin)" }
    if (Test-Path -LiteralPath $install.Cmd) { throw "Cmd directory remains: $($install.Cmd)" }
    $shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Prismattyc.lnk'
    if (Test-Path -LiteralPath $shortcut) { throw "Start menu shortcut remains: $shortcut" }
    $entries = @(Get-PrismattycUninstallKeys)
    if ($entries.Count -ne 0) { throw "ARP has $($entries.Count) Prismattyc entries after uninstall." }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $parts = @($userPath -split ';' | ForEach-Object { $_.Trim().TrimEnd('\') })
    if ($parts -contains $install.Bin) { throw 'User PATH still contains the install bin.' }
    if ($parts -contains $install.Cmd.TrimEnd('\')) { throw 'User PATH still contains the install cmd.' }
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

function Add-UserPathEntry {
    param([Parameter(Mandatory = $true)][string]$Entry)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $parts = @($userPath.Split([char[]]@(';'), [System.StringSplitOptions]::RemoveEmptyEntries))
    if ($parts -notcontains $Entry) {
        $parts = @($Entry) + $parts
        [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
    }
}

function Remove-UserPathEntry {
    param([Parameter(Mandatory = $true)][string]$Entry)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { return }
    $key = $Entry.Trim().TrimEnd('\')
    $parts = @($userPath.Split([char[]]@(';'), [System.StringSplitOptions]::RemoveEmptyEntries) | Where-Object { $_.Trim().TrimEnd('\') -ne $key })
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

function New-PreviewSweepFixtures {
    param($Install)
    $dead = Join-Path $Install.Root 'windows-preview-deadbeef'
    $keep = Join-Path $Install.Root 'not-a-preview'
    $sentinel = Join-Path ([System.IO.Path]::GetTempPath()) ('prismattyc-msi-junction-' + [guid]::NewGuid().ToString('n'))
    $junction = Join-Path $Install.Root 'windows-preview-junction'
    New-Item -ItemType Directory -Path $Install.Root -Force | Out-Null
    New-Item -ItemType Directory -Path $dead -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $dead 'marker.txt') -Value 'remove' -Encoding ascii
    New-Item -ItemType Directory -Path $keep -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $keep 'marker.txt') -Value 'keep' -Encoding ascii
    New-Item -ItemType Directory -Path $sentinel -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $sentinel 'sentinel.txt') -Value 'keep' -Encoding ascii
    if (Test-Path -LiteralPath $junction) {
        $existing = Get-Item -LiteralPath $junction -Force
        if (($existing.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            [System.IO.Directory]::Delete($junction)
        } else {
            throw "preview junction path is a real directory: $junction"
        }
    }
    New-Item -ItemType Junction -Path $junction -Target $sentinel | Out-Null
    $keepPath = 'C:\prismattyc-msi-path-keep'
    $dangling = Join-Path $Install.Root 'windows-preview-missing\bin'
    $previewBin = Join-Path $dead 'bin'
    $junctionBin = Join-Path $junction 'bin'
    foreach ($entry in @($previewBin, $dangling, $junctionBin, $keepPath, $Install.Bin)) {
        Add-UserPathEntry $entry
    }
    [pscustomobject]@{
        Dead        = $dead
        Keep        = $keep
        Sentinel    = $sentinel
        Junction    = $junction
        KeepPath    = $keepPath
        Dangling    = $dangling
        PreviewBin  = $previewBin
        JunctionBin = $junctionBin
    }
}

function Assert-PreviewSweep {
    param($Fixtures)
    if (Test-Path -LiteralPath $Fixtures.Dead) { throw "sweep left $($Fixtures.Dead)" }
    if (-not (Test-Path -LiteralPath (Join-Path $Fixtures.Keep 'marker.txt'))) {
        throw 'sweep removed not-a-preview'
    }
    if (-not (Test-Path -LiteralPath (Join-Path $Fixtures.Sentinel 'sentinel.txt'))) {
        throw 'sweep followed the preview junction'
    }
    if (-not (Test-Path -LiteralPath $Fixtures.Junction)) {
        throw 'sweep removed the preview junction'
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $parts = @($userPath -split ';' | Where-Object { $_ } | ForEach-Object { $_.Trim().TrimEnd('\') })
    if ($parts -notcontains $Fixtures.KeepPath) { throw "PATH lost $($Fixtures.KeepPath)" }
    if ($parts -contains $Fixtures.PreviewBin.TrimEnd('\')) { throw "PATH still has $($Fixtures.PreviewBin)" }
    if ($parts -contains $Fixtures.Dangling.TrimEnd('\')) { throw "PATH still has dangling $($Fixtures.Dangling)" }
    if ($parts -notcontains $Fixtures.JunctionBin.TrimEnd('\')) { throw "PATH lost the junction preview $($Fixtures.JunctionBin)" }
    Write-Output 'windows-preview-deadbeef removed'
    Write-Output 'not-a-preview kept'
    Write-Output 'prismattyc-msi-path-keep kept'
    Write-Output 'preview junction left in place'
}

function Remove-PreviewSweepFixtures {
    param($Fixtures, $Install)
    if (-not $Fixtures) { return }
    if ($Fixtures.Junction -and (Test-Path -LiteralPath $Fixtures.Junction)) {
        $item = Get-Item -LiteralPath $Fixtures.Junction -Force -ErrorAction SilentlyContinue
        if ($item -and (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)) {
            [System.IO.Directory]::Delete($Fixtures.Junction)
        }
    }
    foreach ($path in @($Fixtures.Sentinel, $Fixtures.Dead, $Fixtures.Keep)) {
        if ($path -and (Test-Path -LiteralPath $path)) {
            Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    foreach ($entry in @($Fixtures.PreviewBin, $Fixtures.Dangling, $Fixtures.JunctionBin, $Fixtures.KeepPath, $Install.Bin)) {
        if ($entry) { Remove-UserPathEntry $entry }
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
$fixtures = $null
try {
    $fixtures = New-PreviewSweepFixtures -Install $install
    if ($UpgradeFrom) {
        $older = (Resolve-Path -LiteralPath $UpgradeFrom).Path
        Invoke-Msi -ArgumentList (@('/i', $older) + $tail)
    }
    Invoke-Msi -ArgumentList $installArgs
    Assert-Installed -VersionText $ExpectedVersion -ProductVersion $ExpectedProductVersion -PathExpected ([bool]$AddToPath)
    Assert-PreviewSweep -Fixtures $fixtures
    Assert-LockedExecutableSurvivesReinstall -CommandLine (New-MsiCommandLine $installArgs)

    $installed = @(Get-PrismattycUninstallKeys)
    if ($installed.Count -ne 1) { throw "ARP has $($installed.Count) Prismattyc entries before uninstall." }
    Invoke-Msi -ArgumentList (@('/x', $installed[0].ProductCode, '/qn', '/norestart', 'REBOOT=ReallySuppress'))
    Assert-Removed -Survivors $preserved
    Assert-PreviewSweep -Fixtures $fixtures
    Write-Output ("MSI {0} checks passed." -f ((Get-MsiProofPhases) -join ', '))
} finally {
    Remove-PreviewSweepFixtures -Fixtures $fixtures -Install $install
}
