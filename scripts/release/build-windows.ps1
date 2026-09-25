[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Output = 'build/windows-release'
)
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$previousRustFlags = $env:RUSTFLAGS
Push-Location $repoRoot
try {
    if (-not $IsWindows -and $env:OS -ne 'Windows_NT') { throw 'Run this script on native Windows.' }
    if (Test-Path $Output) { throw "Output already exists: $Output" }
    rustup toolchain install 1.90.0 --profile minimal
    if ($LASTEXITCODE -ne 0) { throw 'Rust toolchain installation failed.' }
    rustup target add --toolchain 1.90.0 $Target
    if ($LASTEXITCODE -ne 0) { throw 'Rust target installation failed.' }
    $env:RUSTFLAGS = "$previousRustFlags -C target-feature=+crt-static".Trim()
    cargo +1.90.0 build --locked --release --target $Target --workspace --bins
    if ($LASTEXITCODE -ne 0) { throw 'Windows build failed.' }
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    $binDir = Join-Path $targetRoot "$Target/release"
    $metadata = cargo +1.90.0 metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
    $version = ($metadata.packages | Where-Object { $_.name -eq 'prismattyc-host' }).version
    python scripts/release/package-windows.py --version $version --target $Target --bin-dir $binDir --out $Output
    if ($LASTEXITCODE -ne 0) { throw 'Windows packaging failed.' }
    Write-Host "Windows package: $((Resolve-Path $Output).Path)"
} finally {
    $env:RUSTFLAGS = $previousRustFlags
    Pop-Location
}
