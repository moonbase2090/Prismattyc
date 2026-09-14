//! `pmux-mcp --version` / `-V` short-circuit before connecting.

use std::process::{Command, Output};

fn mcp(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pmux-mcp"))
        .args(args)
        .output()
        .expect("run pmux-mcp")
}

#[test]
fn version_flag_prints_package_and_exits_zero() {
    for flag in ["--version", "-V"] {
        let out = mcp(&[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status.code());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.starts_with("pmux-mcp "), "{flag}: {text:?}");
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "{flag}: must contain {}: {text:?}",
            env!("CARGO_PKG_VERSION")
        );
        assert!(
            out.stderr.is_empty(),
            "{flag} must not require identity: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn help_flag_lists_version() {
    let out = mcp(&["--help"]);
    assert!(out.status.success(), "--help: {:?}", out.status.code());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--version"), "{text:?}");
    assert!(text.contains("-V"), "{text:?}");
}
