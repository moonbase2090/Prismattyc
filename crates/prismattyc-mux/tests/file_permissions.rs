//! File permission hardening exercised through the daemon and state writers.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn restart_repairs_modes_and_preserves_sessions_mail_and_pane_logs() {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/native/file-permissions-e2e.py");
    let output = Command::new("python3")
        .arg(script)
        .arg("--bins")
        .arg(Path::new(env!("CARGO_BIN_EXE_pmux")).parent().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["directories"], "0700");
    assert_eq!(proof["files"], "0600");
    assert_eq!(proof["sessions"], "preserved");
    assert_eq!(proof["mail"], "claimed and committed");
    assert_eq!(proof["pane_log"], "preserved");
}

#[test]
fn host_and_mail_writers_create_private_files_and_repair_legacy_modes() {
    use prismattyc_mux::{attach_tabs, host_register, host_render_status, mailbox};
    let dir = std::env::temp_dir().join(format!("pmux-writer-modes-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let pid_path = dir.join("pmux.host.pid");
    let pid = std::process::id();
    assert!(host_register::register_host_pid(&pid_path, pid).unwrap());
    let render = serde_json::json!({"host_pid": pid, "schema_version": 1});
    host_render_status::publish(&pid_path, pid, &render).unwrap();
    let view = dir.join("pmux.attach-tabs.json");
    attach_tabs::save(&view, &attach_tabs::AttachTabsFile::default()).unwrap();
    let db = dir.join("mail.db");
    drop(mailbox::Store::open(&db).unwrap());
    let paths = [&pid_path, &view, &db];
    for path in paths {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert!(host_register::register_host_pid(&pid_path, pid).unwrap());
    attach_tabs::save(&view, &attach_tabs::AttachTabsFile::default()).unwrap();
    drop(mailbox::Store::open(&db).unwrap());
    for path in paths
        .into_iter()
        .chain([&pid_path.with_extension("render.json")])
    {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(host_register::live_host_pid(&pid_path), Some(pid));
    fs::remove_dir_all(dir).unwrap();
}
