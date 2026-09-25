//! Shared `[mux]` table in the Prismattyc config file.
//!
//! Same path as `prismattyc-host`: `$PRISMATTYC_CONFIG`, else
//! `$XDG_CONFIG_HOME/prismattyc/config.toml`, else
//! `~/.config/prismattyc/config.toml`.
//! CLI flags and `PMUX_*` env vars always win over the file.

use std::path::{Path, PathBuf};

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::remote_size::RemoteSizePolicy;

/// `[mux]` keys. The host stores this so `deny_unknown_fields` accepts the
/// table; only `prismattyc-mux` applies it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MuxSection {
    /// Instance name used to derive `pmux.sock` / `pmux-<instance>.sock`.
    pub instance: Option<String>,
    /// Absolute socket path. Overrides `instance` when set.
    pub socket: Option<PathBuf>,
    /// When true (default), `pmux new` attaches in a TTY after create.
    /// Set false to keep the old create-and-print behavior.
    pub attach_on_new: Option<bool>,
    /// Which saved pane commands `pmux space open` re-runs (PT-93).
    pub space_open_runs_commands: Option<SpaceOpenRunsCommands>,
    /// Who sets pane size when a TTY attach and the host share a window (PT-200).
    pub remote_size: Option<RemoteSizePolicy>,
}

/// Who `pmux space open` types a saved pane `command` into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SpaceOpenRunsCommands {
    /// Sessions that stored an agent id (default).
    #[default]
    Agents,
    /// Every session that stored a command.
    All,
    /// Never re-run.
    None,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FileWithMux {
    #[serde(default)]
    mux: Option<MuxSection>,
}

/// Same lookup order as `prismattyc-host` config.
pub fn prism_config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("PRISMATTYC_CONFIG") {
        return PathBuf::from(explicit);
    }
    let base = crate::platform::config_home()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::platform::home_dir().map(|home| Path::new(&home).join(".config")));
    base.map_or_else(
        || PathBuf::from("prismattyc-config.toml"),
        |base| base.join("prismattyc").join("config.toml"),
    )
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn env_first(key: &str) -> Option<String> {
    env_nonempty(key)
}

fn env_os_first(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// Missing file or missing table is defaults. Invalid TOML / unknown mux keys
/// are errors so a typo does not silently fall through.
pub fn load_mux_section(path: &Path) -> Result<MuxSection> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MuxSection::default())
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(MuxSection::default());
    }
    let parsed: FileWithMux =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed.mux.unwrap_or_default())
}

/// CLI > env > file > built-in default. Returns `(instance, socket_override)`.
///
/// A CLI `--instance` names the socket by itself: it drops the `PMUX_SOCKET`
/// / `[mux] socket` override, which every pmux pane exports for its own
/// server (PT-61). Only an explicit `--socket` still wins over it.
pub fn resolve_mux_target(
    cli_instance: Option<String>,
    cli_socket: Option<PathBuf>,
    file: &MuxSection,
) -> Result<(String, Option<PathBuf>)> {
    let cli_named_instance = cli_instance.is_some();
    let instance = first_name([
        cli_instance,
        env_first("PMUX_INSTANCE"),
        file.instance.clone(),
    ])
    .unwrap_or_else(|| "default".into());
    if instance.is_empty() || instance.contains('/') {
        bail!("mux instance must be a non-empty name without path separators");
    }

    let socket = cli_socket.or_else(|| {
        if cli_named_instance {
            return None;
        }
        env_os_first("PMUX_SOCKET")
            .map(PathBuf::from)
            .or_else(|| file.socket.clone())
    });
    if let Some(path) = socket.as_ref() {
        if !path.is_absolute() {
            bail!("mux socket must be an absolute path");
        }
    }
    Ok((instance, socket))
}

/// CLI > env > file > built-in default (`true`).
pub fn resolve_attach_on_new(cli: Option<bool>, file: &MuxSection) -> Result<bool> {
    if let Some(value) = cli {
        return Ok(value);
    }
    if let Some(raw) = env_first("PMUX_ATTACH_ON_NEW") {
        return parse_bool_env("PMUX_ATTACH_ON_NEW", &raw);
    }
    Ok(file.attach_on_new.unwrap_or(true))
}

/// CLI > env `PMUX_REMOTE_SIZE` > file > built-in default (`latest`).
pub fn resolve_remote_size(
    cli: Option<RemoteSizePolicy>,
    file: &MuxSection,
) -> Result<RemoteSizePolicy> {
    if let Some(value) = cli {
        return Ok(value);
    }
    if let Some(raw) = env_first("PMUX_REMOTE_SIZE") {
        return parse_remote_size(&raw);
    }
    Ok(file.remote_size.unwrap_or_default())
}

fn parse_remote_size(raw: &str) -> Result<RemoteSizePolicy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "latest" => Ok(RemoteSizePolicy::Latest),
        "host" => Ok(RemoteSizePolicy::Host),
        other => bail!("PMUX_REMOTE_SIZE must be latest or host, got {other:?}"),
    }
}

/// CLI `--no-run` > env > file > default (`agents`).
pub fn resolve_space_open_runs_commands(
    no_run: bool,
    file: &MuxSection,
) -> Result<SpaceOpenRunsCommands> {
    if no_run {
        return Ok(SpaceOpenRunsCommands::None);
    }
    if let Some(raw) = env_first("PMUX_SPACE_OPEN_RUNS_COMMANDS") {
        return parse_space_open_runs_commands(&raw);
    }
    Ok(file
        .space_open_runs_commands
        .unwrap_or(SpaceOpenRunsCommands::Agents))
}

fn parse_space_open_runs_commands(raw: &str) -> Result<SpaceOpenRunsCommands> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "agents" => Ok(SpaceOpenRunsCommands::Agents),
        "all" => Ok(SpaceOpenRunsCommands::All),
        "none" => Ok(SpaceOpenRunsCommands::None),
        other => bail!("PMUX_SPACE_OPEN_RUNS_COMMANDS must be agents/all/none, got {other:?}"),
    }
}

fn parse_bool_env(key: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => bail!("{key} must be true/false/1/0/yes/no/on/off, got {other:?}"),
    }
}

/// One `[mux]` key for the shared config template (PT-84).
#[derive(Debug, Clone, Copy)]
pub struct MuxKeySpec {
    pub name: &'static str,
    pub doc: &'static str,
    pub range: &'static str,
    pub kind: MuxKeyKind,
}

/// Default form of a `[mux]` key in the template.
#[derive(Debug, Clone, Copy)]
pub enum MuxKeyKind {
    String(&'static str),
    CommentedPath(&'static str),
    Bool(bool),
}

/// `[mux]` keys. Host generator and `pmux config init` share this table.
pub const MUX_KEYS: &[MuxKeySpec] = &[
    MuxKeySpec {
        name: "instance",
        doc: "Instance name used to derive pmux.sock / pmux-<instance>.sock",
        range: "non-empty name, no '/'",
        kind: MuxKeyKind::String("default"),
    },
    MuxKeySpec {
        name: "socket",
        doc: "Absolute socket path; overrides instance when set",
        range: "absolute path",
        kind: MuxKeyKind::CommentedPath("/run/user/1000/prismattyc/pmux.sock"),
    },
    MuxKeySpec {
        name: "attach_on_new",
        doc: "Attach in a TTY after pmux new",
        range: "true|false",
        kind: MuxKeyKind::Bool(true),
    },
    MuxKeySpec {
        name: "space_open_runs_commands",
        doc: "Re-run saved pane commands on space open",
        range: "agents|all|none",
        kind: MuxKeyKind::String("agents"),
    },
    MuxKeySpec {
        name: "remote_size",
        doc: "Who sets pane size when a TTY attach shares the host window",
        range: "latest|host",
        kind: MuxKeyKind::String("latest"),
    },
];

/// Render the `[mux]` table with comments.
pub fn render_mux_section() -> String {
    let mut out = String::from("[mux]\n");
    for key in MUX_KEYS {
        out.push_str(&format!("# {}. {}.\n", key.doc, key.range));
        match key.kind {
            MuxKeyKind::String(value) => {
                out.push_str(&format!("{} = \"{value}\"\n", key.name));
            }
            MuxKeyKind::CommentedPath(path) => {
                out.push_str(&format!("# {} = \"{path}\"\n", key.name));
            }
            MuxKeyKind::Bool(value) => {
                out.push_str(&format!("{} = {value}\n", key.name));
            }
        }
    }
    out
}

/// Insert missing `[mux]` keys. Existing keys and comments stay.
pub fn merge_mux_section(document: &mut toml_edit::DocumentMut) {
    if document
        .get("mux")
        .and_then(|item| item.as_table())
        .is_none()
    {
        let mut table = toml_edit::Table::new();
        table.set_implicit(false);
        table.decor_mut().set_prefix("\n");
        document["mux"] = toml_edit::Item::Table(table);
    }
    let Some(table) = document["mux"].as_table_mut() else {
        return;
    };
    for key in MUX_KEYS {
        if table_has_key_or_comment(table, key.name) {
            continue;
        }
        match key.kind {
            MuxKeyKind::String(value) => {
                table.insert(key.name, toml_edit::value(value));
            }
            MuxKeyKind::CommentedPath(path) => {
                insert_commented_path(table, key.name, path);
            }
            MuxKeyKind::Bool(value) => {
                table.insert(key.name, toml_edit::value(value));
            }
        }
    }
}

fn table_has_key_or_comment(table: &toml_edit::Table, name: &str) -> bool {
    if table.contains_key(name) {
        return true;
    }
    let needle = format!("# {name} =");
    let live = format!("{name} =");
    table.to_string().lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(&needle) || trimmed.starts_with(&live)
    })
}

fn insert_commented_path(table: &mut toml_edit::Table, name: &str, path: &str) {
    table.insert(name, toml_edit::value(path));
    if let Some(mut key) = table.key_mut(name) {
        key.leaf_decor_mut().set_prefix("# ");
    }
}

/// Write `contents` to `path` via a sibling temp file, fsync, and rename.
pub fn write_config_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{filename}.prism-config-{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o644);
        let mut file = opts
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temporary, metadata.permissions())
                .with_context(|| format!("preserve permissions for {}", path.display()))?;
        }
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replace {} with {}", path.display(), temporary.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn first_name(candidates: [Option<String>; 3]) -> Option<String> {
    candidates.into_iter().find_map(|value| {
        value.and_then(|name| {
            let trimmed = name.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn restore_env(key: &str, prior: Option<std::ffi::OsString>) {
        match prior {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    fn without_mux_env<T>(body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let keys = [
            "PMUX_INSTANCE",
            "PMUX_INSTANCE",
            "PMUX_SOCKET",
            "PMUX_SOCKET",
            "PMUX_REMOTE_SIZE",
        ];
        let prior: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        let result = body();
        for (key, value) in prior {
            restore_env(key, value);
        }
        result
    }

    #[test]
    fn mux_template_contains_every_key() {
        let text = render_mux_section();
        assert!(text.starts_with("[mux]\n"));
        for key in MUX_KEYS {
            assert!(text.contains(key.name), "mux template missing {}", key.name);
        }
        let parsed: FileWithMux = toml::from_str(&text).expect("mux template parses");
        let mux = parsed.mux.expect("mux table");
        assert_eq!(mux.instance.as_deref(), Some("default"));
        assert!(mux.socket.is_none());
        assert_eq!(mux.attach_on_new, Some(true));
        assert_eq!(
            mux.space_open_runs_commands,
            Some(SpaceOpenRunsCommands::Agents)
        );
        assert_eq!(mux.remote_size, Some(RemoteSizePolicy::Latest));
    }

    #[test]
    fn merge_appends_commented_socket() {
        let mut document = "theme = \"dracula\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        merge_mux_section(&mut document);
        let text = document.to_string();
        assert!(text.contains("[mux]"));
        assert!(text.contains("instance = \"default\""));
        assert!(
            text.contains("# socket ="),
            "merge must append commented socket: {text}"
        );
        merge_mux_section(&mut document);
        assert_eq!(
            document.to_string().matches("# socket =").count(),
            1,
            "second merge must not duplicate commented socket"
        );
    }

    fn temp_cfg(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-mux-cfg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn missing_file_is_defaults() {
        let path = temp_cfg("missing");
        assert_eq!(load_mux_section(&path).unwrap(), MuxSection::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn loads_instance_and_socket() {
        let path = temp_cfg("load");
        std::fs::write(
            &path,
            "focus_border = \"amber\"\n\n[mux]\ninstance = \"work\"\nsocket = \"/tmp/prism-work.sock\"\n",
        )
        .unwrap();
        let section = load_mux_section(&path).unwrap();
        assert_eq!(section.instance.as_deref(), Some("work"));
        assert_eq!(
            section.socket.as_deref(),
            Some(Path::new("/tmp/prism-work.sock"))
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_mux_key_is_rejected() {
        let path = temp_cfg("bad");
        std::fs::write(&path, "[mux]\nnot_a_key = true\n").unwrap();
        assert!(load_mux_section(&path).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn with_mux_env<T>(vars: &[(&str, &str)], body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let keys = ["PMUX_INSTANCE", "PMUX_SOCKET"];
        let prior: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        let result = body();
        for (key, value) in prior {
            restore_env(key, value);
        }
        result
    }

    #[test]
    fn cli_instance_beats_env_and_file_socket() {
        // Inside a pmux pane PMUX_SOCKET points at the pane's own server;
        // `pmux --instance other` must still reach the other server.
        with_mux_env(&[("PMUX_SOCKET", "/tmp/pane.sock")], || {
            let file = MuxSection {
                instance: None,
                socket: Some(PathBuf::from("/tmp/file.sock")),
                attach_on_new: None,
                space_open_runs_commands: None,
                remote_size: None,
            };
            let (instance, socket) = resolve_mux_target(Some("other".into()), None, &file).unwrap();
            assert_eq!(instance, "other");
            assert_eq!(socket, None, "--instance names the socket by itself");

            let (_, socket) = resolve_mux_target(None, None, &file).unwrap();
            assert_eq!(
                socket.as_deref(),
                Some(Path::new("/tmp/pane.sock")),
                "without --instance the env socket still applies"
            );
            let (_, socket) = resolve_mux_target(
                Some("other".into()),
                Some(PathBuf::from("/tmp/cli.sock")),
                &file,
            )
            .unwrap();
            assert_eq!(socket.as_deref(), Some(Path::new("/tmp/cli.sock")));
        });
    }

    #[test]
    fn cli_wins_over_file() {
        without_mux_env(|| {
            let file = MuxSection {
                instance: Some("file".into()),
                socket: Some(PathBuf::from("/tmp/file.sock")),
                attach_on_new: None,
                space_open_runs_commands: None,
                remote_size: None,
            };
            let (instance, socket) = resolve_mux_target(
                Some("cli".into()),
                Some(PathBuf::from("/tmp/cli.sock")),
                &file,
            )
            .unwrap();
            assert_eq!(instance, "cli");
            assert_eq!(socket.as_deref(), Some(Path::new("/tmp/cli.sock")));
        });
    }

    #[test]
    fn file_used_when_cli_absent() {
        without_mux_env(|| {
            let file = MuxSection {
                instance: Some("work".into()),
                socket: None,
                attach_on_new: None,
                space_open_runs_commands: None,
                remote_size: None,
            };
            let (instance, socket) = resolve_mux_target(None, None, &file).unwrap();
            assert_eq!(instance, "work");
            assert!(socket.is_none());
        });
    }

    #[test]
    fn env_wins_over_file() {
        without_mux_env(|| {
            std::env::set_var("PMUX_INSTANCE", "from-env");
            let file = MuxSection {
                instance: Some("file".into()),
                socket: None,
                attach_on_new: None,
                space_open_runs_commands: None,
                remote_size: None,
            };
            let (instance, socket) = resolve_mux_target(None, None, &file).unwrap();
            assert_eq!(instance, "from-env");
            assert!(socket.is_none());
        });
    }

    #[test]
    fn relative_socket_is_rejected() {
        without_mux_env(|| {
            let file = MuxSection {
                instance: None,
                socket: Some(PathBuf::from("relative.sock")),
                attach_on_new: None,
                space_open_runs_commands: None,
                remote_size: None,
            };
            assert!(resolve_mux_target(None, None, &file).is_err());
        });
    }

    fn without_attach_env<T>(body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior_new = std::env::var_os("PMUX_ATTACH_ON_NEW");
        let prior_old = std::env::var_os("PMUX_ATTACH_ON_NEW");
        std::env::remove_var("PMUX_ATTACH_ON_NEW");
        std::env::remove_var("PMUX_ATTACH_ON_NEW");
        let result = body();
        restore_env("PMUX_ATTACH_ON_NEW", prior_new);
        restore_env("PMUX_ATTACH_ON_NEW", prior_old);
        result
    }

    #[test]
    fn attach_on_new_defaults_true() {
        without_attach_env(|| {
            assert!(resolve_attach_on_new(None, &MuxSection::default()).unwrap());
        });
    }

    #[test]
    fn attach_on_new_file_opt_out() {
        without_attach_env(|| {
            let file = MuxSection {
                attach_on_new: Some(false),
                ..MuxSection::default()
            };
            assert!(!resolve_attach_on_new(None, &file).unwrap());
        });
    }

    #[test]
    fn attach_on_new_cli_wins_over_file_and_env() {
        without_attach_env(|| {
            std::env::set_var("PMUX_ATTACH_ON_NEW", "0");
            let file = MuxSection {
                attach_on_new: Some(false),
                ..MuxSection::default()
            };
            assert!(resolve_attach_on_new(Some(true), &file).unwrap());
            assert!(!resolve_attach_on_new(Some(false), &file).unwrap());
        });
    }

    #[test]
    fn attach_on_new_env_wins_over_file() {
        without_attach_env(|| {
            std::env::set_var("PMUX_ATTACH_ON_NEW", "false");
            let file = MuxSection {
                attach_on_new: Some(true),
                ..MuxSection::default()
            };
            assert!(!resolve_attach_on_new(None, &file).unwrap());
        });
    }

    #[test]
    fn attach_on_new_unknown_env_is_rejected() {
        without_attach_env(|| {
            std::env::set_var("PMUX_ATTACH_ON_NEW", "maybe");
            assert!(resolve_attach_on_new(None, &MuxSection::default()).is_err());
        });
    }

    #[test]
    fn remote_size_defaults_latest() {
        without_mux_env(|| {
            assert_eq!(
                resolve_remote_size(None, &MuxSection::default()).unwrap(),
                RemoteSizePolicy::Latest
            );
            let file = MuxSection {
                remote_size: Some(RemoteSizePolicy::Host),
                ..MuxSection::default()
            };
            assert_eq!(
                resolve_remote_size(None, &file).unwrap(),
                RemoteSizePolicy::Host
            );
            std::env::set_var("PMUX_REMOTE_SIZE", "latest");
            assert_eq!(
                resolve_remote_size(None, &file).unwrap(),
                RemoteSizePolicy::Latest
            );
        });
    }

    #[test]
    fn loads_attach_on_new() {
        let path = temp_cfg("attach-on-new");
        std::fs::write(&path, "[mux]\nattach_on_new = false\n").unwrap();
        let section = load_mux_section(&path).unwrap();
        assert_eq!(section.attach_on_new, Some(false));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn without_space_open_env<T>(body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prior = std::env::var_os("PMUX_SPACE_OPEN_RUNS_COMMANDS");
        std::env::remove_var("PMUX_SPACE_OPEN_RUNS_COMMANDS");
        let result = body();
        restore_env("PMUX_SPACE_OPEN_RUNS_COMMANDS", prior);
        result
    }

    #[test]
    fn space_open_runs_commands_defaults_agents() {
        without_space_open_env(|| {
            assert_eq!(
                resolve_space_open_runs_commands(false, &MuxSection::default()).unwrap(),
                SpaceOpenRunsCommands::Agents
            );
            assert_eq!(
                resolve_space_open_runs_commands(true, &MuxSection::default()).unwrap(),
                SpaceOpenRunsCommands::None
            );
        });
    }

    #[test]
    fn space_open_runs_commands_file_and_env() {
        without_space_open_env(|| {
            let file = MuxSection {
                space_open_runs_commands: Some(SpaceOpenRunsCommands::All),
                ..MuxSection::default()
            };
            assert_eq!(
                resolve_space_open_runs_commands(false, &file).unwrap(),
                SpaceOpenRunsCommands::All
            );
            std::env::set_var("PMUX_SPACE_OPEN_RUNS_COMMANDS", "none");
            assert_eq!(
                resolve_space_open_runs_commands(false, &file).unwrap(),
                SpaceOpenRunsCommands::None
            );
            assert_eq!(
                resolve_space_open_runs_commands(true, &file).unwrap(),
                SpaceOpenRunsCommands::None
            );
        });
    }

    #[test]
    fn loads_space_open_runs_commands() {
        let path = temp_cfg("space-open-runs");
        std::fs::write(&path, "[mux]\nspace_open_runs_commands = \"all\"\n").unwrap();
        let section = load_mux_section(&path).unwrap();
        assert_eq!(
            section.space_open_runs_commands,
            Some(SpaceOpenRunsCommands::All)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
