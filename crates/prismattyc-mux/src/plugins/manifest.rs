// SPDX-License-Identifier: MIT

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use pmux_plugin_api::{API_VERSION, CONTRACTS};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u16,
    pub id: String,
    pub version: String,
    pub plugin_api: String,
    pub component: String,
    #[serde(default)]
    pub requires: BTreeMap<String, String>,
    #[serde(default)]
    pub optional: BTreeMap<String, String>,
    #[serde(default)]
    pub assets: Vec<String>,
    pub notices: Vec<String>,
    pub contributions: Vec<Contribution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Contribution {
    View {
        id: String,
        title: String,
    },
    Command {
        id: String,
        #[serde(default)]
        mcp_name: Option<String>,
        input_schema: String,
        output_schema: String,
    },
}

impl Contribution {
    pub fn id(&self) -> &str {
        match self {
            Self::View { id, .. } | Self::Command { id, .. } => id,
        }
    }
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 64 * 1024,
            "plug-in manifest exceeds its byte budget"
        );
        let manifest: Self = toml::from_str(std::str::from_utf8(bytes)?)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.manifest_version == 1,
            "unsupported plug-in manifest version"
        );
        validate_plugin_id(&self.id)?;
        Version::parse(&self.version).context("invalid plug-in version")?;
        VersionReq::parse(&self.plugin_api)?;
        ensure!(
            valid_path(&self.component) && self.component.ends_with(".wasm"),
            "invalid component path"
        );
        ensure!(
            !self.contributions.is_empty() && self.contributions.len() <= 64,
            "plug-in contributions exceed their limit"
        );
        ensure!(
            self.assets.len() <= 128 && !self.notices.is_empty() && self.notices.len() <= 16,
            "invalid plug-in asset/notice count"
        );
        for (name, range) in &self.requires {
            ensure!(
                name.len() <= 128 && name.is_ascii() && !name.chars().any(char::is_control),
                "invalid required capability name"
            );
            VersionReq::parse(range)?;
        }
        for (name, range) in &self.optional {
            ensure!(
                !self.requires.contains_key(name),
                "capability declared as both required and optional"
            );
            ensure!(
                name.len() <= 128 && name.is_ascii() && !name.chars().any(char::is_control),
                "invalid optional capability name"
            );
            VersionReq::parse(range)?;
        }
        let mut ids = BTreeSet::new();
        let mut exports = BTreeSet::new();
        for contribution in &self.contributions {
            ensure!(
                valid_id(contribution.id(), 64) && ids.insert(contribution.id()),
                "invalid or duplicate contribution ID"
            );
            match contribution {
                Contribution::View { title, .. } => {
                    ensure!(
                        !title.is_empty()
                            && title.len() <= 128
                            && !title.chars().any(char::is_control),
                        "invalid view title"
                    );
                    ensure!(
                        self.requires.contains_key("pmux:view/frame"),
                        "view contribution requires its capability"
                    );
                }
                Contribution::Command {
                    mcp_name,
                    input_schema,
                    output_schema,
                    ..
                } => {
                    ensure!(
                        self.requires.contains_key("pmux:tool/call"),
                        "command contribution requires its capability"
                    );
                    ensure!(
                        valid_path(input_schema) && valid_path(output_schema),
                        "invalid command schema path"
                    );
                    if let Some(name) = mcp_name {
                        ensure!(
                            valid_id(name, 64) && exports.insert(name),
                            "invalid or duplicate MCP export"
                        );
                    }
                }
            }
        }
        let mut files = BTreeSet::from([self.component.as_str(), "plugin.toml"]);
        for name in self.assets.iter().chain(&self.notices) {
            ensure!(
                valid_path(name) && files.insert(name.as_str()),
                "invalid or duplicate package file"
            );
        }
        for contribution in &self.contributions {
            if let Contribution::Command {
                input_schema,
                output_schema,
                ..
            } = contribution
            {
                ensure!(
                    !files.contains(input_schema.as_str())
                        && !files.contains(output_schema.as_str()),
                    "schema aliases a non-schema package file"
                );
            }
        }
        Ok(())
    }

    pub fn compatibility(&self) -> Result<()> {
        ensure!(
            VersionReq::parse(&self.plugin_api)?.matches(&Version::parse(API_VERSION)?),
            "incompatible plug-in API"
        );
        for (name, range) in &self.requires {
            let (_, version) = CONTRACTS
                .iter()
                .find(|(contract, _)| contract == name)
                .context("unsupported required plug-in capability")?;
            ensure!(
                VersionReq::parse(range)?.matches(&Version::parse(version)?),
                "incompatible plug-in capability"
            );
        }
        Ok(())
    }

    pub fn files(&self) -> BTreeSet<&str> {
        let mut files = BTreeSet::from(["plugin.toml", self.component.as_str()]);
        files.extend(self.assets.iter().chain(&self.notices).map(String::as_str));
        for contribution in &self.contributions {
            if let Contribution::Command {
                input_schema,
                output_schema,
                ..
            } = contribution
            {
                files.extend([input_schema.as_str(), output_schema.as_str()]);
            }
        }
        files
    }
}

pub(super) fn validate_plugin_id(id: &str) -> Result<()> {
    ensure!(
        id.len() <= 128
            && id.split('.').count() >= 3
            && id.split('.').all(|part| valid_id(part, 64)),
        "invalid publisher-qualified plug-in ID"
    );
    Ok(())
}

fn valid_id(id: &str, max: usize) -> bool {
    !id.is_empty()
        && id.len() <= max
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.split('/').count() <= 8
        && path.split('/').all(|part| {
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            !part.is_empty()
                && part.len() <= 128
                && part != "."
                && part != ".."
                && !part.ends_with('.')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> &'static str {
        r#"
manifest_version = 1
id = "org.moonbase.example"
version = "1.2.3"
plugin_api = ">=2, <3"
component = "view.wasm"
notices = ["LICENSE"]
[requires]
"pmux:view/frame" = "^2"
[[contributions]]
kind = "view"
id = "main"
title = "Example"
"#
    }

    #[test]
    fn manifests_reject_unknown_authority_paths_and_duplicate_contributions() {
        assert_eq!(
            Manifest::parse(source().as_bytes()).unwrap().id,
            "org.moonbase.example"
        );
        for (old, new) in [
            ("component = \"view.wasm\"", "component = \"../view.wasm\""),
            ("\"pmux:view/frame\"", "\"pmux:pty/write\""),
            (
                "title = \"Example\"",
                "title = \"Example\"\nexecutable = \"run.exe\"",
            ),
        ] {
            assert!(Manifest::parse(source().replace(old, new).as_bytes()).is_err());
        }
        assert!(Manifest::parse(
            format!(
                "{}\n[[contributions]]\nkind = \"view\"\nid = \"main\"\ntitle = \"Again\"",
                source()
            )
            .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn incompatible_manifests_remain_readable_without_becoming_executable() {
        let manifest = Manifest::parse(
            source()
                .replace("plugin_api = \">=2, <3\"", "plugin_api = \"^1\"")
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert!(manifest.compatibility().is_err());
        // A package built for the 1.x view event, as the published 1.0.0
        // activity monitor is, cannot link against the 2.0 event.
        assert!(Manifest::parse(
            source()
                .replace(
                    "\"pmux:view/frame\" = \"^2\"",
                    "\"pmux:view/frame\" = \"^1.0.0\""
                )
                .as_bytes(),
        )
        .unwrap()
        .compatibility()
        .is_err());
        assert!(Manifest::parse(source().as_bytes())
            .unwrap()
            .compatibility()
            .is_ok());
    }

    #[test]
    fn package_paths_are_portable_regular_file_names() {
        assert!(valid_path("schemas/poll-input.json"));
        for path in [
            "/tmp/run",
            "a/../b",
            "a\\b",
            "a:b",
            "a//b",
            "CON",
            "a/NUL.txt",
            "com1.wasm",
            "a.",
            ".",
        ] {
            assert!(!valid_path(path), "{path}");
        }
    }
}
