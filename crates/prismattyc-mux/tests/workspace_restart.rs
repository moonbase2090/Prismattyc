//! Exercise process lifetime and durable state through the real daemon and CLI.
#![cfg(unix)]

use std::path::Path;

fn run(case: &str) -> serde_json::Value {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/native/login-restore-e2e.py");
    let bins = Path::new(env!("CARGO_BIN_EXE_pmux")).parent().unwrap();
    let output = prismattyc_process::spawn(
        prismattyc_process::capture_command("python3")
            .arg(script)
            .args(["--bins", bins.to_str().unwrap(), "--case", case]),
    )
    .and_then(std::process::Child::wait_with_output)
    .expect("run isolated restart test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("restart proof result")
}

#[test]
fn single_instance_restart_never_spawns_duplicate_children() {
    let proof = run("single-instance");
    assert_eq!(proof["child_launches"], 1);
}

#[test]
fn concurrent_cli_starts_wait_for_one_daemon() {
    let proof = run("concurrent");
    assert_eq!(proof["callers"], 6);
    assert_eq!(proof["single_daemon"], true);
}

#[test]
fn foreground_replay_keeps_the_shell_and_respects_policy_changes() {
    let proof = run("foreground");
    assert_eq!(proof["command"], "resumed");
    assert_eq!(proof["policy_change"], "honored");
}

#[test]
fn foreground_replay_survives_a_versioned_binary_update() {
    let proof = run("stable-replay");
    assert_eq!(proof["saved_command"], "fixture-agent 117");
    assert_eq!(proof["updated_binary"], "resumed");
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

#[test]
fn saved_space_migration_respects_explicit_opt_out() {
    let proof = run("migration");
    assert_eq!(proof["saved_space"], "restored");
    assert_eq!(proof["opt_out"], "honored");
}

#[test]
fn invalid_checkpoint_never_launches_children_or_replaces_saved_state() {
    let proof = run("corrupt");
    assert_eq!(proof["child_launches"], 1);
    assert_eq!(proof["checkpoint_preserved"], true);
}

#[test]
fn cli_only_install_stays_opted_out_after_saving_a_space_and_restarting() {
    let proof = run("new-install");
    assert_eq!(proof["first_space"], "still opted out");
    assert_eq!(proof["restart"], "still opted out");
}

#[test]
fn a_live_update_keeps_running_programs_their_screens_and_input() {
    let proof = run("live-update");
    assert_eq!(proof["program"], "kept");
    assert_eq!(proof["screen"], "kept");
    assert_eq!(proof["restart"], "updated_live");
}

#[test]
fn a_failed_live_update_leaves_the_server_or_restores_the_sessions() {
    let proof = run("live-update-fallbacks");
    assert_eq!(proof["probe_refused"], "unchanged");
    assert_eq!(proof["exec_failed"], "restored");
    assert_eq!(proof["import_failed"], "restored");
}

#[test]
fn a_live_update_keeps_the_first_pane_with_either_launch_form() {
    let proof = run("live-update-launch-forms");
    assert_eq!(
        proof["kept"],
        serde_json::json!(["/bin/sh -i", "-- /bin/sh -i"])
    );
}
