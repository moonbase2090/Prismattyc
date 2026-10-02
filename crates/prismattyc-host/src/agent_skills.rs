//! First-launch installation of the bundled pmux Agent Skill.

use std::{fs, io::Write, path::Path, time::SystemTime};

#[cfg(target_os = "macos")]
const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(target_os = "macos")]
const OPT_OUT_ENV: &str = "PRISMATTYC_NO_AGENT_SKILLS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallDecision {
    Disabled,
    AlreadyCurrent,
    Attempted,
}

fn is_disabled(config_enabled: bool, env_value: Option<&str>) -> bool {
    !config_enabled || env_value == Some("1")
}

fn install_once(
    version_file: &Path,
    version: &str,
    disabled: bool,
    install: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<InstallDecision> {
    if disabled {
        return Ok(InstallDecision::Disabled);
    }
    match fs::read_to_string(version_file) {
        Ok(installed) if installed.trim() == version => return Ok(InstallDecision::AlreadyCurrent),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(anyhow::anyhow!("read {}: {error}", version_file.display())),
    }

    let install_result = install();
    // Record completed attempts as well as failures, so a broken external
    // agent installation cannot add work to every subsequent app launch.
    write_version(version_file, version)?;
    install_result?;
    Ok(InstallDecision::Attempted)
}

fn write_version(path: &Path, version: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("create {}: {error}", parent.display()))?;
    }
    let mut file = fs::File::create(path)
        .map_err(|error| anyhow::anyhow!("write {}: {error}", path.display()))?;
    writeln!(file, "{version}")
        .map_err(|error| anyhow::anyhow!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| anyhow::anyhow!("flush {}: {error}", path.display()))
}

#[cfg(target_os = "macos")]
pub fn start(config_enabled: bool, pmux: std::path::PathBuf) {
    let env_opt_out = std::env::var(OPT_OUT_ENV).ok();
    if is_disabled(config_enabled, env_opt_out.as_deref()) {
        return;
    }

    let state_file = version_file();
    let worker = std::thread::Builder::new()
        .name("pmux-agent-skill-install".into())
        .spawn(move || {
            let result = state_file.and_then(|state_file| {
                install_once(&state_file, VERSION, false, || {
                    let output = std::process::Command::new(&pmux)
                        .args(["skills", "install", "--agent", "detected"])
                        .output()
                        .map_err(|error| {
                            anyhow::anyhow!("run `pmux skills install --agent detected`: {error}")
                        })?;
                    if !output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        anyhow::bail!(
                            "`pmux skills install --agent detected` exited with {}; stdout: {}; stderr: {}",
                            output.status,
                            stdout.trim(),
                            stderr.trim()
                        );
                    }
                    Ok(())
                })
            });
            if let Err(error) = result {
                log_failure(&format!(
                    "Prismattyc {VERSION}: automatic pmux Agent Skill installation failed: {error:#}. The app stayed open; retry with `pmux skills install --agent detected`. To disable automatic installation, set `install_agent_skills = false` or {OPT_OUT_ENV}=1."
                ));
            }
        });

    if let Err(error) = worker {
        log_failure(&format!(
            "Prismattyc {VERSION}: could not start the background pmux Agent Skill installer: {error}. The app stayed open; retry with `pmux skills install --agent detected`."
        ));
    }
}

#[cfg(target_os = "macos")]
fn log_failure(message: &str) {
    match state_file_for_log().and_then(|path| append_failure(&path, message)) {
        Ok(()) => {}
        Err(log_error) => eprintln!("{message} (also could not write its log: {log_error:#})"),
    }
}

#[cfg(target_os = "macos")]
fn version_file() -> anyhow::Result<std::path::PathBuf> {
    let data_home = prismattyc_mux::platform::data_home()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            prismattyc_mux::platform::home_dir()
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".local/share"))
        })
        .ok_or_else(|| anyhow::anyhow!("cannot determine the user data directory"))?;
    Ok(data_home
        .join("prismattyc")
        .join("agent-skills-install-version"))
}

#[cfg(target_os = "macos")]
fn state_file_for_log() -> anyhow::Result<std::path::PathBuf> {
    Ok(version_file()?.with_file_name("agent-skills-install.log"))
}

#[cfg(target_os = "macos")]
fn append_failure(path: &Path, message: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("create {}: {error}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| anyhow::anyhow!("open {}: {error}", path.display()))?;
    let seconds = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(file, "[{seconds}] {message}")
        .map_err(|error| anyhow::anyhow!("append {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "prismattyc-agent-skills-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory.join("installed-version")
    }

    #[test]
    fn install_runs_once_per_version_and_retries_after_version_change() {
        let version_file = temp_path("version");
        let mut runs = 0;
        assert_eq!(
            install_once(&version_file, "1.2.3", false, || {
                runs += 1;
                Ok(())
            })
            .unwrap(),
            InstallDecision::Attempted
        );
        assert_eq!(
            install_once(&version_file, "1.2.3", false, || {
                runs += 1;
                Ok(())
            })
            .unwrap(),
            InstallDecision::AlreadyCurrent
        );
        assert_eq!(
            install_once(&version_file, "1.2.4", false, || {
                runs += 1;
                Ok(())
            })
            .unwrap(),
            InstallDecision::Attempted
        );
        assert_eq!(runs, 2);
        assert_eq!(fs::read_to_string(&version_file).unwrap(), "1.2.4\n");
        let _ = fs::remove_dir_all(version_file.parent().unwrap());
    }

    #[test]
    fn opt_out_skips_without_running_or_recording_the_version() {
        let version_file = temp_path("opt-out");
        assert!(is_disabled(false, None));
        assert!(is_disabled(true, Some("1")));
        assert!(!is_disabled(true, Some("0")));
        assert!(!is_disabled(true, None));
        assert_eq!(
            install_once(&version_file, "1.2.3", is_disabled(true, Some("1")), || {
                panic!("opt-out must not run the installer")
            })
            .unwrap(),
            InstallDecision::Disabled
        );
        assert!(!version_file.exists());
        let _ = fs::remove_dir_all(version_file.parent().unwrap());
    }

    #[test]
    fn failed_install_is_recorded_and_does_not_run_again_this_version() {
        let version_file = temp_path("failed");
        let mut runs = 0;
        assert!(install_once(&version_file, "1.2.3", false, || {
            runs += 1;
            anyhow::bail!("user-edited skill was preserved")
        })
        .is_err());
        assert_eq!(
            install_once(&version_file, "1.2.3", false, || {
                runs += 1;
                Ok(())
            })
            .unwrap(),
            InstallDecision::AlreadyCurrent
        );
        assert_eq!(runs, 1);
        let _ = fs::remove_dir_all(version_file.parent().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installer_failure_message_is_appended_to_the_user_log() {
        let log_file = temp_path("log");
        append_failure(&log_file, "skill copy was preserved after a conflict").unwrap();
        let logged = fs::read_to_string(&log_file).unwrap();
        assert!(logged.contains("skill copy was preserved after a conflict"));
        let _ = fs::remove_dir_all(log_file.parent().unwrap());
    }
}
