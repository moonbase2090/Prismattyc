[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Output = 'build/windows-release',
    [string]$Version = '',
    [switch]$SkipBuild,
    [switch]$SkipPackage,
    [switch]$Signed
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$previousRustFlags = $env:RUSTFLAGS
Push-Location $repoRoot
try {
    if (-not $IsWindows -and $env:OS -ne 'Windows_NT') { throw 'Run this script on native Windows.' }
    if ($SkipBuild -and $SkipPackage) { throw 'Choose a build, a package, or both.' }
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    $binDir = Join-Path $targetRoot "$Target/release"
    if (-not $SkipBuild) {
        if (Test-Path $Output) { throw "Output already exists: $Output" }
        rustup toolchain install 1.90.0 --profile minimal
        if ($LASTEXITCODE -ne 0) { throw 'Rust toolchain installation failed.' }
        rustup target add --toolchain 1.90.0 $Target
        if ($LASTEXITCODE -ne 0) { throw 'Rust target installation failed.' }
        $env:RUSTFLAGS = "$previousRustFlags -C target-feature=+crt-static".Trim()
        cargo +1.90.0 build --locked --release --target $Target --workspace --bins
        if ($LASTEXITCODE -ne 0) { throw 'Windows build failed.' }
    }
    if ($SkipPackage) {
        Write-Host "Windows binaries staged in $binDir"
        return
    }
    if (Test-Path $Output) { throw "Output already exists: $Output" }
    if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
        dotnet tool install --global wix --version 5.0.2
        if ($LASTEXITCODE -ne 0) { throw 'WiX installation failed.' }
        $env:PATH = "$(Join-Path $env:USERPROFILE '.dotnet\tools');$env:PATH"
    }
    $metadata = cargo +1.90.0 metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $Version = ($metadata.packages | Where-Object { $_.name -eq 'prismattyc-host' }).version
    }
    $packageArgs = @('scripts/release/package-windows.py', '--version', $Version, '--target', $Target, '--bin-dir', $binDir, '--out', $Output)
    if ($Signed) { $packageArgs += '--signed' }
    python @packageArgs
    if ($LASTEXITCODE -ne 0) { throw 'Windows packaging failed.' }
    if ($Signed) {
        Write-Host 'Windows code signing: executables were signed before packaging. Sign the MSI next, then refresh checksums.'
    } else {
        Write-Host 'Windows code signing skipped: Azure Artifact Signing settings are absent.'
    }
    Write-Host "Windows package: $((Resolve-Path $Output).Path)"
} finally {
    $env:RUSTFLAGS = $previousRustFlags
    Pop-Location
}
