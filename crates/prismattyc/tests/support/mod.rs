//! Nested outer-PTY helpers for host UX integration tests.
//!
//! Prismattyc runs under a real PTY (like a user terminal). The harness writes
//! keyboard/mouse encodings to the master and scrapes the transcript for
//! painted markers (child text, find chrome, OSC title).
//!
//! **Not covered here:** outer-host chord theft (Kitty steals Ctrl+Shift+F) —
//! keep those on the human dogfood matrix.
//!
//! Include from an integration test with:
//! ```ignore
//! #[path = "support/mod.rs"]
//! mod support;
//! ```

#![allow(dead_code)] // helpers used selectively per test file

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Default geometry for dogfood-scale UX scripts.
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Interactive Prismattyc session under an outer PTY.
pub struct PtyUx {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    transcript: Arc<Mutex<Vec<u8>>>,
    /// Join handle for the drain thread (dropped on kill).
    _drain: Option<std::thread::JoinHandle<()>>,
}

impl PtyUx {
    /// Spawn `prism` with `args` (program + argv for the child, not including prism).
    pub fn spawn(args: &[&str]) -> Self {
        Self::spawn_sized(args, DEFAULT_ROWS, DEFAULT_COLS)
    }

    pub fn spawn_sized(args: &[&str], rows: u16, cols: u16) -> Self {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open outer pty");

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prismattyc"));
        // Quiet scroll title noise can stay on — tests assert on it.
        for a in args {
            cmd.arg(a);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("spawn prism under outer pty");
        drop(pair.slave);

        let transcript = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        let buf_tx = Arc::clone(&transcript);
        let drain = std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut g) = buf_tx.lock() {
                            g.extend_from_slice(&chunk[..n]);
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(5) => break, // EIO: slave closed
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let writer = pair.master.take_writer().expect("pty writer");
        Self {
            child,
            writer,
            transcript,
            _drain: Some(drain),
        }
    }

    /// `prism -- /bin/sh -c <script>` with a long-lived sleep so host UX can run.
    pub fn spawn_fixture_script(script: &str) -> Self {
        // Keep the child alive after printing so Prismattyc's host loop stays up.
        let wrapped = format!("{script}\n# hold open for host UX\nsleep 120\n");
        Self::spawn(&["--", "/bin/sh", "-c", &wrapped])
    }

    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .expect("write to outer pty master");
        let _ = self.writer.flush();
    }

    pub fn write_str(&mut self, s: &str) {
        self.write_raw(s.as_bytes());
    }

    /// Type printable characters as individual keypresses (no modifiers).
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            self.write_raw(s.as_bytes());
            // Tiny gap so Prismattyc's poll loop can process each key.
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ctrl+Shift+`;` — Kitty-safe find chord (see README / dogfood notes).
    pub fn open_find(&mut self) {
        self.write_raw(&kitty_key(';' as u32, true, false, true));
    }

    pub fn key_enter(&mut self) {
        self.write_raw(b"\r");
    }

    pub fn key_shift_enter(&mut self) {
        // Kitty: Enter = codepoint 13
        self.write_raw(&kitty_key(13, true, false, false));
    }

    pub fn key_esc(&mut self) {
        self.write_raw(b"\x1b");
    }

    pub fn key_shift_page_up(&mut self) {
        self.write_raw(b"\x1b[5;2~");
    }

    pub fn key_shift_page_down(&mut self) {
        self.write_raw(b"\x1b[6;2~");
    }

    pub fn key_shift_home(&mut self) {
        self.write_raw(b"\x1b[1;2H");
    }

    pub fn key_shift_end(&mut self) {
        self.write_raw(b"\x1b[1;2F");
    }

    /// SGR mouse wheel up at 1-based cell (col, row). Requires Prismattyc mouse capture.
    pub fn mouse_wheel_up(&mut self, col: u16, row: u16) {
        // Button 64 = wheel up (xterm SGR).
        let seq = format!("\x1b[<64;{col};{row}M");
        self.write_raw(seq.as_bytes());
    }

    pub fn mouse_wheel_down(&mut self, col: u16, row: u16) {
        let seq = format!("\x1b[<65;{col};{row}M");
        self.write_raw(seq.as_bytes());
    }

    pub fn transcript_bytes(&self) -> Vec<u8> {
        self.transcript.lock().expect("transcript lock").clone()
    }

    pub fn transcript(&self) -> String {
        String::from_utf8_lossy(&self.transcript_bytes()).into_owned()
    }

    pub fn transcript_stripped(&self) -> String {
        strip_ansi(&self.transcript())
    }

    /// Block until `needle` appears in the raw transcript (ANSI retained).
    pub fn wait_for(&self, needle: &str, timeout: Duration) {
        self.wait_until(
            |t| t.contains(needle),
            timeout,
            &format!("substring {needle:?}"),
        );
    }

    /// Block until `needle` appears after crude ANSI stripping.
    pub fn wait_for_stripped(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let t = self.transcript_stripped();
            if t.contains(needle) {
                return;
            }
            if Instant::now() > deadline {
                let raw = self.transcript();
                let stripped = self.transcript_stripped();
                panic!(
                    "timeout waiting for stripped {needle:?}\n--- stripped (tail) ---\n{}\n--- raw (tail) ---\n{}",
                    tail(&stripped, 800),
                    tail(&raw, 800)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_until(&self, mut pred: impl FnMut(&str) -> bool, timeout: Duration, label: &str) {
        let deadline = Instant::now() + timeout;
        loop {
            let t = self.transcript();
            if pred(&t) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "timeout waiting for {label}\n--- transcript tail ---\n{}",
                    tail(&t, 1200)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// True once the child has been reaped (or never needed kill).
    fn already_reaped(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Best-effort terminate; integration tests should not hang the suite.
    ///
    /// Idempotent: reaps first so Drop after a natural exit does not signal a
    /// recycled PID (dual-sign review note on #37).
    pub fn kill(&mut self) {
        // Already exited — do not signal whatever now holds that pid.
        if self.already_reaped() {
            return;
        }
        let Some(pid) = self.child.process_id() else {
            let _ = self.child.wait();
            return;
        };
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.already_reaped() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Re-check before KILL — process may have exited mid-wait.
        if self.already_reaped() {
            return;
        }
        if let Some(pid) = self.child.process_id() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let _ = self.child.wait();
    }
}

impl Drop for PtyUx {
    fn drop(&mut self) {
        // try_wait first via kill() — never signal a reaped/recycled pid.
        self.kill();
    }
}

/// Kitty keyboard protocol key press: `CSI codepoint ; modifier u`
///
/// `modifier = 1 + Shift + 2·Alt + 4·Ctrl` (Kitty encoding).
pub fn kitty_key(codepoint: u32, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let modifier = 1u32 + u32::from(shift) + 2 * u32::from(alt) + 4 * u32::from(ctrl);
    format!("\x1b[{codepoint};{modifier}u").into_bytes()
}

/// Strip common CSI / OSC sequences for substring asserts on visible text.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            // Keep printable + newline/tab; drop other C0.
            if c == '\n' || c == '\r' || c == '\t' || !c.is_control() {
                out.push(c);
            }
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                // CSI: read until final byte 0x40–0x7E
                for d in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: BEL or ST (ESC \)
                chars.next();
                while let Some(d) = chars.next() {
                    if d == '\u{07}' {
                        break;
                    }
                    if d == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some('(') | Some(')') | Some('*') | Some('+') => {
                // charset designate: ESC ( B etc.
                chars.next();
                let _ = chars.next();
            }
            Some(_) => {
                // short ESC seq: skip next char
                let _ = chars.next();
            }
            None => {}
        }
    }
    out
}

fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().skip(n - max).collect()
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn strip_ansi_keeps_text() {
        let s = "\u{1b}[7m Find: hello█\u{1b}[0m";
        assert!(strip_ansi(s).contains("Find: hello"));
    }

    #[test]
    fn kitty_ctrl_shift_semi() {
        assert_eq!(kitty_key(';' as u32, true, false, true), b"\x1b[59;6u");
    }
}
