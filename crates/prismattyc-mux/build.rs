fn main() {
    prismattyc_winres::embed(prismattyc_winres::file_description("pmux"));
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=PRISMATTYC_PLUGIN_BUNDLE_DIR");
    let mut generated = String::new();
    if let Some(bundle) = std::env::var_os("PRISMATTYC_PLUGIN_BUNDLE_DIR") {
        let bundle = std::path::PathBuf::from(bundle)
            .canonicalize()
            .expect("plug-in bundle directory");
        let index = bundle.join("pmux-plugins.json");
        println!("cargo:rerun-if-changed={}", index.display());
        let bytes = std::fs::read(&index).expect("read bundled plug-in catalog");
        let catalog: serde_json::Value =
            serde_json::from_slice(&bytes).expect("parse bundled plug-in catalog");
        generated.push_str(&format!("const BUNDLED_INDEX: &[u8] = include_bytes!({:?});\nconst BUNDLED_PACKAGES: &[(&str, &[u8])] = &[\n", index));
        for package in catalog["packages"].as_array().expect("catalog packages") {
            let sha = package["sha256"].as_str().expect("package digest");
            assert!(
                sha.len() == 64
                    && sha
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid bundle digest"
            );
            let path = bundle.join(format!("{sha}.pmux-plugin"));
            println!("cargo:rerun-if-changed={}", path.display());
            generated.push_str(&format!("({sha:?}, include_bytes!({path:?})),\n"));
        }
        generated.push_str("];\n");
        let pin = bundle.join("bundle-pin.json");
        let receipt = bundle.join("plugin-build-receipt.json");
        assert_eq!(
            pin.is_file(),
            receipt.is_file(),
            "bundle provenance is incomplete"
        );
        for (name, path) in [("BUNDLED_PIN", pin), ("BUNDLED_RECEIPT", receipt)] {
            println!("cargo:rerun-if-changed={}", path.display());
            if path.is_file() {
                generated.push_str(&format!(
                    "const {name}: Option<&[u8]> = Some(include_bytes!({path:?}));\n"
                ));
            } else {
                generated.push_str(&format!("const {name}: Option<&[u8]> = None;\n"));
            }
        }
    } else {
        generated.push_str("const BUNDLED_INDEX: &[u8] = br#\"{\"version\":1,\"packages\":[]}\"#;\nconst BUNDLED_PACKAGES: &[(&str, &[u8])] = &[];\n");
    }
    if std::env::var_os("PRISMATTYC_PLUGIN_BUNDLE_DIR").is_none() {
        generated.push_str("const BUNDLED_PIN: Option<&[u8]> = None;\nconst BUNDLED_RECEIPT: Option<&[u8]> = None;\n");
    }
    std::fs::write(output.join("plugin_catalog.rs"), generated)
        .expect("write core plug-in resources");
}
