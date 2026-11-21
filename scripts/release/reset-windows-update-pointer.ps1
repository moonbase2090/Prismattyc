# Drop the pmux update forwarding pointer after an MSI install.
# prepare-windows-install.ps1 already clears the same file and stops a stale
# daemon before the files are copied. This script repeats the clear after the
# files are in place and does not stop or restart any process.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

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

$pointer = Join-Path (Get-PrismattycUpdateRoot) 'windows-current.json'
if (Test-Path -LiteralPath $pointer) {
    Remove-Item -LiteralPath $pointer -Force
}
Write-Host 'Prismattyc update pointer cleared; running programs were left alone.'
