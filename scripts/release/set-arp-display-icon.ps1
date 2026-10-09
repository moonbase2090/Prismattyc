# Set DisplayIcon on an uninstall key that already exists.
# A missing key is not an install failure. The result line is for the MSI proof.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('HKCU', 'HKLM')][string]$Root,
    [Parameter(Mandatory = $true)][string]$ProductCode,
    [Parameter(Mandatory = $true)][string]$Icon
)
$ErrorActionPreference = 'Continue'
$path = "${Root}:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductCode"
$trace = Join-Path ([System.IO.Path]::GetTempPath()) "prismattyc-displayicon-$Root.txt"
$exists = Test-Path -LiteralPath $path
$line = "path=$path exists=$exists"
if ($exists) {
    try {
        Set-ItemProperty -LiteralPath $path -Name DisplayIcon -Value $Icon -ErrorAction Stop
        $line = "$line wrote=1"
    } catch {
        $line = "$line wrote=0 error=$($_.Exception.Message)"
    }
}
Set-Content -LiteralPath $trace -Value $line -Encoding ascii
exit 0
