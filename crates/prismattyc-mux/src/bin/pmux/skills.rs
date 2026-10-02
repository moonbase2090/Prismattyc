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
    "pmux skills install [--agent codex|claude|cursor|muse|kiro|all] [--check] [--force]

Install the pmux Agent Skill in user-level skill directories.
--agent defaults to all. Existing files are left alone unless --force is used.
--check reports what would happen without changing files.
Enable this experimental command with PRISMATTYC_EXPERIMENTAL_PMUX_SKILLS=1.
";
const FEATURE_FLAG: &str = "PRISMATTYC_EXPERIMENTAL_PMUX_SKILLS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agent {
    Codex,
    Claude,
    Cursor,
    Muse,
    Kiro,
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
            "all" => Ok(Self::All),
            _ => bail!("--agent must be codex, claude, cursor, muse, kiro, or all"),
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

#[derive(Debug)]
struct FileTarget {
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
    if !feature_flag_enabled(std::env::var(FEATURE_FLAG).ok().as_deref()) {
        bail!("pmux skills install is off by default; set {FEATURE_FLAG}=1 to enable it");
    }
    let home = PathBuf::from(
        prismattyc_mux::platform::home_dir().context("cannot determine the user home directory")?,
    );
    install(&home, args)
}

fn feature_flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
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
    let mut push = |label, base: &str| {
        targets.push(FileTarget {
            label,
            skill_dir: home.join(base).join("skills").join("pmux"),
        });
    };
    match agent {
        Agent::Codex => {
            push("codex", ".codex");
            push("codex shared", ".agents");
        }
        Agent::Claude => push("claude", ".claude"),
        Agent::Cursor => push("cursor", ".cursor"),
        Agent::Kiro => push("kiro", ".kiro"),
        Agent::Muse => {}
        Agent::All => {
            push("codex", ".codex");
            push("codex shared", ".agents");
            push("claude", ".claude");
            push("cursor", ".cursor");
            push("kiro", ".kiro");
        }
    }
    targets
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
    let agent = args.agent.unwrap_or(Agent::All);
    let targets = file_targets(home, agent);
    let mut file_states = Vec::with_capacity(targets.len());
    for target in &targets {
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

    let muse_path = if agent.includes_muse() {
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
        if agent.includes_muse() {
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

    if agent.includes_muse() && (muse_path.is_none() || args.force) {
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
    if agent.includes_muse() {
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
                "kiro".into(),
                "--check".into(),
                "--force".into()
            ])
            .unwrap(),
            InstallArgs {
                agent: Some(Agent::Kiro),
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
    fn installer_feature_flag_is_off_by_default_and_accepts_standard_true_values() {
        assert!(!feature_flag_enabled(None));
        assert!(!feature_flag_enabled(Some("0")));
        assert!(feature_flag_enabled(Some("1")));
        assert!(feature_flag_enabled(Some(" true ")));
        assert!(feature_flag_enabled(Some("ON")));
        assert!(feature_flag_enabled(Some("yes")));
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
