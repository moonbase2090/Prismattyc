// SPDX-License-Identifier: MIT

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use pmux_plugin_api::host::pmux::view::frame;
use pmux_plugin_api::{ValidatedFrame, MAX_FRAME_BYTES};

use super::{Reply, Request};
use crate::local_socket::{connect_timeout, UnixStream};
use crate::{
    ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData, PROTOCOL_VERSION,
};

/// One persistent frontend connection. Closing it revokes its views and exports.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next: u64,
    failed: bool,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Self> {
        let stream = connect_timeout(socket, Duration::from_secs(2))
            .context("connect to pmuxd; plug-in changes require a running daemon")?;
        stream.set_read_timeout(Some(Duration::from_secs(360)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            next: 1,
            failed: false,
        })
    }

    pub fn request(&mut self, operation: Request) -> Result<Reply> {
        ensure!(!self.failed, "plug-in connection is closed");
        let id = self.next;
        self.next = id.checked_add(1).context("plug-in request IDs exhausted")?;
        let response = self.exchange(
            ControlRequest::Plugin {
                version: PROTOCOL_VERSION,
                request_id: id,
                operation,
            },
            id,
        );
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.failed = true;
                let _ = self.writer.shutdown(std::net::Shutdown::Both);
                return Err(error);
            }
        };
        match response.body {
            ControlResponseBody::Ok {
                response: ControlResponseData::Plugin { reply },
            } => Ok(reply),
            ControlResponseBody::Error { error } => bail!("{}", error.message),
            _ => bail!("unexpected plug-in response kind"),
        }
    }

    fn exchange(&mut self, request: ControlRequest, expected_id: u64) -> Result<ControlResponse> {
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() <= crate::control::MAX_REQUEST_BYTES,
            "plug-in request exceeds its byte budget"
        );
        self.writer.write_all(&bytes)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        let mut bytes = Vec::new();
        (&mut self.reader)
            .take(crate::control::MAX_RESPONSE_BYTES as u64 + 1)
            .read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= crate::control::MAX_RESPONSE_BYTES && bytes.last() == Some(&b'\n'),
            "truncated or oversized plug-in response"
        );
        let response: ControlResponse = serde_json::from_slice(&bytes)?;
        ensure!(
            response.version == PROTOCOL_VERSION && response.request_id == expected_id,
            "mismatched plug-in response header"
        );
        Ok(response)
    }
}

pub fn decode_frame(encoded: &str, viewport: &frame::Viewport) -> Result<ValidatedFrame> {
    ensure!(
        encoded.len() <= MAX_FRAME_BYTES.div_ceil(3) * 4,
        "encoded frame exceeds its byte budget"
    );
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    ensure!(
        bytes.len() <= MAX_FRAME_BYTES,
        "frame exceeds its byte budget"
    );
    let (content, rest) = postcard::take_from_bytes(&bytes)?;
    ensure!(rest.is_empty(), "trailing typed frame bytes");
    Ok(ValidatedFrame::new(content, viewport)?)
}
