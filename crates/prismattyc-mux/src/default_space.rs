//! Keep at least one Space on disk so a fresh install can open a session.
//!
//! A new install has no Space files. `pmux space open` with no name then
//! fails to read `default.json`, and the host has nothing to open. `pmuxd`
//! calls [`ensure`] every time it starts, which covers the first start and
//! any later restart that finds no Space. A Space that already exists, under
//! any name, is left alone, so the call never adds a second default.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Name of the Space created when none exists.
pub const DEFAULT_SPACE_NAME: &str = "default";

/// Set to `0`, `false`, `off`, or `no` to skip creating the default Space.
pub const DEFAULT_SPACE_ENV: &str = "PRISMATTYC_DEFAULT_SPACE";

/// What [`ensure`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// At least one Space already exists. Nothing was written.
    Exists,
    /// The default Space was created.
    Created,
    /// The environment turned the default Space off.
    Disabled,
}

/// True when `dir` holds no readable Space and no `default` file.
///
/// An unreadable `default.json` still counts as present, so a damaged file
/// is never replaced.
pub fn needs_default_space(dir: &Path) -> Result<bool> {
    if !crate::list_spaces(dir)?.is_empty() {
        return Ok(false);
    }
    let default = crate::layout_path(dir, DEFAULT_SPACE_NAME)?;
    Ok(!default.exists())
}

fn disabled_by_env() -> bool {
    std::env::var(DEFAULT_SPACE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Create the default Space through `pmux` when `spaces_dir` has none.
///
/// `adopt` names a live session for the Space to own, such as the session a
/// fresh daemon starts with, so no second shell is spawned. Without it, or
/// when adopting fails, `pmux space create` starts a new session. The daemon
/// lock allows one `pmuxd` per socket, and clients wait for its pid, which is
/// published after this call, so nothing else writes the Space meanwhile.
pub fn ensure(
    pmux: &Path,
    socket: &Path,
    spaces_dir: &Path,
    adopt: Option<&str>,
) -> Result<Outcome> {
    if disabled_by_env() {
        return Ok(Outcome::Disabled);
    }
    if !needs_default_space(spaces_dir)? {
        return Ok(Outcome::Exists);
    }
    let mut errors = Vec::new();
    if let Some(session) = adopt {
        match run_pmux(
            pmux,
            socket,
            &["space", "save", DEFAULT_SPACE_NAME, session],
        ) {
            Ok(()) if !needs_default_space(spaces_dir)? => return Ok(Outcome::Created),
            Ok(()) => errors.push("space save wrote no Space".to_string()),
            Err(err) => errors.push(format!("{err:#}")),
        }
    }
    match run_pmux(
        pmux,
        socket,
        &["space", "create", DEFAULT_SPACE_NAME, "--no-attach"],
    ) {
        Ok(()) => Ok(Outcome::Created),
        Err(_) if !needs_default_space(spaces_dir)? => Ok(Outcome::Exists),
        Err(err) => {
            errors.push(format!("{err:#}"));
            bail!("{}", errors.join("; "))
        }
    }
}

fn run_pmux(pmux: &Path, socket: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new(pmux);
    command
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env("PMUX_SOCKET", socket)
        .env_remove("PRISMATTYC_SPACES_SOCKET")
        .env_remove("PRISMATTYC_PANE_ID")
        .stdin(Stdio::null());
    if let Some(home) = crate::platform::home_dir() {
        command.current_dir(home);
    }
    let output = command
        .output()
        .with_context(|| format!("run {}", pmux.display()))?;
    if !output.status.success() {
        bail!(
            "pmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pmux-default-space-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_space(dir: &Path, name: &str) {
        let mut space = crate::SavedSpace {
            version: crate::SAVED_SPACE_VERSION,
            id: None,
            created_at_unix_ms: None,
            saved_at_unix: 1,
            sessions: vec![],
            tabs: vec![],
            active_tab: 0,
            focused_session: None,
        };
        crate::space_add_session(&mut space, crate::stub_space_session("work"), None).unwrap();
        crate::save_space(dir, name, &space).unwrap();
    }

    #[test]
    fn missing_directory_needs_default() {
        let dir = temp_dir("missing");
        assert!(needs_default_space(&dir).unwrap());
    }

    #[test]
    fn empty_directory_needs_default() {
        let dir = temp_dir("empty");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), b"not a space").unwrap();
        let needed = needs_default_space(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(needed);
    }

    #[test]
    fn any_named_space_suppresses_default() {
        let dir = temp_dir("named");
        write_space(&dir, "work");
        let needed = needs_default_space(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!needed);
    }

    #[test]
    fn existing_default_space_is_not_recreated() {
        let dir = temp_dir("default");
        write_space(&dir, DEFAULT_SPACE_NAME);
        let needed = needs_default_space(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!needed);
    }

    #[test]
    fn damaged_default_file_is_kept() {
        let dir = temp_dir("damaged");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("default.json"), b"{ not json").unwrap();
        let needed = needs_default_space(&dir).unwrap();
        let body = fs::read(dir.join("default.json")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!needed);
        assert_eq!(body, b"{ not json");
    }

    #[test]
    fn ensure_skips_pmux_when_a_space_exists() {
        let dir = temp_dir("skip");
        write_space(&dir, "work");
        // A missing pmux binary proves no child was started.
        let outcome = ensure(
            Path::new("/nonexistent/pmux"),
            Path::new("/nonexistent.sock"),
            &dir,
            Some("default"),
        );
        let names: Vec<_> = crate::list_spaces(&dir)
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(outcome.unwrap(), Outcome::Exists);
        assert_eq!(names, vec!["work".to_string()]);
    }
}
