//! The registered host's theme colors, for plug-in views drawn in its panes.
//!
//! The host writes `{stem}.host.theme.json` beside its pid file whenever its
//! theme changes. Readers trust the file only while the host that wrote it is
//! still the registered live host.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::host_register::{host_pid_path_from_socket, live_host_pid};

/// The colors a host paints terminal panes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeColors {
    /// Whether the theme is a dark one.
    pub dark: bool,
    /// Default text color.
    pub foreground: [u8; 3],
    /// Default pane background.
    pub background: [u8; 3],
    /// ANSI colors 0 to 15.
    pub ansi: [[u8; 3]; 16],
}

#[derive(Serialize, Deserialize)]
struct ThemeFile {
    host_pid: u32,
    colors: ThemeColors,
}

fn theme_path(pid_path: &Path) -> PathBuf {
    pid_path.with_extension("theme.json")
}

/// Publish atomically. Only the registered host may write its theme.
pub fn publish(pid_path: &Path, pid: u32, colors: &ThemeColors) -> io::Result<()> {
    if live_host_pid(pid_path) != Some(pid) {
        return Err(io::Error::other("theme host identity mismatch"));
    }
    let path = theme_path(pid_path);
    let temporary = path.with_extension(format!("json.{pid}.tmp"));
    let file = ThemeFile {
        host_pid: pid,
        colors: *colors,
    };
    crate::private_fs::write(&temporary, serde_json::to_vec(&file)?)?;
    fs::rename(temporary, path)
}

/// The registered live host's theme, or `None` when no live host has published one.
#[must_use]
pub fn read(socket: &Path) -> Option<ThemeColors> {
    let pid_path = host_pid_path_from_socket(socket);
    let pid = live_host_pid(&pid_path)?;
    let file: ThemeFile = serde_json::from_slice(&fs::read(theme_path(&pid_path)).ok()?).ok()?;
    (file.host_pid == pid).then_some(file.colors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_register::register_host_pid;

    fn colors(dark: bool) -> ThemeColors {
        let mut ansi = [[0; 3]; 16];
        for (index, color) in ansi.iter_mut().enumerate() {
            *color = [index as u8, 0x40, 0x80];
        }
        ThemeColors {
            dark,
            foreground: [0x11, 0x22, 0x33],
            background: [0xfa, 0xfb, 0xfc],
            ansi,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pmux-host-theme-{name}-{}-{}",
            std::process::id(),
            crate::host_render_status::unix_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_registered_host_publishes_its_theme_and_a_reader_gets_the_latest() {
        let dir = scratch("latest");
        let socket = dir.join("pmux.sock");
        let pid_path = host_pid_path_from_socket(&socket);
        let pid = std::process::id();
        assert_eq!(read(&socket), None);
        register_host_pid(&pid_path, pid).unwrap();
        assert_eq!(read(&socket), None);
        publish(&pid_path, pid, &colors(true)).unwrap();
        assert_eq!(read(&socket), Some(colors(true)));
        publish(&pid_path, pid, &colors(false)).unwrap();
        assert_eq!(read(&socket), Some(colors(false)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_registered_host_can_publish_and_other_hosts_files_are_ignored() {
        let dir = scratch("owner");
        let socket = dir.join("pmux.sock");
        let pid_path = host_pid_path_from_socket(&socket);
        let pid = std::process::id();
        assert!(publish(&pid_path, pid, &colors(true)).is_err());
        register_host_pid(&pid_path, pid).unwrap();
        assert!(publish(&pid_path, pid + 1, &colors(true)).is_err());
        let other = ThemeFile {
            host_pid: pid + 1,
            colors: colors(true),
        };
        fs::write(theme_path(&pid_path), serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(read(&socket), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
