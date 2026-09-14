//! Desktop notifications for the windowed host (PT-39).
//!
//! Spawn-and-forget over the platform CLI: `notify-send` on Linux/BSD,
//! `osascript` on macOS. Every failure — missing binary, no notification
//! daemon, no session bus — is swallowed, so a bell can never take the
//! host down. Every spawned child is reaped on a worker thread, so a bell
//! can never leave a zombie either.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

/// Raise an OS notification for a terminal BEL. `tab` names the tab that
/// rang when known.
pub(crate) fn bell(tab: Option<&str>) {
    let body = match tab {
        Some(title) if !title.is_empty() => format!("Bell in tab \"{title}\""),
        _ => "Bell in a pane".to_string(),
    };
    let (program, args) = notify_argv("Prismattyc", &body);
    let mut command = Command::new(program);
    command.args(args);
    let _ = spawn_detached(&mut command);
}

/// Raise an OS notification for an agent attention signal.
pub(crate) fn attention(title: &str, body: &str) {
    let (program, args) = notify_argv(title, body);
    let mut command = Command::new(program);
    command.args(args);
    let _ = spawn_detached(&mut command);
}

/// Platform notification command line. Pure, so tests never launch a real
/// notification.
#[cfg(not(target_os = "macos"))]
fn notify_argv(title: &str, body: &str) -> (&'static str, Vec<String>) {
    ("notify-send", vec![title.to_string(), body.to_string()])
}

/// Platform notification command line. Pure, so tests never launch a real
/// notification.
#[cfg(target_os = "macos")]
fn notify_argv(title: &str, body: &str) -> (&'static str, Vec<String>) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape_applescript(body),
        escape_applescript(title)
    );
    ("osascript", vec!["-e".to_string(), script])
}

/// Spawn detached with a reaper thread: the child is always `wait()`ed, so
/// Unix zombies cannot accumulate no matter how the child exits.
fn spawn_detached(command: &mut Command) -> bool {
    match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// The bundled "Zen" bell: 528 Hz + octave partial, long calm decay
/// (operator pick, PT-39). 16-bit mono PCM WAV, synthesized — no external
/// asset, and identical on every machine.
const ZEN_WAV: &[u8] = include_bytes!("../assets/bell-zen.wav");

/// Materialize the bundled cue once per process so player CLIs (which want
/// a path, not a pipe) can read it. A stale or partial file is rewritten.
fn zen_wav_path() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join("prismattyc-bell-zen.wav");
        let fresh = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == ZEN_WAV.len() as u64);
        if !fresh && std::fs::write(&path, ZEN_WAV).is_err() {
            return None;
        }
        Some(path)
    })
    .clone()
}

/// Play the bundled bell cue on a worker thread. The chain tries each
/// player in turn — a player that spawns but exits non-zero (no sound
/// server, bad device) does not end the search. Total absence of every
/// player is silent.
pub(crate) fn bell_sound() {
    let Some(path) = zen_wav_path() else {
        return;
    };
    thread::spawn(move || {
        let _ = first_ok(PLAYERS, |program| play_to_end(program, &path));
    });
}

#[cfg(not(target_os = "macos"))]
pub(crate) const PLAYERS: &[&str] = &["paplay", "pw-play", "aplay"];
#[cfg(target_os = "macos")]
pub(crate) const PLAYERS: &[&str] = &["afplay"];

/// Run one player to completion; true only on a clean exit. The `wait()`
/// both judges the exit status and reaps the child.
fn play_to_end(program: &str, path: &Path) -> bool {
    Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| child.wait())
        .is_ok_and(|status| status.success())
}

/// First candidate the attempt function accepts wins. Separated from
/// process spawning so tests exercise the fallback with a mock.
fn first_ok(candidates: &[&str], mut attempt: impl FnMut(&str) -> bool) -> bool {
    candidates.iter().any(|candidate| attempt(candidate))
}

/// Play a file through the same player chain as the bell. Blocks until the
/// chosen player exits. OGG skips `aplay` (it cannot decode Vorbis).
pub(crate) fn play_file_to_end(path: &Path) -> bool {
    let players = crate::walkthrough_audio::players_for(PLAYERS, path);
    if players.is_empty() {
        return false;
    }
    first_ok(&players, |program| play_to_end(program, path))
}

#[cfg(target_os = "macos")]
fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn notify_argv_carries_title_and_body() {
        let (program, args) = notify_argv("Prismattyc", "Bell in a pane");
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(program, "notify-send");
            assert_eq!(args, ["Prismattyc", "Bell in a pane"]);
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(program, "osascript");
            assert_eq!(args.len(), 2);
            assert!(args[1].contains("Bell in a pane"));
            assert!(args[1].contains("Prismattyc"));
        }
    }

    #[test]
    fn attention_notification_argv_carries_agent_session_and_message() {
        let (program, args) = notify_argv("Claude needs you — work", "permission needed");
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(program, "notify-send");
            assert_eq!(args, ["Claude needs you — work", "permission needed"]);
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(program, "osascript");
            assert!(args[1].contains("Claude needs you — work"));
            assert!(args[1].contains("permission needed"));
        }
    }

    #[test]
    fn fallback_skips_failed_players_and_stops_at_first_success() {
        let tried = RefCell::new(Vec::new());
        let players = ["paplay", "pw-play", "aplay"];
        let played = first_ok(&players, |program| {
            tried.borrow_mut().push(program.to_string());
            program == "aplay"
        });
        assert!(played);
        assert_eq!(tried.borrow().as_slice(), ["paplay", "pw-play", "aplay"]);

        let tried = RefCell::new(Vec::new());
        let played = first_ok(&players, |program| {
            tried.borrow_mut().push(program.to_string());
            program == "paplay"
        });
        assert!(played);
        assert_eq!(tried.borrow().as_slice(), ["paplay"], "short-circuits");

        assert!(!first_ok(&players, |_| false), "all fail means silence");
    }

    #[test]
    fn spawn_detached_reaps_and_reports_spawn_failure() {
        // A real but harmless child proves the reaper path; a bogus binary
        // proves the spawn-failure path. Neither notifies nor makes sound.
        assert!(spawn_detached(&mut Command::new("true")));
        assert!(!spawn_detached(&mut Command::new(
            "prismattyc-no-such-player-binary"
        )));
    }

    #[test]
    fn zen_asset_is_a_real_wav() {
        assert_eq!(&super::ZEN_WAV[..4], b"RIFF");
        assert_eq!(&super::ZEN_WAV[8..12], b"WAVE");
    }

    #[test]
    fn zen_wav_materializes_to_a_readable_path() {
        let path = super::zen_wav_path().expect("temp dir must be writable");
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes, super::ZEN_WAV);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_escapes_quotes_and_backslashes() {
        assert_eq!(super::escape_applescript("a\"b\\c"), "a\\\"b\\\\c");
    }
}
