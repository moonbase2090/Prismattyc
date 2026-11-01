// SPDX-License-Identifier: MIT
//! Owner-configured native transports. These definitions never enter a guest.

use std::path::PathBuf;

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointSource {
    Socket { path: PathBuf },
    CommandQuery { argv: Vec<String> },
    CommandTail { argv: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub source: EndpointSource,
}

pub fn validate_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label.len() <= 64
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "endpoint labels require 1 to 64 ASCII letters, digits, '-', '_' or '.'"
    );
    Ok(())
}

impl EndpointSource {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Socket { path } => ensure!(
                path.is_absolute() && path.as_os_str().len() <= 4096,
                "endpoint sockets require an absolute bounded path"
            ),
            Self::CommandQuery { argv } | Self::CommandTail { argv } => {
                ensure!(
                    !argv.is_empty() && argv.len() <= 256,
                    "endpoint commands require 1 to 256 arguments"
                );
                ensure!(
                    !argv[0].is_empty()
                        && argv
                            .iter()
                            .all(|argument| argument.len() <= 4096 && !argument.contains('\0'))
                        && argv.iter().map(String::len).sum::<usize>() <= 16 * 1024,
                    "endpoint command exceeds its argument budget"
                );
            }
        }
        Ok(())
    }
}

impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        validate_label(&self.label)?;
        ensure!(self.label != "local", "local is reserved for this daemon");
        ensure!(
            self.id.len() == 32 && self.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid endpoint identity"
        );
        self.source.validate()
    }
}
