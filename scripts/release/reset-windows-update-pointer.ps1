# Drop the pmux update forwarding pointer after an MSI install.
# The next launch of the installed executables runs those files. This script
# does not stop or restart any process. A running daemon keeps its image.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$localAppData = [Environment]::GetFolderPath('LocalApplicationData')
if (-not $localAppData) { throw 'LocalApplicationData is unset.' }
$state = Join-Path $localAppData 'prismattyc\updates\windows-current.json'
if (Test-Path -LiteralPath $state) {
    Remove-Item -LiteralPath $state -Force
}
Write-Host "Prismattyc update pointer cleared; running programs were left alone."
