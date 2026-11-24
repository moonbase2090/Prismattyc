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

/// Shuts down an owned frontend connection without waiting for a blocked RPC.
pub struct ConnectionCloser(UnixStream);

impl ConnectionCloser {
    pub fn close(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    pub fn close_handle(&self) -> Result<ConnectionCloser> {
        Ok(ConnectionCloser(self.writer.try_clone()?))
    }
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
                plugin_api: pmux_plugin_api::API_VERSION.into(),
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
        if response.request_id == 0
            && matches!(&response.body, ControlResponseBody::Error { error } if error.code == crate::ControlErrorCode::InvalidJson)
        {
            bail!("pmuxd has no compatible plug-in API; update Prismattyc");
        }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_frontend_can_cancel_an_rpc_without_waiting_for_the_daemon_reply() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "pc-{}",
            &super::super::package::nonce().unwrap()[..16]
        ));
        super::super::package::private_directory(&root).unwrap();
        let socket = root.join("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).unwrap()["operation"]
                    ["operation"],
                "list"
            );
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        let mut client = Client::connect(&socket).unwrap();
        let closer = client.close_handle().unwrap();
        let (send, receive) = mpsc::channel();
        let request = std::thread::spawn(move || {
            send.send(client.request(Request::List).is_err()).unwrap();
        });
        assert!(matches!(
            receive.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        closer.close();
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        request.join().unwrap();
        server.join().unwrap();
        drop(closer);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use pmux_plugin_api::MAX_TILES;

    fn blank() -> frame::Cell {
        frame::Cell {
            text: " ".into(),
            width: 1,
            foreground: frame::Color::Default,
            background: frame::Color::Default,
            attributes: frame::Attributes {
                bold: false,
                dim: false,
                italic: false,
                blink: false,
                reverse: false,
                hidden: false,
                strike: false,
                overline: false,
            },
            underline: frame::Underline::None,
        }
    }

    /// The most tiles the API allows, filling the frame budget, at the default
    /// macOS cell size.
    fn largest() -> (frame::Content, frame::Viewport) {
        let pixels = frame::PixelSize {
            width: 18,
            height: 41,
        };
        let viewport = frame::Viewport {
            columns: 200,
            rows: 60,
            pixels: Some(pixels),
        };
        let cells = 200 * 60;
        // The validator's own accounting: 32 per cell, 256 per frame, the text,
        // then the pixels and 64 per tile.
        let room = MAX_FRAME_BYTES - (cells * 33 + 256) - MAX_TILES * 64;
        let tile_bytes = 18 * 41 * 4;
        let columns = (room / MAX_TILES / tile_bytes) as u16;
        let tiles = (0..MAX_TILES)
            .map(|index| {
                let index = index as u16;
                let per_row = 200 / columns;
                frame::Tile {
                    x: index % per_row * columns,
                    y: index / per_row,
                    columns,
                    rows: 1,
                    pixel_width: u32::from(columns) * 18,
                    pixel_height: 41,
                    rgba: vec![0x7f; usize::from(columns) * tile_bytes],
                }
            })
            .collect();
        let content = frame::Content {
            columns: 200,
            rows: 60,
            cells: vec![blank(); cells],
            tiles,
            cursor: None,
        };
        (content, viewport)
    }

    #[test]
    fn the_largest_valid_frame_crosses_one_control_response() -> Result<()> {
        let (content, viewport) = largest();
        let frame = ValidatedFrame::new(content, &viewport)?;
        assert!(frame.byte_bound() > MAX_FRAME_BYTES - MAX_FRAME_BYTES / 10);
        // What the daemon sends: the compact frame, base64, in one JSON reply.
        let bytes = postcard::to_allocvec(frame.content())?;
        assert!(bytes.len() <= MAX_FRAME_BYTES);
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let response = ControlResponse {
            version: PROTOCOL_VERSION,
            request_id: u64::MAX,
            body: ControlResponseBody::Ok {
                response: ControlResponseData::Plugin {
                    reply: Reply::Frame {
                        handle: "h".repeat(64),
                        content: encoded.clone(),
                        next_tick_ms: u16::MAX,
                    },
                },
            },
        };
        let wire = serde_json::to_vec(&response)?;
        assert!(
            wire.len() <= crate::control::MAX_RESPONSE_BYTES,
            "{} bytes on the wire",
            wire.len()
        );
        // And what the frontend accepts on the other side.
        let decoded = decode_frame(&encoded, &viewport)?;
        assert_eq!(decoded.content().tiles.len(), MAX_TILES);
        Ok(())
    }
}
