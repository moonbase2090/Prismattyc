//! Exercise process lifetime and durable state through the real daemon and CLI.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn run(case: &str) -> serde_json::Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/native/login-restore-e2e.py");
    let bins = Path::new(env!("CARGO_BIN_EXE_pmux")).parent().unwrap();
    let output = Command::new("python3")
        .arg(script)
        .args(["--bins", bins.to_str().unwrap(), "--case", case])
        .output()
        .expect("run isolated restart test");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("restart proof result")
}

#[test]
fn single_instance_restart_never_spawns_duplicate_children() {
    let proof = run("single-instance");
    assert_eq!(proof["child_launches"], 1);
}

#[test]
fn reboot_restores_saved_sessions_and_mail() {
    let proof = run("reboot");
    assert_eq!(proof["sessions"], 3);
    assert_eq!(proof["mail"], "recovered");
    assert_eq!(proof["space"], "restored");
}

#[test]
fn crashed_daemon_restarts_but_stop_does_not() {
    let proof = run("supervisor");
    assert_eq!(proof["daemon_replaced"], true);
    assert_eq!(proof["intentional_stop"], "honored");
}
