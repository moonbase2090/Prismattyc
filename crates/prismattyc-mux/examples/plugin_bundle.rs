// SPDX-License-Identifier: MIT
//! Developer/release artifact helper. It has no publication operation.

use std::io::Read;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use prismattyc_mux::plugins::{
    fetch_bundle, pin_candidate_bundle, verify_bundle, BundlePin, BundleSource, Manifest,
};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let pin = match args.first().map(String::as_str) {
        Some("verify") if args.len() == 2 || (args.len() == 3 && args[2] == "--candidate") => {
            verify_bundle(Path::new(&args[1]), args.len() == 3)?
        }
        Some("candidate") if args.len() == 3 => {
            pin_candidate_bundle(Path::new(&args[1]), args[2].clone())?
        }
        Some("pin") if args.len() == 3 => {
            fetch_bundle(&args[1], Path::new(&args[2]), None)?
        }
        Some("fetch") if args.len() == 3 => {
            let mut bytes = Vec::new();
            std::fs::File::open(&args[1])?.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
            let expected = BundlePin::parse(&bytes)?;
            let BundleSource::Official { ref tag } = expected.source else {
                bail!("fetch requires an official immutable catalog pin");
            };
            fetch_bundle(tag, Path::new(&args[2]), Some(&expected))?
        }
        Some("manifest") if args.len() == 2 => {
            let mut bytes = Vec::new();
            std::fs::File::open(Path::new(&args[1]).join("plugin.toml"))?
                .take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 64 * 1024, "manifest exceeds its byte budget");
            let manifest = Manifest::parse(&bytes).context("validate production package manifest")?;
            manifest.compatibility()?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
            return Ok(());
        }
        _ => bail!("usage: plugin_bundle verify DIR [--candidate] | candidate DIR SOURCE_COMMIT | pin OFFICIAL_TAG DIR | fetch PIN_FILE DIR | manifest SOURCE_DIR"),
    };
    println!("{}", serde_json::to_string_pretty(&pin)?);
    Ok(())
}
