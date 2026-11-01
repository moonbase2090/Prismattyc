// SPDX-License-Identifier: MIT

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wit");
    let mut resolve = wit_parser::Resolve::default();
    let (root, _) = resolve.push_dir("wit").expect("parse plug-in contracts");
    let package = &resolve.packages[root].name;
    let version = package.version.as_ref().expect("versioned plug-in API");
    println!(
        "cargo:rustc-env=PMUX_PLUGIN_API_NAME={}:{}",
        package.namespace, package.name
    );
    println!("cargo:rustc-env=PMUX_PLUGIN_API_VERSION={version}");
    let mut contracts = Vec::new();
    let mut imports = Vec::new();
    for (id, package) in resolve.packages.iter() {
        let version = package.name.version.as_ref().expect("versioned capability");
        for interface in package.interfaces.keys() {
            let name = format!(
                "{}:{}/{}",
                package.name.namespace, package.name.name, interface
            );
            imports.push(format!("{name}@{version}"));
            if id != root {
                contracts.push((name, version.to_string()));
            }
        }
    }
    contracts.sort();
    let mut generated = String::from("pub const CONTRACTS: &[(&str, &str)] = &[\n");
    for (name, version) in contracts {
        generated.push_str(&format!("    ({name:?}, {version:?}),\n"));
    }
    generated.push_str("];\n");
    imports.sort();
    generated.push_str("pub const IMPORT_CONTRACTS: &[&str] = &[\n");
    for name in imports {
        generated.push_str(&format!("    {name:?},\n"));
    }
    generated.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("build output"));
    fs::write(out.join("contracts.rs"), generated).expect("write capability metadata");
}
