// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::io::{Read, Write};

use anyhow::{ensure, Context, Result};
use pmux_plugin_api::host::pmux::{activity::read, plugin::lifecycle};
use prismattyc_core::activity::{ActivityCursor, ActivityPage};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

const MAX_MESSAGE_BYTES: usize = 24 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BuildIdentity {
    core: String,
    revision: String,
    api: String,
    backend: String,
}

impl BuildIdentity {
    pub(super) fn current() -> Self {
        Self {
            core: prismattyc_core::package_version().into(),
            revision: prismattyc_core::git_hash().into(),
            api: pmux_plugin_api::API_VERSION.into(),
            backend: "pulley64".into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Bootstrap {
    pub build: BuildIdentity,
    pub component: Vec<u8>,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Operation {
    Validate,
    Prepare {
        context: lifecycle::Context,
        assets: BTreeMap<String, Vec<u8>>,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Bootstrap(Bootstrap),
    Activate,
    Invoke(lifecycle::Message),
    Stop,
    Endpoints(Result<Vec<read::Endpoint>, read::Error>),
    Page(Result<ActivityPage, read::Error>),
}

#[derive(Serialize, Deserialize)]
pub(super) enum Response {
    Validated,
    Prepared,
    Activated,
    Reply(lifecycle::Reply),
    Stopped,
    Endpoints,
    Poll {
        endpoint: u64,
        after: Option<ActivityCursor>,
        limit: u16,
    },
    Failed(String),
}

pub(super) fn write<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    let bytes = postcard::to_allocvec(value)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "worker message exceeds its byte budget"
    );
    output.write_all(&(bytes.len() as u32).to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

pub(super) fn read<T: DeserializeOwned>(input: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        length <= MAX_MESSAGE_BYTES,
        "worker message exceeds its byte budget"
    );
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    let (value, rest) = postcard::take_from_bytes(&bytes).context("invalid worker message")?;
    ensure!(rest.is_empty(), "trailing worker message bytes");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_framing_rejects_oversized_truncated_and_trailing_messages() {
        assert!(read::<Request>(&mut u32::MAX.to_le_bytes().as_slice()).is_err());
        assert!(read::<Request>(&mut [1, 0, 0, 0].as_slice()).is_err());
        assert!(read::<Request>(&mut [2, 0, 0, 0, 1, 99].as_slice()).is_err());
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Activate).unwrap();
        assert_eq!(bytes, [1, 0, 0, 0, 1]);
        assert!(matches!(
            read::<Request>(&mut bytes.as_slice()).unwrap(),
            Request::Activate
        ));
    }
}
