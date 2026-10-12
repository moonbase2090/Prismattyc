//! Start `../bin/<this file's stem>.exe` and exit with its status.
//!
//! The cmd directory holds copies of this program under the names
//! `prismattyc.exe`, `pmux.exe`, `pmuxd.exe`, `pmux-attach.exe`, and
//! `pmux-mcp.exe`. The copy's file name chooses the real program. Standard
//! streams are inherited. This process does not run `cmd.exe`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct ForwardTarget {
    name: String,
    path: PathBuf,
}

fn forward_name(exe: &Path) -> Option<&str> {
    let name = exe.file_stem()?.to_str()?;
    if name.is_empty()
        || name.contains(['/', '\\', ':', '.'])
        || name.eq_ignore_ascii_case("prismattyc-host")
    {
        return None;
    }
    Some(name)
}

fn forward_target(exe: &Path) -> Option<ForwardTarget> {
    let name = forward_name(exe)?;
    let cmd_dir = exe.parent()?;
    Some(ForwardTarget {
        name: name.to_string(),
        path: cmd_dir.join("..").join("bin").join(format!("{name}.exe")),
    })
}

fn forward_error(name: &str) -> String {
    format!("{name}: cannot start ..\\bin\\{name}.exe")
}

fn spawn_forward(target: &ForwardTarget) -> i32 {
    match Command::new(&target.path)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => {
            eprintln!("{}", forward_error(&target.name));
            1
        }
    }
}

fn main() {
    let code = match std::env::current_exe() {
        Ok(exe) => match forward_target(&exe) {
            Some(target) => spawn_forward(&target),
            None => {
                eprintln!("{}", forward_error("forwarder"));
                1
            }
        },
        Err(_) => {
            eprintln!("{}", forward_error("forwarder"));
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{forward_error, forward_target};
    use std::path::PathBuf;

    fn exe(parts: &[&str]) -> PathBuf {
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        path
    }

    #[test]
    fn target_is_the_sibling_bin_named_from_the_stem() {
        let pmux = forward_target(&exe(&["install", "cmd", "pmux.exe"])).unwrap();
        assert_eq!(pmux.name, "pmux");
        assert_eq!(
            pmux.path,
            exe(&["install", "cmd"])
                .join("..")
                .join("bin")
                .join("pmux.exe")
        );
        let attach = forward_target(&exe(&["install", "cmd", "pmux-attach.exe"])).unwrap();
        assert_eq!(attach.name, "pmux-attach");
        assert!(attach.path.ends_with("pmux-attach.exe"));
        assert!(forward_target(&exe(&["install", "cmd", "prismattyc-host.exe"])).is_none());
        assert!(forward_target(&exe(&["install", "cmd", ".exe"])).is_none());
    }

    #[test]
    fn error_names_the_relative_program_only() {
        let text = forward_error("pmux");
        assert_eq!(text, r"pmux: cannot start ..\bin\pmux.exe");
        assert!(!text.contains("install"));
        assert!(!text.contains("Users"));
    }
}
