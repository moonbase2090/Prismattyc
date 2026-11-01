// SPDX-License-Identifier: MIT
//! Build release resources before compiling the native core with
//! PRISMATTYC_PLUGIN_BUNDLE_DIR pointing at the output directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use prismattyc_mux::plugins::{pack, CatalogEntry, Manifest};
use sha2::{Digest, Sha256};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.len() >= 3 && args.len() % 2 == 1,
        "usage: package_plugin OUTPUT_DIR SOURCE_DIR CORE_WASM [SOURCE_DIR CORE_WASM ...]"
    );
    let output = PathBuf::from(&args[0]);
    std::fs::create_dir_all(&output)?;
    let mut catalog = Vec::new();
    let runtime = pmux_plugin_runtime::Runtime::new()?;
    for pair in args[1..].chunks_exact(2) {
        let source = Path::new(&pair[0]);
        let manifest = Manifest::parse(&std::fs::read(source.join("plugin.toml"))?)?;
        manifest.compatibility()?;
        let module = std::fs::read(&pair[1])?;
        let component = wit_component::ComponentEncoder::default()
            .module(&module)?
            .validate(true)
            .encode()?;
        runtime.validate_component(&component)?;
        let mut files = BTreeMap::new();
        for name in manifest.files() {
            let bytes = if name == manifest.component {
                component.clone()
            } else {
                let path = source.join(name);
                let metadata = std::fs::symlink_metadata(&path)?;
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "package source is not a regular file"
                );
                ensure!(
                    metadata.len() <= 1024 * 1024,
                    "package source exceeds its file budget"
                );
                std::fs::read(&path).with_context(|| format!("read package source {name}"))?
            };
            files.insert(name.to_owned(), bytes);
        }
        let bytes = pack(files)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        std::fs::write(output.join(format!("{sha256}.pmux-plugin")), &bytes)?;
        catalog.push(CatalogEntry {
            id: manifest.id,
            version: manifest.version,
            sha256,
            bytes: bytes.len() as u64,
        });
    }
    catalog.sort_by(|a, b| (&a.id, &a.version).cmp(&(&b.id, &b.version)));
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({"version":1,"packages":catalog}))?;
    std::fs::write(output.join("pmux-plugins.json"), bytes)?;
    println!("Packaged {} plug-in version(s)", catalog.len());
    Ok(())
}
