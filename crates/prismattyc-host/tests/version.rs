//! `prismattyc-host --version` / `-V` short-circuit before opening a window.

use std::process::{Command, Output};

fn host(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prismattyc-host"))
        .args(args)
        .output()
        .expect("run prismattyc-host")
}

#[test]
fn version_flag_prints_package_and_exits_zero() {
    for flag in ["--version", "-V"] {
        let out = host(&[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status.code());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.starts_with("prismattyc-host "), "{flag}: {text:?}");
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "{flag}: must contain {}: {text:?}",
            env!("CARGO_PKG_VERSION")
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("failed to start"),
            "{flag} must not spawn a window: {stderr:?}"
        );
    }
}

fn isolated_config_path(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "pt-84-host-cli-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    (dir, path)
}

fn host_with_config(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prismattyc-host"))
        .args(args)
        .env("PRISMATTYC_CONFIG", config)
        .output()
        .expect("run prismattyc-host")
}

#[test]
fn version_and_help_do_not_create_default_config() {
    for flag in ["--version", "-V", "--help", "-h"] {
        let (dir, path) = isolated_config_path(flag.trim_start_matches('-'));
        let out = host_with_config(&path, &[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status.code());
        assert!(
            !path.exists(),
            "{flag} must not create {}: exists={}",
            path.display(),
            path.exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn write_config_custom_path_does_not_create_default_config() {
    let (dir, default_path) = isolated_config_path("write-default");
    let custom = dir.join("custom.toml");
    let out = host_with_config(&default_path, &["--write-config", custom.to_str().unwrap()]);
    assert!(out.status.success(), "{:?}", out.status.code());
    assert!(custom.exists(), "custom path should be written");
    assert!(
        !default_path.exists(),
        "--write-config CUSTOM must not also write PRISMATTYC_CONFIG"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_config_prints_template_and_exits_zero() {
    let out = host(&["--write-config", "-"]);
    assert!(out.status.success(), "{:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("theme = \"prismattyc-default\""));
    assert!(text.contains("[mux]"));
    assert!(text.contains("[keys]"));
    assert!(text.contains("split_right"));
    assert!(text.contains("[a11y]"));
    assert!(text.contains("os_tree = true"));
    assert!(text.contains("announce = true"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("failed to start"),
        "--write-config must not spawn a window: {stderr:?}"
    );
}

#[test]
fn write_config_without_path_prints_template_and_exits_zero() {
    let out = host(&["--write-config"]);
    assert!(out.status.success(), "{:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("theme = \"prismattyc-default\""));
    assert!(text.contains("\n[mux]\n"));
    assert!(text.contains("\n[keys]\n"));
    assert!(text.contains("\n[a11y]\n"));
}

#[test]
fn write_config_file_contains_complete_template() {
    let (dir, default_path) = isolated_config_path("write-file");
    let custom = dir.join("custom.toml");
    let out = host_with_config(&default_path, &["--write-config", custom.to_str().unwrap()]);
    assert!(out.status.success(), "{:?}", out.status.code());
    let text = std::fs::read_to_string(&custom).expect("custom config should be readable");
    assert!(text.starts_with("# Prismattyc host config."));
    assert!(text.contains("\n[mux]\n"));
    assert!(text.contains("\n[keys]\n"));
    assert!(text.contains("\n[a11y]\n"));
    assert!(text.contains("split_right ="));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_config_merge_preserves_nested_values_and_adds_missing_keys() {
    let (dir, default_path) = isolated_config_path("write-merge");
    let custom = dir.join("custom.toml");
    std::fs::write(
        &custom,
        "theme = \"user-theme\"\n\n[mux]\ninstance = \"user-instance\"\n\n[keys]\nsplit_right = \"ctrl+alt+enter\"\n\n[a11y]\nannounce = false\n",
    )
    .unwrap();
    let out = host_with_config(
        &default_path,
        &["--write-config", custom.to_str().unwrap(), "--merge"],
    );
    assert!(out.status.success(), "{:?}", out.status.code());
    let text = std::fs::read_to_string(&custom).expect("merged config should be readable");
    assert!(text.contains("theme = \"user-theme\""));
    assert!(text.contains("instance = \"user-instance\""));
    assert!(text.contains("split_right = \"ctrl+alt+enter\""));
    assert!(text.contains("split_down ="), "missing nested key: {text}");
    assert!(text.contains("announce = false"));
    assert!(text.contains("os_tree = true"));
    assert!(text.contains("attach_on_new = true"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_lists_mux_and_a11y_tables() {
    let out = host(&["--help"]);
    assert!(out.status.success(), "{:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("[mux]"), "help missing [mux]: {text}");
    assert!(
        text.contains("mux.instance"),
        "help missing mux keys: {text}"
    );
    assert!(text.contains("[a11y]"), "help missing [a11y]: {text}");
    assert!(
        text.contains("a11y.os_tree"),
        "help missing a11y keys: {text}"
    );
}
