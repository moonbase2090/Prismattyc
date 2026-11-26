// SPDX-License-Identifier: MIT
//! A running daemon asks the installed `pmuxd` which official plug-ins it bundles.
//! This runs in its own process: it starts child processes, and in the library's test
//! binary a child forked at the wrong moment can briefly hold another test's lock.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use prismattyc_mux::plugins::{bundled_defaults, bundled_newer_than, Bundled, BundledPlugin};

const MONITOR: &str = "org.moonbase.activity-monitor";

/// Install a script through a child that has exited, so this process never holds the
/// executed file open for writing.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let source = dir.join(format!("{name}.source"));
    std::fs::write(&source, format!("#!/bin/sh\n{body}\n")).unwrap();
    let installed = prismattyc_process::status(
        std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(&path),
    )
    .unwrap();
    assert!(installed.success(), "install the fake pmuxd");
    std::fs::remove_file(&source).unwrap();
    path
}

#[test]
fn the_installed_daemon_is_asked_only_when_newer_and_a_bad_answer_is_an_error() {
    let dir = std::env::temp_dir().join(format!("pmux-bundled-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let asked = dir.join("asked");

    let newer = script(
        &dir,
        "pmuxd-newer",
        &format!(
            r#"case "$1" in --version) echo 'pmuxd 9.0.0 (abc)';; --bundled-plugins) printf '{{"plugins":[{{"id":"{MONITOR}","version":"9.0.0"}}]}}\n';; *) exit 9;; esac"#
        ),
    );
    assert_eq!(
        bundled_newer_than(Some(&newer), "1.0.0").unwrap(),
        Bundled {
            plugins: vec![BundledPlugin {
                id: MONITOR.into(),
                version: "9.0.0".into()
            }]
        }
    );

    // A newer daemon that cannot answer is an error, not "nothing pending".
    let broken = script(
        &dir,
        "pmuxd-broken",
        r#"case "$1" in --version) echo 'pmuxd 9.0.0 (abc)';; *) exit 2;; esac"#,
    );
    let error = bundled_newer_than(Some(&broken), "1.0.0").unwrap_err();
    assert!(
        format!("{error:#}").contains("bundled plug-ins"),
        "{error:#}"
    );

    // A daemon that is not newer is never run with the new flag: an older release
    // would take it for a program to start.
    let older = script(
        &dir,
        "pmuxd-older",
        &format!(
            r#"case "$1" in --version) echo 'pmuxd 1.0.0 (abc)';; *) touch '{}';; esac"#,
            asked.display()
        ),
    );
    assert_eq!(
        bundled_newer_than(Some(&older), "1.0.0").unwrap(),
        bundled_defaults().unwrap()
    );
    assert!(
        !asked.exists(),
        "a daemon that is not newer was run with --bundled-plugins"
    );
    // With no installed daemon to ask, this binary's own bundles are used.
    assert_eq!(
        bundled_newer_than(None, "1.0.0").unwrap(),
        bundled_defaults().unwrap()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
