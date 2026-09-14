//! `prism --version` / `-V` and `--help` / `-h` short-circuit before any PTY
//! spawn (so they need no TTY), print to stdout, and exit 0. After `--`, the
//! same tokens are the child program name, not host flags.

#![cfg(unix)]

use std::process::{Command, Output};

fn prism(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prismattyc"))
        .args(args)
        .output()
        .expect("run prism")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn version_flag_prints_package_version_and_exits_zero() {
    for flag in ["--version", "-V"] {
        let out = prism(&[flag]);
        assert!(out.status.success(), "{flag}: exit {:?}", out.status.code());
        let text = stdout(&out);
        // help2man expects "<name> <version>" as the first line.
        assert!(
            text.starts_with("prismattyc "),
            "{flag}: version line must start with `prismattyc `: {text:?}"
        );
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "{flag}: must contain {}: {text:?}",
            env!("CARGO_PKG_VERSION")
        );
    }
}

#[test]
fn help_flag_prints_synopsis_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let out = prism(&[flag]);
        assert!(out.status.success(), "{flag}: exit {:?}", out.status.code());
        let text = stdout(&out);
        assert!(text.contains("prism"), "{flag}: {text:?}");
        assert!(
            text.contains("--experimental-rich"),
            "{flag}: help must list the real flag: {text:?}"
        );
        assert!(
            text.to_lowercase().contains("usage"),
            "{flag}: help needs a usage line: {text:?}"
        );
    }
}

#[test]
fn version_after_separator_is_the_child_program_not_a_flag() {
    // `prism -- --version` must NOT print prism's version; it tries to launch a
    // program literally named "--version", which fails to spawn.
    let out = prism(&["--", "--version"]);
    assert!(
        !out.status.success(),
        "post-`--` --version must be treated as a (missing) child program"
    );
    assert!(
        !stdout(&out).starts_with("prism "),
        "post-`--` --version must not print prism's version: {:?}",
        stdout(&out)
    );
}
