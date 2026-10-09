//! Windows icon and version resource for the packaged executables.
//!
//! Called from each consumer `build.rs`. The resource is compiled only when
//! the cargo target OS is Windows, so macOS and Linux builds stay unchanged.

use std::path::Path;

pub const PRODUCT_NAME: &str = "Prismattyc";
pub const COMPANY_NAME: &str = "Moonbase 2090 LLC";

/// File description stored in the version resource. One package shares one
/// resource across its binaries, so the mux tools share a description.
pub fn file_description(binary: &str) -> &'static str {
    match binary {
        "prismattyc" => "Prismattyc CLI",
        "pmux" | "pmuxd" | "pmux-attach" => "Prismattyc PMUX",
        "pmux-mcp" => "Prismattyc PMUX MCP",
        _ => "Prismattyc",
    }
}

fn icon_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release/prismattyc.ico")
}

/// Embed `scripts/release/prismattyc.ico` as icon resource ID 1, plus version
/// info. No effect unless the package under build targets Windows.
pub fn embed(description: &str) {
    let icon = icon_path();
    println!("cargo:rerun-if-changed={}", icon.display());
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let icon = icon.to_string_lossy().into_owned();
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&icon);
    resource.set("ProductName", PRODUCT_NAME);
    resource.set("CompanyName", COMPANY_NAME);
    resource.set("FileDescription", description);
    resource.set("FileVersion", &version);
    resource.set("ProductVersion", &version);
    resource
        .compile()
        .expect("embed Windows icon and version resource");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_distinguish_the_cli_from_the_host() {
        assert_eq!(file_description("prismattyc-host"), "Prismattyc");
        assert_eq!(file_description("prismattyc"), "Prismattyc CLI");
        assert_ne!(
            file_description("prismattyc-host"),
            file_description("prismattyc")
        );
        assert!(file_description("prismattyc").contains("Prismattyc"));
        assert!(file_description("prismattyc-host").contains("Prismattyc"));
        assert_eq!(file_description("pmux"), "Prismattyc PMUX");
        assert_eq!(file_description("pmuxd"), "Prismattyc PMUX");
        assert_eq!(file_description("pmux-attach"), "Prismattyc PMUX");
        assert_eq!(file_description("pmux-mcp"), "Prismattyc PMUX MCP");
        assert_eq!(COMPANY_NAME, "Moonbase 2090 LLC");
        assert_eq!(PRODUCT_NAME, "Prismattyc");
    }

    #[test]
    fn release_icon_is_a_windows_icon() {
        let bytes = std::fs::read(icon_path()).expect("release icon");
        assert_eq!(&bytes[..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes([bytes[4], bytes[5]]);
        assert!(count >= 5, "icon has {count} images");
    }
}
