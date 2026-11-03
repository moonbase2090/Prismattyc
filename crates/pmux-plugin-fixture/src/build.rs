// SPDX-License-Identifier: MIT
//! Compile the actual guest with the caller's Rust toolchain, never opaque bytes.

use anyhow::{ensure, Result};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static MODULE: OnceLock<Vec<u8>> = OnceLock::new();
static COMPONENT: OnceLock<Vec<u8>> = OnceLock::new();

pub fn module() -> &'static [u8] {
    MODULE.get_or_init(|| {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let target = std::env::temp_dir().join(format!("pmux-plugin-fixture-{}-{nonce}", std::process::id()));
        let result = (|| -> Result<Vec<u8>> {
            let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .current_dir(root)
                .args(["build", "-p", "pmux-plugin-fixture", "--release", "--target", "wasm32-unknown-unknown", "--locked"])
                .env("CARGO_TARGET_DIR", &target)
                .env_remove("RUSTFLAGS")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .output()?;
            ensure!(output.status.success(), "build the real component fixture; install wasm32-unknown-unknown for this toolchain:\n{}", String::from_utf8_lossy(&output.stderr));
            Ok(std::fs::read(target.join("wasm32-unknown-unknown/release/pmux_plugin_fixture.wasm"))?)
        })();
        let _ = std::fs::remove_dir_all(target);
        result.expect("build version-matched component fixture")
    })
}

pub fn component() -> &'static [u8] {
    COMPONENT.get_or_init(|| {
        wit_component::ComponentEncoder::default()
            .module(module())
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    })
}
