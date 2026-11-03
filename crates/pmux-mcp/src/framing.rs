// SPDX-License-Identifier: MIT
//! Bound newline-delimited input before the MCP library allocates a whole frame.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

pub const INPUT_BYTES: usize = 4 * 1024 * 1024;
pub const OUTPUT_BYTES: usize = 16 * 1024 * 1024;

pub struct BoundedLines<R> {
    inner: R,
    bytes: usize,
    limit: usize,
    failed: bool,
}

impl<R> BoundedLines<R> {
    pub fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            bytes: 0,
            limit,
            failed: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLines<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP input was rejected",
            )));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                for byte in &buffer.filled()[before..] {
                    this.bytes += 1;
                    if this.bytes > this.limit {
                        this.failed = true;
                        buffer.set_filled(before);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "MCP line exceeds its byte limit",
                        )));
                    }
                    if *byte == b'\n' {
                        this.bytes = 0;
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    #[tokio::test]
    async fn complete_frames_reset_the_limit_but_an_unterminated_frame_cannot_grow() {
        let input = &b"1234567\nabcdefg\n"[..];
        let mut reader = BufReader::new(BoundedLines::new(input, 8)).lines();
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("1234567")
        );
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("abcdefg")
        );
        let mut reader = BoundedLines::new(&b"123456789"[..], 8);
        let mut bytes = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut bytes).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(bytes.len() <= 8);
    }
}
