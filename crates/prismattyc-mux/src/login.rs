//! Per-user login registration. The daemon owns workspace persistence; the
//! login job only supervises the existing CLI's single-instance startup.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct Preference {
    start_at_login: Option<bool>,
}

pub fn preference() -> Result<Option<bool>> {
    let path = crate::prism_config_path();
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(toml::from_str::<Preference>(&raw)?.start_at_login),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn has_workspace(socket: &Path) -> bool {
    crate::workspace::path(socket).is_file()
        || crate::attach_tabs::load(&crate::attach_tabs::layout_path_from_socket(socket))
            .is_some_and(|view| !view.tabs.is_empty())
        || crate::list_spaces(&crate::layout_file::spaces_dir_for_socket(socket))
            .is_ok_and(|spaces| !spaces.is_empty())
}

pub fn enabled(socket: &Path) -> Result<bool> {
    if let Some(choice) = preference()? {
        return Ok(choice);
    }
    Ok(resolved_default(socket)?.unwrap_or_else(|| has_workspace(socket)))
}

fn default_path(socket: &Path) -> PathBuf {
    crate::workspace::path(socket).with_file_name("login-default.json")
}

fn resolved_default(socket: &Path) -> Result<Option<bool>> {
    match std::fs::read(default_path(socket)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Resolve before the first daemon can create a saved Space. This per-instance
/// marker covers CLI-only starts without rewriting shared user configuration.
pub fn initialize_default(socket: &Path) -> Result<bool> {
    if let Some(choice) = preference()? {
        return Ok(choice);
    }
    if let Some(choice) = resolved_default(socket)? {
        return Ok(choice);
    }
    let choice = has_workspace(socket);
    crate::workspace::write_private(
        &default_path(socket),
        if choice { b"true" } else { b"false" },
    )?;
    Ok(choice)
}

pub fn set_preference(enabled: bool) -> Result<()> {
    let path = crate::prism_config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document = raw.parse::<toml_edit::DocumentMut>()?;
    document["start_at_login"] = toml_edit::value(enabled);
    crate::write_config_atomic(&path, &document.to_string())
}

fn label(socket: &Path) -> String {
    let hash = crate::workspace::socket_key(socket);
    format!("dev.prismattyc.pmuxd.{}", &hash[..16])
}

#[cfg(not(windows))]
fn home() -> Result<PathBuf> {
    crate::platform::home_dir()
        .map(PathBuf::from)
        .context("home directory is unavailable")
}

pub fn registration_path(socket: &Path) -> Result<PathBuf> {
    let label = label(socket);
    #[cfg(target_os = "macos")]
    {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist")))
    }
    #[cfg(target_os = "linux")]
    {
        let config = crate::platform::config_home()
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".config"));
        Ok(config.join("systemd/user").join(format!("{label}.service")))
    }
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is unset")?;
        Ok(PathBuf::from(appdata)
            .join("Microsoft/Windows/Start Menu/Programs/Startup")
            .join(format!("{label}.lnk")))
    }
}

fn checked(command: &mut Command) -> Result<()> {
    let output = command.output().context("run login service command")?;
    ensure!(
        output.status.success(),
        "login service command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unit_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// Kept platform-independent so both service formats can be verified on CI.
pub fn render_launch_agent(socket: &Path, executable: &Path) -> Result<String> {
    let args = [
        executable.as_os_str(),
        std::ffi::OsStr::new("--socket"),
        socket.as_os_str(),
        std::ffi::OsStr::new("login"),
        std::ffi::OsStr::new("run"),
    ];
    let arguments = args
        .iter()
        .map(|s| format!("<string>{}</string>", xml(&s.to_string_lossy())))
        .collect::<Vec<_>>()
        .join("\n");
    let environment = service_environment()?
        .into_iter()
        .map(|(key, value)| format!("<key>{}</key><string>{}</string>", xml(&key), xml(&value)))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array>{arguments}</array>\n<key>EnvironmentVariables</key><dict>{environment}</dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>AbandonProcessGroup</key><true/>\n<key>AssociatedBundleIdentifiers</key><string>dev.prismattyc.host</string>\n</dict></plist>\n", xml(&label(socket))))
}

pub fn render_systemd_unit(socket: &Path, executable: &Path) -> Result<String> {
    ensure!(
        !socket.to_string_lossy().contains(['\n', '\r'])
            && !executable.to_string_lossy().contains(['\n', '\r']),
        "login paths must not contain newlines"
    );
    let environment = service_environment()?
        .into_iter()
        .map(|(key, value)| format!("Environment={}\n", unit_quote(&format!("{key}={value}"))))
        .collect::<String>();
    Ok(format!("[Unit]\nDescription=Prismattyc workspace restore\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nExecStart={} --socket {} login run\nRestart=on-failure\nRestartSec=10\nKillMode=process\nUMask=0077\n{environment}\n[Install]\nWantedBy=default.target\n", unit_quote(&executable.to_string_lossy().replace('$', "$$")), unit_quote(&socket.to_string_lossy().replace('$', "$$"))))
}

fn service_environment() -> Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "PRISMATTYC_CONFIG",
        "PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            ensure!(
                !value.contains(['\n', '\r', '\0']),
                "invalid login environment value for {key}"
            );
            vars.push((key.to_owned(), value));
        }
    }
    Ok(vars)
}

pub fn sync(socket: &Path, executable: &Path) -> Result<()> {
    if std::env::var_os("PRISMATTYC_NO_LOGIN_SERVICE").is_some() {
        return Ok(());
    }
    let enabled = initialize_default(socket)?;
    // Resolve the migration once. A new install remains opt-in after it
    // creates its first Space; an existing saved workspace defaults on.
    if preference()?.is_none() {
        set_preference(enabled)?;
    }
    let path = registration_path(socket)?;
    if !enabled {
        return remove(socket);
    }
    ensure!(
        executable.is_absolute() && executable.is_file(),
        "login executable must be an installed absolute path"
    );
    #[cfg(target_os = "macos")]
    {
        let body = render_launch_agent(socket, executable)?;
        let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(&body);
        if changed {
            crate::write_config_atomic(&path, &body)?;
        }
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &format!("{domain}/{}", label(socket))])
            .output()?;
        if loaded.status.success() && changed {
            checked(
                Command::new("/bin/launchctl")
                    .args(["bootout", &format!("{domain}/{}", label(socket))]),
            )?;
        }
        if !loaded.status.success() || changed {
            checked(
                Command::new("/bin/launchctl")
                    .arg("bootstrap")
                    .arg(domain)
                    .arg(path),
            )?;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let body = render_systemd_unit(socket, executable)?;
        if std::fs::read_to_string(&path).ok().as_deref() != Some(&body) {
            crate::write_config_atomic(&path, &body)?;
            checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        }
        checked(
            Command::new("systemctl")
                .args(["--user", "enable", "--now"])
                .arg(path.file_name().context("unit filename")?),
        )?;
    }
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").context("SystemRoot")?;
        let script_path = login_script_path(&path);
        let launch =
            windows_login_launch(Path::new(&system_root), executable, socket, &script_path)?;
        std::fs::create_dir_all(path.parent().context("Startup directory")?)?;
        crate::config::write_config_atomic(&script_path, &launch.script)?;
        let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
        let script = format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={};$s.Arguments={};$s.Save()",
            quote(&path.to_string_lossy()),
            quote(&launch.target.to_string_lossy()),
            quote(&launch.arguments),
        );
        let mut command = crate::platform::hidden_command("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        checked(&mut command)?;
    }
    Ok(())
}

/// VBScript beside the Startup shortcut. `wscript //B` runs it with no window,
/// and the script starts `pmux login run` hidden.
pub fn login_script_path(shortcut: &Path) -> PathBuf {
    shortcut.with_extension("vbs")
}

pub struct WindowsLoginLaunch {
    pub target: PathBuf,
    pub arguments: String,
    pub script: String,
}

/// Hidden logon launcher. Paths that contain a quote or a newline are rejected
/// so they cannot break out of the VBScript string.
pub fn windows_login_launch(
    system_root: &Path,
    executable: &Path,
    socket: &Path,
    script_path: &Path,
) -> Result<WindowsLoginLaunch> {
    for path in [system_root, executable, socket, script_path] {
        let text = path.to_string_lossy();
        ensure!(
            !text.contains(['"', '\n', '\r']),
            "login paths must not contain quotes or newlines"
        );
    }
    let script = format!(
        "CreateObject(\"WScript.Shell\").Run \"\"\"{exe}\"\" --socket \"\"{socket}\"\" login run\", 0, False\n",
        exe = executable.display(),
        socket = socket.display(),
    );
    Ok(WindowsLoginLaunch {
        target: system_root.join("System32").join("wscript.exe"),
        arguments: format!("//B //Nologo \"{}\"", script_path.display()),
        script,
    })
}

pub fn remove(socket: &Path) -> Result<()> {
    let path = registration_path(socket)?;
    remove_registration(&path)
}

pub fn registration_label(path: &Path) -> Option<&str> {
    let name = path.file_stem()?.to_str()?;
    let suffix = name.strip_prefix("dev.prismattyc.pmuxd.")?;
    (suffix.len() == 16 && suffix.bytes().all(|c| c.is_ascii_hexdigit())).then_some(name)
}

/// Unload before deleting the executable, including every instance during
/// uninstall. The caller supplies a path from its private user directory.
pub fn remove_registration(path: &Path) -> Result<()> {
    let label = registration_label(path).context("unrecognized login registration")?;
    #[cfg(not(target_os = "macos"))]
    let _ = label;
    let script = login_script_path(path);
    if script.is_file() {
        std::fs::remove_file(script)?;
    }
    if !path.exists() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        // The supervisor's daemon has its own process group and is retained.
        let target = format!("gui/{}/{}", unsafe { libc::getuid() }, label);
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &target])
            .output()?;
    }
    #[cfg(target_os = "linux")]
    {
        checked(
            Command::new("systemctl")
                .args(["--user", "disable", "--now"])
                .arg(path.file_name().context("unit filename")?),
        )?;
    }
    std::fs::remove_file(path)?;
    Ok(())
}

pub fn report(socket: &Path) -> Result<()> {
    let path = registration_path(socket)?;
    println!(
        "login restore: {}",
        if enabled(socket)? {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "login registration: {}",
        if path.is_file() {
            "installed"
        } else {
            "absent"
        }
    );
    println!(
        "saved workspace: {}",
        if has_workspace(socket) {
            "available"
        } else {
            "absent"
        }
    );
    #[cfg(target_os = "macos")]
    {
        let target = format!("gui/{}/{}", unsafe { libc::getuid() }, label(socket));
        let loaded = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()?;
        println!(
            "login job: {}",
            if loaded.status.success() {
                "loaded"
            } else {
                "not loaded"
            }
        );
    }
    #[cfg(target_os = "linux")]
    {
        let state = Command::new("systemctl")
            .args(["--user", "is-active"])
            .arg(path.file_name().context("unit filename")?)
            .output();
        let state = state
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        println!(
            "login job: {}",
            state
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("user service manager unavailable")
        );
    }
    Ok(())
}

pub fn command_help() -> Result<()> {
    bail!("usage: pmux login <enable|disable|status|sync|run>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_escapes_expansion_and_preserves_detached_daemon_on_disable() {
        let unit = render_systemd_unit(
            Path::new("/run/a %i $USER.sock"),
            Path::new("/opt/bin $x/pmux"),
        )
        .unwrap();
        assert!(
            unit.contains(
                "ExecStart=\"/opt/bin $$x/pmux\" --socket \"/run/a %%i $$USER.sock\" login run\n"
            ),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure\nRestartSec=10\nKillMode=process\n"));
        assert!(render_systemd_unit(Path::new("/run/a\nb.sock"), Path::new("/bin/pmux")).is_err());
    }

    #[test]
    fn windows_login_launch_hides_the_console() {
        let script = Path::new(r"C:\Startup\dev.prismattyc.pmuxd.0123456789abcdef.vbs");
        let launch = windows_login_launch(
            Path::new(r"C:\Windows"),
            Path::new(r"C:\Program Files\Prismattyc\bin\pmux.exe"),
            Path::new(r"C:\prismattyc\default.sock"),
            script,
        )
        .unwrap();
        assert!(launch.target.ends_with("wscript.exe"));
        assert!(launch.arguments.contains("//B"));
        assert!(launch.arguments.contains("//Nologo"));
        assert!(launch.arguments.contains(script.to_str().unwrap()));
        assert!(launch.script.contains(", 0, False"));
        assert!(launch
            .script
            .contains(r"C:\Program Files\Prismattyc\bin\pmux.exe"));
        assert!(launch.script.contains(r"--socket"));
        assert_eq!(
            login_script_path(Path::new(
                r"C:\Startup\dev.prismattyc.pmuxd.0123456789abcdef.lnk"
            )),
            script
        );
        assert!(windows_login_launch(
            Path::new(r"C:\Windows"),
            Path::new("C:\\bad\"pmux.exe"),
            Path::new(r"C:\sock"),
            script,
        )
        .is_err());
        assert!(windows_login_launch(
            Path::new(r"C:\Windows"),
            Path::new("C:\\bad\npmux.exe"),
            Path::new(r"C:\sock"),
            script,
        )
        .is_err());
    }

    #[test]
    fn remove_registration_deletes_the_hidden_login_script() {
        let dir = std::env::temp_dir().join(format!(
            "prismattyc-login-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("dev.prismattyc.pmuxd.0123456789abcdef.lnk");
        let script = login_script_path(&link);
        std::fs::write(&script, "CreateObject").unwrap();
        let removed = remove_registration(&link);
        let gone = !script.exists();
        let _ = std::fs::remove_dir_all(&dir);
        removed.unwrap();
        assert!(gone);
    }
}
