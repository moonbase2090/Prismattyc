//! Install the embedded pmux Agent Skill into user-level agent directories.

use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const SKILL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../skills/pmux/SKILL.md"
));

const HELP: &str =
    "pmux skills install [--agent codex|claude|cursor|muse|kiro|detected|all] [--check] [--force]

Install the pmux Agent Skill in user-level skill directories.
--agent defaults to all. Existing files are left alone unless --force is used.
--agent detected installs only for agents with a config directory or CLI on PATH.
--check reports what would happen without changing files.
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agent {
    Codex,
    Claude,
    Cursor,
    Muse,
    Kiro,
    Detected,
    All,
}

impl Agent {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "cursor" => Ok(Self::Cursor),
            "muse" => Ok(Self::Muse),
            "kiro" => Ok(Self::Kiro),
            "detected" => Ok(Self::Detected),
            "all" => Ok(Self::All),
            _ => bail!("--agent must be codex, claude, cursor, muse, kiro, detected, or all"),
        }
    }

    fn includes_muse(self) -> bool {
        matches!(self, Self::Muse | Self::All)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InstallArgs {
    agent: Option<Agent>,
    check: bool,
    force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Existing {
    Missing,
    Current,
    Different,
}

#[derive(Clone, Debug)]
struct FileTarget {
    agent: Agent,
    label: &'static str,
    skill_dir: PathBuf,
}

pub(super) fn run(rest: Vec<String>) -> Result<()> {
    if rest
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        print!("{HELP}");
        return Ok(());
    }
    let mut args = rest.into_iter();
    let command = args.next().context(HELP)?;
    if command != "install" {
        bail!("unknown skills command {command:?}\n{HELP}");
    }
    let rest: Vec<_> = args.collect();
    if rest.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP}");
        return Ok(());
    }
    let args = parse_install_args(rest)?;
    let home = PathBuf::from(
        prismattyc_mux::platform::home_dir().context("cannot determine the user home directory")?,
    );
    install(&home, args)
}

fn parse_install_args(args: impl IntoIterator<Item = String>) -> Result<InstallArgs> {
    let mut parsed = InstallArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent" => {
                if parsed.agent.is_some() {
                    bail!("--agent may be specified only once");
                }
                parsed.agent = Some(Agent::parse(
                    &args.next().context("--agent requires a value")?,
                )?);
            }
            "--check" if !parsed.check => parsed.check = true,
            "--force" if !parsed.force => parsed.force = true,
            "--help" | "-h" => return Err(anyhow::anyhow!("{HELP}")),
            _ => bail!("unknown or repeated skills install argument {arg:?}\n{HELP}"),
        }
    }
    Ok(parsed)
}

fn file_targets(home: &Path, agent: Agent) -> Vec<FileTarget> {
    let mut targets = Vec::new();
    let mut push = |agent, label, base: &str| {
        targets.push(FileTarget {
            agent,
            label,
            skill_dir: home.join(base).join("skills").join("pmux"),
        });
    };
    match agent {
        Agent::Codex => {
            push(Agent::Codex, "codex", ".codex");
            push(Agent::Codex, "codex shared", ".agents");
        }
        Agent::Claude => push(Agent::Claude, "claude", ".claude"),
        Agent::Cursor => push(Agent::Cursor, "cursor", ".cursor"),
        Agent::Kiro => push(Agent::Kiro, "kiro", ".kiro"),
        Agent::Muse => {}
        Agent::Detected => {}
        Agent::All => {
            push(Agent::Codex, "codex", ".codex");
            push(Agent::Codex, "codex shared", ".agents");
            push(Agent::Claude, "claude", ".claude");
            push(Agent::Cursor, "cursor", ".cursor");
            push(Agent::Kiro, "kiro", ".kiro");
        }
    }
    targets
}

#[derive(Debug, Default)]
struct DetectedTargets {
    files: Vec<FileTarget>,
    muse: bool,
}

fn detected_targets(
    home: &Path,
    config_home: &Path,
    path: Option<&std::ffi::OsStr>,
) -> DetectedTargets {
    let mut detected = DetectedTargets::default();
    let codex_dir = home.join(".codex");
    let shared_dir = home.join(".agents");
    let codex_detected =
        codex_dir.is_dir() || shared_dir.is_dir() || command_on_path(path, &["codex"]);
    if codex_detected {
        if codex_dir.is_dir() {
            detected.files.push(FileTarget {
                agent: Agent::Codex,
                label: "codex",
                skill_dir: codex_dir.join("skills/pmux"),
            });
        }
        if shared_dir.is_dir() {
            detected.files.push(FileTarget {
                agent: Agent::Codex,
                label: "codex shared",
                skill_dir: shared_dir.join("skills/pmux"),
            });
        }
        if !codex_dir.is_dir() && !shared_dir.is_dir() {
            detected.files.push(FileTarget {
                agent: Agent::Codex,
                label: "codex",
                skill_dir: codex_dir.join("skills/pmux"),
            });
        }
    }

    for (agent, label, config_dir, commands) in [
        (Agent::Claude, "claude", ".claude", &["claude"][..]),
        (
            Agent::Cursor,
            "cursor",
            ".cursor",
            &["cursor-agent", "cursor"][..],
        ),
        (Agent::Kiro, "kiro", ".kiro", &["kiro-cli", "kiro"][..]),
    ] {
        let config_dir = home.join(config_dir);
        if config_dir.is_dir() || command_on_path(path, commands) {
            detected.files.push(FileTarget {
                agent,
                label,
                skill_dir: config_dir.join("skills/pmux"),
            });
        }
    }

    detected.muse = config_home.join("muse").is_dir()
        || home.join(".muse").is_dir()
        || command_on_path(path, &["muse"]);
    detected
}

fn command_on_path(path: Option<&std::ffi::OsStr>, names: &[&str]) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|directory| {
        names
            .iter()
            .any(|name| is_executable(&directory.join(name)))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}

#[cfg(not(any(unix, windows)))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn inspect_file(skill_dir: &Path) -> Result<Existing> {
    let path = skill_dir.join("SKILL.md");
    match fs::read(&path) {
        Ok(content) if content == SKILL.as_bytes() => Ok(Existing::Current),
        Ok(_) => Ok(Existing::Different),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Existing::Missing),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn muse_skill_path(home: &Path) -> Result<Option<String>> {
    let output = Command::new("muse")
        .args(["skills", "list", "--source", "user", "--json"])
        .env("HOME", home)
        .output()
        .context(
            "run `muse skills list --source user --json` (install Muse or select another agent)",
        )?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            detail.to_string()
        };
        bail!("muse skills list failed: {detail}");
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parse JSON from `muse skills list --source user --json`")?;
    muse_skill_path_from_json(&response)
}

fn muse_skill_path_from_json(response: &serde_json::Value) -> Result<Option<String>> {
    let Some(skills) = response.get("skills").and_then(serde_json::Value::as_array) else {
        bail!("Muse skill list did not contain a `skills` array");
    };
    for skill in skills {
        let id = skill.get("id").and_then(serde_json::Value::as_str);
        let name = skill.get("name").and_then(serde_json::Value::as_str);
        if id == Some("pmux") || name == Some("pmux") {
            let raw_path = skill
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("Muse listed pmux without a skill path")?;
            // Muse reports its managed user-skill location symbolically.
            // Other user-scope skills (for example ~/.claude/skills) may
            // also appear in the list, but are installed by their own target.
            if matches!(
                raw_path,
                "$CONFIG_DIR/skills/pmux" | "$CONFIG_DIR/skills/pmux/SKILL.md"
            ) {
                return Ok(Some(raw_path.to_string()));
            }
        }
    }
    // Muse omits malformed packages from `skills`, but reports their managed
    // path in diagnostics. Treat that path as present so check/install never
    // mistakes an edited or damaged copy for a missing one.
    if let Some(diagnostics) = response
        .get("diagnostics")
        .and_then(serde_json::Value::as_array)
    {
        for diagnostic in diagnostics {
            let raw_path = diagnostic.get("path").and_then(serde_json::Value::as_str);
            if matches!(
                raw_path,
                Some("$CONFIG_DIR/skills/pmux" | "$CONFIG_DIR/skills/pmux/SKILL.md")
            ) {
                return Ok(raw_path.map(str::to_owned));
            }
        }
    }
    Ok(None)
}

fn state_name(existing: Existing, force: bool) -> &'static str {
    match existing {
        Existing::Current => "current",
        Existing::Missing => "missing",
        Existing::Different if force => "would replace",
        Existing::Different => "conflict",
    }
}

fn install(home: &Path, args: InstallArgs) -> Result<()> {
    let path = std::env::var_os("PATH");
    let config_home = prismattyc_mux::platform::config_home()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    install_with_paths(home, args, path.as_deref(), &config_home)
}

#[cfg(test)]
fn install_with_path(home: &Path, args: InstallArgs, path: Option<&std::ffi::OsStr>) -> Result<()> {
    let config_home = home.join(".config");
    install_with_paths(home, args, path, &config_home)
}

fn install_with_paths(
    home: &Path,
    args: InstallArgs,
    path: Option<&std::ffi::OsStr>,
    config_home: &Path,
) -> Result<()> {
    let agent = args.agent.unwrap_or(Agent::All);
    if agent == Agent::Detected {
        let detected = detected_targets(home, config_home, path);
        return install_detected(home, args, detected, || muse_skill_path(home));
    }
    let targets = file_targets(home, agent);
    let includes_muse = agent.includes_muse();
    if targets.is_empty() && !includes_muse {
        println!(
            "no supported agents detected; install an agent or create its config directory, \
             then run `pmux skills install --agent detected` (for example, `pmux skills install --agent codex`)"
        );
        return Ok(());
    }
    install_targets(home, args, &targets, includes_muse)
}

fn install_detected(
    home: &Path,
    args: InstallArgs,
    detected: DetectedTargets,
    muse_path: impl FnOnce() -> Result<Option<String>>,
) -> Result<()> {
    if detected.files.is_empty() && !detected.muse {
        println!(
            "no supported agents detected; install an agent or create its config directory, \
             then run `pmux skills install --agent detected` (for example, `pmux skills install --agent codex`)"
        );
        return Ok(());
    }

    let mut errors = Vec::new();
    for (agent, label) in [
        (Agent::Codex, "codex"),
        (Agent::Claude, "claude"),
        (Agent::Cursor, "cursor"),
        (Agent::Kiro, "kiro"),
    ] {
        let targets: Vec<_> = detected
            .files
            .iter()
            .filter(|target| target.agent == agent)
            .cloned()
            .collect();
        if !targets.is_empty() {
            if let Err(error) = install_targets(home, args, &targets, false) {
                println!("{label}: failed ({error:#})");
                errors.push(format!("{label}: {error:#}"));
            }
        }
    }

    if detected.muse {
        match muse_path() {
            Err(error) => {
                println!("muse: failed ({error:#})");
                errors.push(format!("muse: {error:#}"));
            }
            Ok(path) => {
                if args.check {
                    println!(
                        "muse: {} ({})",
                        match (path.is_some(), args.force) {
                            (true, true) => "would replace",
                            (true, false) => "present; left in place",
                            (false, _) => "missing",
                        },
                        path.as_deref().unwrap_or("user skills directory")
                    );
                } else {
                    let result = if path.is_none() || args.force {
                        install_muse(home, args.force)
                    } else {
                        Ok(())
                    };
                    match result {
                        Ok(()) => println!(
                            "muse: {} ({})",
                            if path.is_some() && !args.force {
                                "already present; left in place"
                            } else {
                                "installed"
                            },
                            path.as_deref().unwrap_or("user skills directory")
                        ),
                        Err(error) => {
                            println!("muse: failed ({error:#})");
                            errors.push(format!("muse: {error:#}"));
                        }
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "detected agent skill installation had failures: {}",
            errors.join("; ")
        )
    }
}

fn install_targets(
    home: &Path,
    args: InstallArgs,
    targets: &[FileTarget],
    includes_muse: bool,
) -> Result<()> {
    let mut file_states = Vec::with_capacity(targets.len());
    for target in targets {
        let state = inspect_file(&target.skill_dir)?;
        if state == Existing::Different && !args.force {
            bail!(
                "{} already has a different pmux skill at {}; use --force to replace it",
                target.label,
                target.skill_dir.join("SKILL.md").display()
            );
        }
        file_states.push(state);
    }

    let muse_path = if includes_muse {
        muse_skill_path(home)?
    } else {
        None
    };

    if args.check {
        for (target, state) in targets.iter().zip(file_states) {
            println!(
                "{}: {} ({})",
                target.label,
                state_name(state, args.force),
                target.skill_dir.display()
            );
        }
        if includes_muse {
            println!(
                "muse: {} ({})",
                match (muse_path.is_some(), args.force) {
                    (true, true) => "would replace",
                    (true, false) => "present; left in place",
                    (false, _) => "missing",
                },
                muse_path.as_deref().unwrap_or("user skills directory")
            );
        }
        return Ok(());
    }

    if includes_muse && (muse_path.is_none() || args.force) {
        install_muse(home, args.force)?;
    }
    for (target, state) in targets.iter().zip(file_states) {
        if state != Existing::Current {
            write_skill(&target.skill_dir, args.force)?;
        }
        println!(
            "{}: {} ({})",
            target.label,
            if state == Existing::Current {
                "already current"
            } else {
                "installed"
            },
            target.skill_dir.display()
        );
    }
    if includes_muse {
        println!(
            "muse: {} ({})",
            if muse_path.is_some() && !args.force {
                "already present; left in place"
            } else {
                "installed"
            },
            muse_path.as_deref().unwrap_or("user skills directory")
        );
    }
    Ok(())
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = base.join(format!("{prefix}-{}-{stamp}", std::process::id()));
    fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
    Ok(path)
}

fn install_muse(home: &Path, force: bool) -> Result<()> {
    let source = unique_temp_dir("pmux-skill-source")?;
    let result = (|| {
        fs::write(source.join("SKILL.md"), SKILL)
            .with_context(|| format!("write skill source in {}", source.display()))?;
        let mut command = Command::new("muse");
        command
            .args(["skills", "install"])
            .arg(&source)
            .args(["--scope", "user", "--name", "pmux"]);
        if force {
            command.arg("--force");
        }
        command.arg("--json");
        command.env("HOME", home);
        let output = command.output().context("run `muse skills install`")?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            let detail = if detail.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                detail.to_string()
            };
            bail!("muse skills install failed: {detail}");
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&source);
    result
}

fn write_skill(skill_dir: &Path, force: bool) -> Result<()> {
    fs::create_dir_all(skill_dir).with_context(|| format!("create {}", skill_dir.display()))?;
    let destination = skill_dir.join("SKILL.md");
    if destination.exists() && !force {
        match inspect_file(skill_dir)? {
            Existing::Current => return Ok(()),
            Existing::Different => bail!(
                "{} already contains a different skill; use --force to replace it",
                destination.display()
            ),
            Existing::Missing => {}
        }
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = skill_dir.join(format!(".SKILL.md.{}.{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(SKILL.as_bytes())
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flush {}", temporary.display()))?;
        if destination.exists() {
            fs::remove_file(&destination)
                .with_context(|| format!("replace {}", destination.display()))?;
        }
        fs::rename(&temporary, &destination)
            .with_context(|| format!("install {}", destination.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pmux-skills-test-{label}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn install_args_parse_agent_check_and_force() {
        assert_eq!(
            parse_install_args([
                "--agent".into(),
                "detected".into(),
                "--check".into(),
                "--force".into()
            ])
            .unwrap(),
            InstallArgs {
                agent: Some(Agent::Detected),
                check: true,
                force: true,
            }
        );
        assert_eq!(
            parse_install_args(Vec::<String>::new()).unwrap().agent,
            None
        );
        assert!(parse_install_args(["--agent".into(), "unknown".into()]).is_err());
        assert!(parse_install_args(["--force".into(), "--force".into()]).is_err());
    }

    #[test]
    fn agent_targets_use_expected_user_directories() {
        let home = Path::new("/home/tester");
        let codex = file_targets(home, Agent::Codex);
        assert_eq!(codex.len(), 2);
        assert_eq!(codex[0].skill_dir, home.join(".codex/skills/pmux"));
        assert_eq!(codex[1].skill_dir, home.join(".agents/skills/pmux"));
        assert_eq!(file_targets(home, Agent::Muse).len(), 0);
        let all = file_targets(home, Agent::All);
        assert_eq!(all.len(), 5);
        assert!(all
            .iter()
            .any(|target| target.skill_dir == home.join(".agents/skills/pmux")));
        assert!(all
            .iter()
            .any(|target| target.skill_dir == home.join(".kiro/skills/pmux")));
        assert!(Agent::All.includes_muse());
    }

    #[test]
    fn detected_agents_use_existing_config_dirs_or_executable_path_entries() {
        let home = temp_home("detection");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let detected = detected_targets(&home, &home.join(".config"), Some(bin.as_os_str()));
        let paths: Vec<_> = detected
            .files
            .iter()
            .map(|target| target.skill_dir.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                home.join(".codex/skills/pmux"),
                home.join(".claude/skills/pmux")
            ]
        );
        assert!(!detected.muse);
        assert!(!home.join(".agents").exists());
        assert!(!home.join(".cursor").exists());
        assert!(!home.join(".kiro").exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_cli_on_path_gets_skill_without_other_agent_directories() {
        let home = temp_home("detected-cli");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let cursor = bin.join("cursor-agent");
        fs::write(&cursor, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cursor, fs::Permissions::from_mode(0o755)).unwrap();
        }

        install_with_path(
            &home,
            InstallArgs {
                agent: Some(Agent::Detected),
                check: false,
                force: false,
            },
            Some(bin.as_os_str()),
        )
        .unwrap();
        assert_eq!(
            fs::read(home.join(".cursor/skills/pmux/SKILL.md")).unwrap(),
            SKILL.as_bytes()
        );
        assert!(!home.join(".codex").exists());
        assert!(!home.join(".agents").exists());
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".kiro").exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_mode_recognizes_every_supported_cli_on_path() {
        let home = temp_home("detected-all-clis");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["codex", "claude", "cursor-agent", "kiro-cli", "muse"] {
            let executable = bin.join(name);
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        let detected = detected_targets(&home, &home.join(".config"), Some(bin.as_os_str()));
        let labels: Vec<_> = detected.files.iter().map(|target| target.label).collect();
        assert_eq!(labels, vec!["codex", "claude", "cursor", "kiro"]);
        assert!(detected.muse);
        assert!(!home.join(".codex").exists());
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".cursor").exists());
        assert!(!home.join(".kiro").exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_mode_uses_the_config_home_for_muse_detection() {
        let home = temp_home("detected-muse-xdg");
        let config_home = home.join("custom-config");
        fs::create_dir_all(config_home.join("muse")).unwrap();

        let detected = detected_targets(&home, &config_home, None);
        assert!(detected.files.is_empty());
        assert!(detected.muse);
        assert!(!home.join(".config").exists());
        assert!(!home.join(".muse").exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_install_writes_only_for_detected_agents_and_is_idempotent() {
        let home = temp_home("detected-install");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        let no_path = None;
        let args = InstallArgs {
            agent: Some(Agent::Detected),
            check: false,
            force: false,
        };

        install_with_path(&home, args, no_path).unwrap();
        let codex_file = home.join(".codex/skills/pmux/SKILL.md");
        let claude_file = home.join(".claude/skills/pmux/SKILL.md");
        assert_eq!(fs::read(&codex_file).unwrap(), SKILL.as_bytes());
        assert_eq!(fs::read(&claude_file).unwrap(), SKILL.as_bytes());
        assert!(!home.join(".agents").exists());
        assert!(!home.join(".cursor").exists());
        assert!(!home.join(".kiro").exists());

        let before = fs::metadata(&codex_file).unwrap().modified().unwrap();
        install_with_path(&home, args, no_path).unwrap();
        let after = fs::metadata(&codex_file).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "a second install must leave current files alone"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_install_preserves_edited_agent_and_installs_other_agents() {
        let home = temp_home("detected-preserve");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".claude/skills/pmux")).unwrap();
        let edited = home.join(".claude/skills/pmux/SKILL.md");
        fs::write(&edited, "user-edited skill\n").unwrap();

        let result = install_with_path(
            &home,
            InstallArgs {
                agent: Some(Agent::Detected),
                check: false,
                force: false,
            },
            None,
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(home.join(".codex/skills/pmux/SKILL.md")).unwrap(),
            SKILL.as_bytes()
        );
        assert_eq!(fs::read_to_string(edited).unwrap(), "user-edited skill\n");
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detected_muse_failure_does_not_prevent_other_agent_installs() {
        let home = temp_home("detected-muse-failure");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".config/muse")).unwrap();
        let detected = detected_targets(&home, &home.join(".config"), None);
        let result = install_detected(
            &home,
            InstallArgs {
                agent: Some(Agent::Detected),
                check: false,
                force: false,
            },
            detected,
            || anyhow::bail!("Muse CLI is unavailable"),
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(home.join(".codex/skills/pmux/SKILL.md")).unwrap(),
            SKILL.as_bytes()
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn user_copy_is_preserved_unless_force_is_given() {
        let home = temp_home("preserve");
        let target = home.join(".codex/skills/pmux");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "user-edited skill\n").unwrap();

        assert_eq!(inspect_file(&target).unwrap(), Existing::Different);
        assert!(write_skill(&target, false).is_err());
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "user-edited skill\n"
        );

        fs::write(target.join("notes.md"), "keep me\n").unwrap();
        write_skill(&target, true).unwrap();
        assert_eq!(fs::read_to_string(target.join("SKILL.md")).unwrap(), SKILL);
        assert_eq!(
            fs::read_to_string(target.join("notes.md")).unwrap(),
            "keep me\n"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn install_preflights_every_target_before_writing_any_copy() {
        let home = temp_home("preflight");
        let codex = home.join(".codex/skills/pmux");
        let shared = home.join(".agents/skills/pmux");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("SKILL.md"), "user-edited copy\n").unwrap();

        let result = install(
            &home,
            InstallArgs {
                agent: Some(Agent::Codex),
                check: false,
                force: false,
            },
        );
        assert!(result.is_err());
        assert!(
            !codex.exists(),
            "preflight should prevent a partial install"
        );
        assert_eq!(
            fs::read_to_string(shared.join("SKILL.md")).unwrap(),
            "user-edited copy\n"
        );

        install(
            &home,
            InstallArgs {
                agent: Some(Agent::Codex),
                check: false,
                force: true,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(codex.join("SKILL.md")).unwrap(), SKILL);
        assert_eq!(fs::read_to_string(shared.join("SKILL.md")).unwrap(), SKILL);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn identical_copy_is_idempotent_and_check_does_not_create_it() {
        let home = temp_home("idempotent");
        let target = home.join(".cursor/skills/pmux");
        assert_eq!(inspect_file(&target).unwrap(), Existing::Missing);
        assert!(!target.exists());
        install(
            &home,
            InstallArgs {
                agent: Some(Agent::Cursor),
                check: true,
                force: false,
            },
        )
        .unwrap();
        assert!(!target.exists());
        write_skill(&target, false).unwrap();
        let before = fs::metadata(target.join("SKILL.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(inspect_file(&target).unwrap(), Existing::Current);
        write_skill(&target, false).unwrap();
        let after = fs::metadata(target.join("SKILL.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn muse_managed_path_is_distinguished_from_other_user_skill_sources() {
        let managed = serde_json::json!({
            "skills": [{"id": "pmux", "path": "$CONFIG_DIR/skills/pmux/SKILL.md"}]
        });
        assert_eq!(
            muse_skill_path_from_json(&managed).unwrap().as_deref(),
            Some("$CONFIG_DIR/skills/pmux/SKILL.md")
        );
        let other_user_root = serde_json::json!({
            "skills": [{"id": "pmux", "path": "$HOME/.agents/skills/pmux/SKILL.md"}]
        });
        assert_eq!(muse_skill_path_from_json(&other_user_root).unwrap(), None);

        let malformed_managed = serde_json::json!({
            "skills": [],
            "diagnostics": [{
                "code": "invalid-skill-package",
                "scope": "user",
                "path": "$CONFIG_DIR/skills/pmux/SKILL.md"
            }]
        });
        assert_eq!(
            muse_skill_path_from_json(&malformed_managed)
                .unwrap()
                .as_deref(),
            Some("$CONFIG_DIR/skills/pmux/SKILL.md")
        );
    }
}
