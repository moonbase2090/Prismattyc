# Build an older MSI from the packaged payload and run the per-user install test.
# The MSI paths contain a space. Proof output names versions, not machine paths.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PackageDir,
    [string]$ProofPath = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Run this proof on Windows.' }

$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$package = (Resolve-Path -LiteralPath $PackageDir).Path
$manifestPath = Get-ChildItem -LiteralPath $package -Filter 'manifest-x86_64-pc-windows-msvc.json' | Select-Object -First 1
if (-not $manifestPath) { throw 'Windows manifest is missing.' }
$manifest = Get-Content -LiteralPath $manifestPath.FullName -Raw | ConvertFrom-Json
$parts = "$($manifest.msi_product_version)".Split('.')
if ($parts.Count -ne 3) { throw "unexpected MSI product version $($manifest.msi_product_version)" }
$third = [int]$parts[2]
if ($third -le 0) { throw 'MSI product version has no older value' }
$olderProduct = '{0}.{1}.{2}' -f $parts[0], $parts[1], ($third - 1)

$zip = Get-ChildItem -LiteralPath $package -Filter 'prismattyc-*-x86_64-pc-windows-msvc.zip' | Select-Object -First 1
$shipped = Get-ChildItem -LiteralPath $package -Filter 'prismattyc-*-x86_64-pc-windows-msvc.msi' | Select-Object -First 1
if (-not $zip -or -not $shipped) { throw 'Windows zip or MSI is missing.' }

$wix = Join-Path $env:USERPROFILE '.dotnet\tools\wix.exe'
if (-not (Test-Path -LiteralPath $wix)) {
    $found = Get-Command wix -ErrorAction SilentlyContinue
    if (-not $found) { throw 'WiX 5.0.2 is not on PATH.' }
    $wix = $found.Source
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("prismattyc-msi-proof-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    Expand-Archive -LiteralPath $zip.FullName -DestinationPath $stage
    $payload = Get-ChildItem -LiteralPath $stage -Directory |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin\pmux.exe') } |
        Select-Object -First 1
    if (-not $payload) { throw 'packaged zip has no bin\pmux.exe' }
    $olderBuilt = Join-Path $stage 'older.msi'
    & $wix build (Join-Path $repo 'scripts\release\prismattyc.wxs') -arch x64 `
        -d "ProductVersion=$olderProduct" `
        -d "SourceVersion=$($manifest.version)" `
        -d "Payload=$($payload.FullName)" `
        -d "Repo=$repo" `
        -o $olderBuilt
    if ($LASTEXITCODE -ne 0) { throw "older MSI build exited $LASTEXITCODE" }

    $proofDir = Join-Path ([System.IO.Path]::GetTempPath()) 'MSI Proof'
    if ($proofDir -notmatch ' ') { throw 'proof directory must contain a space' }
    New-Item -ItemType Directory -Path $proofDir -Force | Out-Null
    $olderCopy = Join-Path $proofDir 'older.msi'
    $newCopy = Join-Path $proofDir 'new.msi'
    Copy-Item -LiteralPath $olderBuilt -Destination $olderCopy -Force
    Copy-Item -LiteralPath $shipped.FullName -Destination $newCopy -Force

    $script = Join-Path $repo 'scripts\release\test-windows-msi.ps1'
    $output = & $script -Msi $newCopy -UpgradeFrom $olderCopy -ExpectedVersion $manifest.version -ExpectedProductVersion $manifest.msi_product_version 2>&1
    $text = @(
        "version=$($manifest.version)"
        "product_version=$($manifest.msi_product_version)"
        "older_product_version=$olderProduct"
        "msi_name=$($shipped.Name)"
        'path_has_space=true'
        ($output | Out-String).TrimEnd()
    ) -join "`n"
    Write-Host $text
    if ($ProofPath) {
        $proofParent = Split-Path -Parent $ProofPath
        if ($proofParent) { New-Item -ItemType Directory -Path $proofParent -Force | Out-Null }
        Set-Content -LiteralPath $ProofPath -Value ($text + "`n") -Encoding ascii
    }
} finally {
    Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
}
