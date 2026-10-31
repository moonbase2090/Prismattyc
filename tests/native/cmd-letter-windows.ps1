# PR #396 Windows check: Ctrl+C must still interrupt, with or without a
# selection, in a shell pane and under an agent-named foreground process, and
# plain typing must still reach the pane. Drives a real prismattyc-host window
# with SendInput-level key and mouse events.
#
#   powershell -ExecutionPolicy Bypass -File cmd-letter-windows.ps1 -Bins DIR [-Out DIR]
#
# DIR holds prismattyc-host.exe, pmux.exe and pmuxd.exe built from the head
# under test. Run it in an interactive desktop session (RDP or console), not
# over SSH: input injection needs an unlocked desktop. Exit 0 only if every
# check passes. Results: OUT\result.txt.
#
# Win+letter is not checked: Windows reserves most Win+letter chords (Win+C,
# Win+V and others), so they never reach the app.
param(
    [Parameter(Mandatory = $true)][string]$Bins,
    [string]$Out = (Join-Path $env:TEMP "cmd-letter-$PID")
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$hostExe = Join-Path $Bins 'prismattyc-host.exe'
foreach ($bin in 'prismattyc-host.exe', 'pmuxd.exe') {
    if (-not (Test-Path (Join-Path $Bins $bin))) { throw "missing $bin in $Bins" }
}
New-Item -ItemType Directory -Path $Out -Force | Out-Null
$Out = (Resolve-Path $Out).Path
$result = Join-Path $Out 'result.txt'
Set-Content -Path $result -Value '' -Encoding ascii
$script:fails = 0
function Log([string]$m) { $line = "[cmd-letter] $m"; Write-Output $line; Add-Content -Path $result -Value $line -Encoding ascii }
function Pass([string]$m) { Log "PASS: $m" }
function Fail([string]$m) { Log "FAIL: $m"; $script:fails++ }

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Input {
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] static extern short VkKeyScan(char ch);
    [DllImport("user32.dll")] static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    const uint KEYUP = 2;
    public const byte SHIFT = 0x10, CTRL = 0x11, RETURN = 0x0D;
    public static void Down(byte vk) { keybd_event(vk, 0, 0, UIntPtr.Zero); }
    public static void Up(byte vk) { keybd_event(vk, 0, KEYUP, UIntPtr.Zero); }
    public static void Tap(byte vk) { Down(vk); System.Threading.Thread.Sleep(20); Up(vk); }
    public static void Chord(byte mod, byte vk) { Down(mod); Tap(vk); Up(mod); }
    public static void Type(string text) {
        foreach (char ch in text) {
            short scan = VkKeyScan(ch);
            if (scan == -1) throw new ArgumentException("no key for " + ch);
            byte vk = (byte)(scan & 0xff);
            bool shift = (scan & 0x100) != 0;
            if (shift) Down(SHIFT);
            Tap(vk);
            if (shift) Up(SHIFT);
            System.Threading.Thread.Sleep(15);
        }
    }
    public static void Drag(int x1, int y1, int x2, int y2) {
        SetCursorPos(x1, y1); mouse_event(2, 0, 0, 0, UIntPtr.Zero);   // left down
        for (int i = 1; i <= 10; i++) { SetCursorPos(x1 + (x2 - x1) * i / 10, y1 + (y2 - y1) * i / 10); System.Threading.Thread.Sleep(20); }
        mouse_event(4, 0, 0, 0, UIntPtr.Zero);                          // left up
    }
}
'@

function Focus-Host {
    $p = Get-Process -Id $script:hostProc.Id -ErrorAction SilentlyContinue
    if (-not $p -or $p.MainWindowHandle -eq [IntPtr]::Zero) { return $false }
    [Input]::ShowWindow($p.MainWindowHandle, 9) | Out-Null
    [Input]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
    Start-Sleep -Milliseconds 400
    return $true
}
function Run-Line([string]$line) { Focus-Host | Out-Null; [Input]::Type($line); [Input]::Tap([Input]::RETURN) }
function Ctrl-C { Focus-Host | Out-Null; [Input]::Chord([Input]::CTRL, 0x43) }
function Path-In([string]$name) { "'" + (Join-Path $Out $name) + "'" }

# Ctrl+C while a foreground command runs: the command must stop and the prompt
# must return. $before runs after the command starts (for example, a selection).
function Check-Interrupt([string]$label, [string]$command, [scriptblock]$before) {
    $int = Join-Path $Out "int-$label.txt"; $alive = Join-Path $Out "alive-$label.txt"
    Remove-Item $int, $alive -ErrorAction SilentlyContinue
    Run-Line "$command; 'NOT_INTERRUPTED' | Out-File -Encoding ascii $(Path-In "int-$label.txt")"
    Start-Sleep -Seconds 3
    if ($before) { & $before }
    Ctrl-C
    Start-Sleep -Seconds 1
    Run-Line "'ALIVE' | Out-File -Encoding ascii $(Path-In "alive-$label.txt")"
    Start-Sleep -Seconds 3
    if ((Test-Path $alive) -and -not (Test-Path $int)) { Pass "${label}: Ctrl+C interrupted; prompt came back" }
    else { Fail "${label}: Ctrl+C did not interrupt (alive=$(Test-Path $alive), int=$(Test-Path $int))" }
}

# --- host --------------------------------------------------------------------
$cfg = Join-Path $Out 'config.toml'
Set-Content -Path $cfg -Encoding ascii -Value "automatic_update_checks = false`nspace_startup = `"fresh`"`nsession_naming = `"auto`"`n"
$env:PRISMATTYC_CONFIG = $cfg
$env:PRISMATTYC_BELL_TOASTER = '0'
$env:PATH = "$Bins;$env:PATH"
$shell = (Get-Command powershell.exe).Source
$script:hostProc = Start-Process -FilePath $hostExe -ArgumentList @('--', $shell, '-NoLogo', '-NoProfile') `
    -RedirectStandardError (Join-Path $Out 'host.log') -RedirectStandardOutput (Join-Path $Out 'host.out') -PassThru
$deadline = (Get-Date).AddSeconds(30)
while (-not (Focus-Host)) {
    if ($script:hostProc.HasExited) { Fail 'host exited at startup (see host.log)'; exit 1 }
    if ((Get-Date) -gt $deadline) { Fail 'host window did not appear within 30 s'; exit 1 }
    Start-Sleep -Milliseconds 500
}
Start-Sleep -Seconds 4
$version = (& $hostExe --version 2>$null | Select-Object -First 1)
Log "host=$version os=$([Environment]::OSVersion.VersionString) arch=$env:PROCESSOR_ARCHITECTURE"

try {
    # 1. Plain typing reaches the pane.
    $typed = Join-Path $Out 'typed.txt'
    Run-Line "'TYPED' | Out-File -Encoding ascii $(Path-In 'typed.txt')"
    Start-Sleep -Seconds 2
    $got = if (Test-Path $typed) { (Get-Content $typed -Raw).Trim() } else { '(not written)' }
    if ($got -eq 'TYPED') { Pass "plain typing reaches the pane" } else { Fail "plain typing: expected TYPED, got $got" }

    # 2. Ctrl+C in a shell pane.
    Check-Interrupt 'shell' 'Start-Sleep -Seconds 20' $null

    # 3. Ctrl+C with a multi-cell selection: smart_ctrl_c is off by default, so
    #    it must still interrupt instead of copying.
    Run-Line "'SELECT-ME SELECT-ME SELECT-ME SELECT-ME'"
    Start-Sleep -Seconds 1
    $selectDrag = {
        $p = Get-Process -Id $script:hostProc.Id
        $r = New-Object Input+RECT
        [Input]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
        $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
        Focus-Host | Out-Null
        [Input]::Drag($r.Left + [int]($w * 0.10), $r.Top + [int]($h * 0.35), $r.Left + [int]($w * 0.70), $r.Top + [int]($h * 0.45))
        Start-Sleep -Milliseconds 300
    }
    Check-Interrupt 'selection' 'Start-Sleep -Seconds 20' $selectDrag

    # 4. Ctrl+C under an agent-named foreground process: a copy of ping.exe named
    #    claude.exe, which runs until interrupted.
    $agentDir = Join-Path $Out 'agentsim'
    New-Item -ItemType Directory -Path $agentDir -Force | Out-Null
    Copy-Item (Join-Path $env:SystemRoot 'System32\PING.EXE') (Join-Path $agentDir 'claude.exe') -Force
    Check-Interrupt 'agent' "& '$(Join-Path $agentDir 'claude.exe')' -t 127.0.0.1 | Out-Null" $null
    if (Get-Process -Name claude -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$agentDir*" }) {
        Fail 'agent: claude.exe still running after Ctrl+C'
        Get-Process -Name claude | Where-Object { $_.Path -like "$agentDir*" } | Stop-Process -Force
    } else { Pass 'agent: claude.exe exited' }

    # Late check: interrupted commands must not have finished after all.
    Start-Sleep -Seconds 18
    $late = @('shell', 'selection', 'agent') | Where-Object { Test-Path (Join-Path $Out "int-$_.txt") }
    if ($late) { Fail "interrupted command completed later: $($late -join ', ')" }
    else { Pass 'no interrupted command completed later' }
} finally {
    if (-not $script:hostProc.HasExited) { Stop-Process -Id $script:hostProc.Id -Force -ErrorAction SilentlyContinue }
}

Log "RESULT: $(if ($script:fails -eq 0) { 'PASS' } else { "FAIL ($($script:fails))" })"
exit [int]($script:fails -gt 0)
