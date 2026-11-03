# Windows lib-suite check: the workspace builds and passes clippy for every
# target, and each prismattyc-mux / prismattyc-host lib test finishes. A whole
# lib run on Windows used to sit with no result for 30+ minutes, so each test
# runs alone under a timeout, and the ones that hang are named.
#
#   powershell -ExecutionPolicy Bypass -File tests\native\windows-lib-suites.ps1 [-Repo DIR] [-Out DIR] [-TimeoutSec N] [-Crates a,b] [-Filter TEXT] [-SkipClippy]
#
# Run it from a checkout of the head under test on a clean Windows VM with the
# MSVC Rust toolchain. Exit 0 only if clippy is clean and no test fails or
# hangs. Results: OUT\result.txt (one line per test that did not pass, then a
# summary) and OUT\<crate>\<test>.log for each of those tests.
param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    [string]$Out = (Join-Path $env:TEMP "windows-lib-suites-$PID"),
    [int]$TimeoutSec = 120,
    [string[]]$Crates = @('prismattyc-mux', 'prismattyc-host'),
    [string]$Filter = '',
    [switch]$SkipClippy
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

New-Item -ItemType Directory -Path $Out -Force | Out-Null
$Out = (Resolve-Path $Out).Path
$result = Join-Path $Out 'result.txt'
Set-Content -Path $result -Value '' -Encoding ascii
$script:fails = 0
function Log([string]$m) { $line = "[lib-suites] $m"; Write-Output $line; Add-Content -Path $result -Value $line -Encoding ascii }
function Invoke-TimedTest(
    [string]$FilePath,
    [string]$Name,
    [int]$TimeoutSeconds,
    [string]$StdoutPath,
    [string]$StderrPath
) {
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = "$Name --exact --test-threads=1 --nocapture"
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    $stdout = [System.IO.File]::Open($StdoutPath, 'Create', 'Write', 'Read')
    $stderr = [System.IO.File]::Open($StderrPath, 'Create', 'Write', 'Read')
    try {
        [void]$process.Start()
        $stdoutCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdout)
        $stderrCopy = $process.StandardError.BaseStream.CopyToAsync($stderr)
        $timedOut = -not $process.WaitForExit($TimeoutSeconds * 1000)
        if ($timedOut) {
            # A hung test can own child processes, so terminate its whole tree.
            Start-Process -FilePath 'taskkill.exe' -Wait -WindowStyle Hidden `
                -ArgumentList @('/PID', $process.Id, '/T', '/F') | Out-Null
            if (-not $process.WaitForExit(5000)) { $process.Kill() }
        }
        $process.WaitForExit()
        [void]$stdoutCopy.GetAwaiter().GetResult()
        [void]$stderrCopy.GetAwaiter().GetResult()
        [pscustomobject]@{ TimedOut = $timedOut; ExitCode = $process.ExitCode }
    } finally {
        $stdout.Dispose()
        $stderr.Dispose()
        $process.Dispose()
    }
}

Push-Location $Repo
try {
    Log "head $(git rev-parse HEAD) rustc $((rustc --version) -join ' ')"

    if (-not $SkipClippy) {
        # Merge Cargo's streams before PowerShell 5.1 can turn stderr progress into NativeCommandError.
        & $env:ComSpec /d /c 'cargo clippy --locked --workspace --all-targets -- -D warnings 2>&1' |
            Tee-Object -FilePath (Join-Path $Out 'clippy.log') | Out-Host
        $clippyExitCode = $LASTEXITCODE
        if ($clippyExitCode -ne 0) { Log 'FAIL: clippy --workspace --all-targets -D warnings'; $script:fails++ }
        else { Log 'PASS: clippy --workspace --all-targets -D warnings' }
    }

    foreach ($crate in $Crates) {
        # The lib test executable comes from cargo's JSON artifact messages.
        $exe = $null
        $crateFile = $crate -replace '[^A-Za-z0-9_.-]', '_'
        $buildJson = Join-Path $Out "$crateFile-test-build.jsonl"
        $buildLog = Join-Path $Out "$crateFile-test-build.log"
        $build = Start-Process -FilePath 'cargo' -Wait -PassThru -NoNewWindow `
            -ArgumentList @('test', '--locked', '-p', $crate, '--lib', '--no-run', '--message-format=json') `
            -RedirectStandardOutput $buildJson -RedirectStandardError $buildLog
        Get-Content -Path $buildJson | ForEach-Object {
            if ($_ -notmatch '^\{') { return }
            $msg = $_ | ConvertFrom-Json
            if ($msg.reason -eq 'compiler-artifact' -and $msg.profile.test -and $msg.executable -and
                ($msg.target.kind -contains 'lib')) { $exe = $msg.executable }
        }
        if ($build.ExitCode -ne 0 -or -not $exe) {
            Get-Content -Path $buildLog | Out-Host
            Log "FAIL: $crate lib tests did not build"
            $script:fails++
            continue
        }
        Remove-Item $buildJson, $buildLog -ErrorAction SilentlyContinue

        $tests = & $exe --list --format terse |
            Where-Object { $_ -match ': test$' } |
            ForEach-Object { $_ -replace ': test$', '' } |
            Where-Object { -not $Filter -or $_ -like "*$Filter*" }
        $logs = Join-Path $Out $crate
        New-Item -ItemType Directory -Path $logs -Force | Out-Null
        $pass = 0; $fail = 0; $hang = 0; $ignored = 0
        foreach ($name in $tests) {
            $log = Join-Path $logs (($name -replace '[^A-Za-z0-9_.-]', '_') + '.log')
            $run = Invoke-TimedTest -FilePath $exe -Name $name -TimeoutSeconds $TimeoutSec `
                -StdoutPath $log -StderrPath "$log.err"
            if ($run.TimedOut) {
                $hang++; Log "HANG: $crate $name (over ${TimeoutSec}s)"
                continue
            }
            if ($run.ExitCode -ne 0) { $fail++; Log "FAIL: $crate $name (exit $($run.ExitCode))"; continue }
            if (Select-String -Path $log -Pattern '1 ignored' -Quiet) { $ignored++ } else { $pass++ }
            Remove-Item $log, "$log.err" -ErrorAction SilentlyContinue
        }
        Log "$crate lib: $pass passed, $fail failed, $hang hung, $ignored ignored (of $(@($tests).Count))"
        $script:fails += $fail + $hang
    }
} finally {
    Pop-Location
}

if ($script:fails -eq 0) { Log 'RESULT: PASS'; exit 0 }
Log "RESULT: FAIL ($script:fails)"
exit 1
