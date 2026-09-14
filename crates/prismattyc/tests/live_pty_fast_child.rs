//! Live outer-PTY regressions for the host event/paint loop (matrix F2/F4).
//!
//! Fast-exit children must still paint before host teardown (EOF-before-paint).

#![cfg(unix)]

use std::io::Read;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn run_prism_under_pty(program_and_args: &[&str], timeout: Duration) -> (u32, String) {
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open outer pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prismattyc"));
    for a in program_and_args {
        cmd.arg(a);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn prism under pty");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut collected = Vec::new();
    let mut buf = [0_u8; 8192];
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => collected.extend_from_slice(&buf[..n]),
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("pty read: {error}"),
        }
        if child.try_wait().ok().flatten().is_some() {
            std::thread::sleep(Duration::from_millis(40));
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&buf[..n]);
            }
            break;
        }
    }
    let status = child.wait().expect("wait prism");
    (
        status.exit_code(),
        String::from_utf8_lossy(&collected).into_owned(),
    )
}

#[test]
fn real_binary_printf_transcript_contains_output() {
    let (code, out) =
        run_prism_under_pty(&["/usr/bin/printf", "HELLO_PHASE1"], Duration::from_secs(5));
    assert_eq!(code, 0, "prism exit non-zero; transcript={out:?}");
    assert!(
        out.contains("HELLO_PHASE1"),
        "fast-child output missing from host transcript (EOF-before-paint?): {out:?}"
    );
}

#[test]
fn real_binary_alt_screen_child_output_appears() {
    // Exit while still on alt so the final paint is the alt buffer.
    let seq = "\u{1b}[?1049hALT!";
    let (code, out) = run_prism_under_pty(&["/usr/bin/printf", seq], Duration::from_secs(5));
    assert_eq!(code, 0, "prism exit non-zero; transcript={out:?}");
    assert!(
        out.contains("ALT!"),
        "alt-screen child output missing from transcript: {out:?}"
    );
}

#[test]
fn real_binary_shell_c_printf_transcript_contains_output() {
    let (code, out) = run_prism_under_pty(
        &["/bin/sh", "-c", "printf HELLO_SHELL"],
        Duration::from_secs(5),
    );
    assert_eq!(code, 0, "prism exit non-zero; transcript={out:?}");
    assert!(
        out.contains("HELLO_SHELL"),
        "shell -c printf missing from transcript: {out:?}"
    );
}

/// `prism -- /usr/bin/printf …` must treat the path as the child program (not $SHELL script).
#[test]
fn real_binary_double_dash_printf_transcript() {
    let (code, out) = run_prism_under_pty(
        &["--", "/usr/bin/printf", "HELLO_DASH"],
        Duration::from_secs(5),
    );
    assert_eq!(code, 0, "prism exit non-zero; transcript={out:?}");
    assert!(
        out.contains("HELLO_DASH"),
        "`prism -- printf` missing HELLO_DASH (CLI -- / EOF paint?): {out:?}"
    );
    assert!(
        !out.contains("cannot execute binary file"),
        "double-dash must not hand the binary path to $SHELL as a script: {out:?}"
    );
}

// flood: continuous child output (`yes`) must not starve cooperative
/// signal exit. SIGTERM is delivered only to prism (not the flood child); prism
/// must exit promptly so `TerminalGuard` Drop restores the outer TTY.
///
/// Bound is wall-clock (2s, matching the codex harness). If CI is flaky under
/// heavy load, prefer the deterministic unit tests
/// `pty_drain_observes_signal_under_continuous_flood` and
/// `pty_drain_budget_yields_before_queue_empty` in `main.rs`.
#[test]
fn real_binary_sigterm_exits_under_pty_flood() {
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open outer pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prismattyc"));
    // Prefer GNU yes; fall back to a tight shell writer if missing.
    let yes = if std::path::Path::new("/usr/bin/yes").is_file() {
        "/usr/bin/yes"
    } else {
        "/bin/sh"
    };
    if yes == "/bin/sh" {
        cmd.arg("-c");
        cmd.arg("while :; do printf y; done");
    } else {
        cmd.arg(yes);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn prism under pty with flood child");
    drop(pair.slave);

    // Keep master open and drain output so the outer PTY does not block prism.
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let drain = std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // Let the flood fill from-PTY queue and enter the drain loop.
    std::thread::sleep(Duration::from_millis(200));
    let pid = child.process_id().expect("prism process id for SIGTERM");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM prism");
    assert!(status.success(), "kill -TERM failed: {status}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut exited = false;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !exited {
        // Fail closed: force-kill so the suite does not hang, then assert.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        let _ = child.wait();
        let _ = drain.join();
        panic!(
            "prism still alive >2s after SIGTERM under continuous PTY flood \
             (drain starvation; pid={pid})"
        );
    }
    let _ = drain.join();
}
