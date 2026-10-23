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

/// `[mux]` keys. The host stores this table so one config file can carry it.
/// Keys this build does not know are ignored; see [`mux_unknown_key_warnings`].
/// Unknown keys outside `[mux]` are still rejected by the host.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema, serde::Serialize,
)]
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
    /// Pane program. Empty uses the platform default.
    pub shell: Option<String>,
}

/// Who `pmux space open` types a saved pane `command` into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[derive(schemars::JsonSchema, serde::Serialize)]
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

/// Missing file or missing table is defaults. Invalid TOML is an error.
/// An unknown key inside `[mux]` warns and is ignored so the known keys stay.
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
    for warning in
        mux_unknown_key_warnings(&raw).with_context(|| format!("parse {}", path.display()))?
    {
        eprintln!("prismattyc: warning: {warning}");
    }
    let parsed: FileWithMux =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed.mux.unwrap_or_default())
}

/// Unknown `[mux]` key names, sorted. The warning names the key and not its
/// value. Invalid TOML is an error. A missing table is empty.
pub fn mux_unknown_key_warnings(raw: &str) -> Result<Vec<String>> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: toml::Value =
        toml::from_str(raw).context("parse config while checking [mux] keys")?;
    let Some(table) = value.as_table() else {
        return Ok(Vec::new());
    };
    let Some(mux) = table.get("mux") else {
        return Ok(Vec::new());
    };
    let Some(mux_table) = mux.as_table() else {
        return Ok(Vec::new());
    };
    let mut unknown: Vec<&str> = mux_table
        .keys()
        .map(String::as_str)
        .filter(|key| !MUX_KEYS.iter().any(|spec| spec.name == *key))
        .collect();
    unknown.sort_unstable();
    Ok(unknown
        .into_iter()
        .map(|key| format!("ignoring unknown [mux] key `{key}`"))
        .collect())
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
    MuxKeySpec {
        name: "shell",
        doc: "Pane shell override. Unset on Windows preserves COMSPEC or cmd.exe. Set pwsh.exe or powershell.exe to opt in to PowerShell. Unset on Unix uses SHELL or /bin/sh",
        range: "program name or path",
        kind: MuxKeyKind::CommentedPath("pwsh.exe"),
    },
];

/// Render the `[mux]` table with comments.
pub fn render_mux_section() -> String {
    let mut out = String::from("[mux]\n");
    out.push_str(
        "# A new pane uses [mux] shell when that value is set.\n\
         # Otherwise it uses COMSPEC on Windows, or cmd.exe when COMSPEC is unset.\n\
         # Elsewhere it uses SHELL, or /bin/sh when SHELL is unset.\n\
         # One PowerShell pane: pmux new NAME -- powershell.exe\n",
    );
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

/// A completed replacement. A directory sync failure does not undo a rename.
#[derive(Debug)]
pub struct ConfigCommit<T> {
    pub value: T,
    pub changed: bool,
    pub durability_warning: Option<String>,
}

impl<T> ConfigCommit<T> {
    /// Legacy callers still report a durability warning without replaying an edit.
    pub fn report(self) -> T {
        if let Some(warning) = self.durability_warning {
            eprintln!("prismattyc: config saved, but directory sync failed: {warning}");
        }
        self.value
    }
}

/// Serialize a complete fresh-read edit across host processes. Locks a stable
/// sibling file, since replacing the config changes its inode. Existing symlinks
/// resolve to their target, preserving the user's link and one lock identity.
pub fn transact_config<T>(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T>,
) -> Result<ConfigCommit<T>> {
    locked_config(path, |raw| {
        let mut document = raw
            .parse::<toml_edit::DocumentMut>()
            .context("parse config before edit")?;
        let value = edit(&mut document)?;
        Ok((document.to_string(), value))
    })
}

/// Deliberately replace the entire document (for an explicit template write).
/// Read/modify/write callers use `transact_config` instead of a stale string.
pub fn write_config_atomic(path: &Path, contents: &str) -> Result<()> {
    locked_config(path, |_| Ok((contents.to_string(), ())))?.report();
    Ok(())
}

/// Atomically replace a generated artifact without leaving a persistent sidecar.
/// Service/startup directories consume their contents; config locks belong only
/// to configuration transactions, not generated registration files.
pub fn write_file_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("create artifact directory")?;
    }
    if let Some(warning) = replace_file(
        path,
        contents,
        Replacement::Unconditional,
        sync_config_directory,
    )? {
        eprintln!("prismattyc: file saved, but directory sync failed: {warning}");
    }
    Ok(())
}

enum Replacement<'a> {
    Unconditional,
    IfUnchanged(Option<&'a str>),
}

fn locked_config<T>(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<(String, T)>,
) -> Result<ConfigCommit<T>> {
    locked_config_with_sync(path, edit, sync_config_directory)
}

fn sync_config_directory(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn locked_config_with_sync<T>(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<(String, T)>,
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<ConfigCommit<T>> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).context("create config directory")?;
    let target = if std::fs::symlink_metadata(path).is_ok() {
        std::fs::canonicalize(path).context("resolve config path")?
    } else {
        std::fs::canonicalize(parent)?.join(path.file_name().context("config filename")?)
    };
    let parent = target.parent().context("config parent")?;
    let filename = target
        .file_name()
        .context("config filename")?
        .to_string_lossy();
    let lock_path = parent.join(format!(".{filename}.lock"));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    let lock = options.open(&lock_path).context("open config lock")?;
    lock.lock().context("lock config")?;
    let original = match std::fs::read_to_string(&target) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read config under lock"),
    };
    let raw = original.as_deref().unwrap_or_default();
    let (contents, value) = edit(raw)?;
    if contents == raw && target.exists() {
        return Ok(ConfigCommit {
            value,
            changed: false,
            durability_warning: None,
        });
    }
    let durability_warning = replace_file(
        &target,
        &contents,
        Replacement::IfUnchanged(original.as_deref()),
        sync_directory,
    )?;
    Ok(ConfigCommit {
        value,
        changed: true,
        durability_warning,
    })
}

fn replace_file(
    path: &Path,
    contents: &str,
    condition: Replacement<'_>,
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<Option<String>> {
    let target = path;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .context("artifact filename")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{filename}.prism-config-{}-{nonce}",
        std::process::id()
    ));
    let before = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .context("create temporary config")?;
        file.write_all(contents.as_bytes())
            .context("write temporary config")?;
        if let Ok(metadata) = std::fs::metadata(target) {
            std::fs::set_permissions(&temporary, metadata.permissions())
                .context("preserve config permissions")?;
        }
        file.sync_all().context("sync temporary config")?;
        // Cooperating writers are serialized. Detect external edits made before
        // this check; an editor ignoring the advisory lock can still race rename.
        if let Replacement::IfUnchanged(expected) = condition {
            let current = match std::fs::read_to_string(target) {
                Ok(raw) => Some(raw),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("recheck config before replacement"),
            };
            anyhow::ensure!(
                current.as_deref() == expected,
                "config changed outside the settings editor; retry the edit"
            );
        }
        std::fs::rename(&temporary, target).context("replace config")?;
        Ok(())
    })();
    if let Err(error) = before {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(sync_directory(parent).err().map(|error| error.to_string()))
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
        assert!(text.contains("pmux new NAME -- powershell.exe"));
        assert!(!text.contains("is not a setting"));
        assert!(!text.contains("drops this file"));
        assert!(
            !text.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("shell ") || trimmed.starts_with("shell=")
            }),
            "the template must leave shell commented: {text}"
        );
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
        assert!(mux.shell.is_none());
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
    fn generated_artifact_replacement_leaves_only_the_requested_file() {
        let path = temp_cfg("generated-artifact").with_file_name("pmux.service");
        write_file_atomic(&path, "[Service]\nExecStart=/opt/pmux login run\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[Service]\nExecStart=/opt/pmux login run\n"
        );
        write_file_atomic(&path, "[Service]\nExecStart=/opt/new-pmux login run\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[Service]\nExecStart=/opt/new-pmux login run\n"
        );
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("pmux.service")]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn committed_write_reports_directory_sync_failure_without_replay() {
        let path = temp_cfg("durability");
        std::fs::write(&path, "entries = []\n").unwrap();
        let calls = std::cell::Cell::new(0);
        let result = locked_config_with_sync(
            &path,
            |raw| {
                calls.set(calls.get() + 1);
                let mut document: toml_edit::DocumentMut = raw.parse()?;
                document["entries"]
                    .as_array_mut()
                    .unwrap()
                    .push("one addition");
                Ok((document.to_string(), 17))
            },
            |_| Err(std::io::Error::other("injected directory sync failure")),
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(result.value, 17);
        assert!(result.changed);
        assert_eq!(
            result.durability_warning.as_deref(),
            Some("injected directory sync failure")
        );
        let saved: toml::Value = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            saved["entries"].as_array().unwrap(),
            &vec![toml::Value::String("one addition".into())]
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn external_removal_of_empty_file_is_a_conflict() {
        let path = temp_cfg("external-removal");
        std::fs::write(&path, "").unwrap();
        let result = transact_config(&path, |document| {
            std::fs::remove_file(&path)?;
            document["font_px"] = toml_edit::value(18.0);
            Ok(())
        });
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("changed outside"));
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn transaction_retains_symlink_and_target_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let path = temp_cfg("symlink");
        let target = path.with_file_name("target.toml");
        std::fs::write(&target, "# existing\nfont_px = 15.0\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &path).unwrap();
        transact_config(&path, |document| {
            document["font_px"] = toml_edit::value(18.0);
            Ok(())
        })
        .unwrap();
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "# existing\nfont_px = 18.0\n"
        );
        std::fs::remove_file(&target).unwrap();
        assert!(transact_config(&path, |_| Ok(())).is_err());
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
    fn unknown_mux_key_keeps_known_keys() {
        let path = temp_cfg("unknown");
        std::fs::write(
            &path,
            "[mux]\nzed = 1\ninstance = \"work\"\nnot_a_key = true\n",
        )
        .unwrap();
        let section = load_mux_section(&path).unwrap();
        assert_eq!(section.instance.as_deref(), Some("work"));
        assert_eq!(
            mux_unknown_key_warnings("[mux]\nzed = 1\ninstance = \"work\"\nnot_a_key = true\n")
                .unwrap(),
            vec![
                "ignoring unknown [mux] key `not_a_key`".to_string(),
                "ignoring unknown [mux] key `zed`".to_string(),
            ]
        );
        assert!(mux_unknown_key_warnings("theme = \"monokai\"\n")
            .unwrap()
            .is_empty());
        std::fs::write(&path, "[mux]\ninstance = \n").unwrap();
        assert!(load_mux_section(&path).is_err(), "invalid TOML still fails");
        std::fs::write(&path, "[mux]\nattach_on_new = \"yes\"\n").unwrap();
        assert!(
            load_mux_section(&path).is_err(),
            "a known key with the wrong type still fails"
        );
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
                shell: None,
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
                shell: None,
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
                shell: None,
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
                shell: None,
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
                shell: None,
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
