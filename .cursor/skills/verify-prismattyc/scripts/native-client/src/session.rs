// SPDX-License-Identifier: MIT
use crate::{
    backing::{self, PreparedImage},
    control::{Control, Reply},
    frame::{self, Placement},
    images::Key,
    ring::Producer,
};
use anyhow::{ensure, Result};
use lunatui_core::{ClusterTable, Grid};
use lunatui_image::ImageSource;
use lunatui_prismattyc::FrameEncoder;
use std::{
    collections::BTreeMap,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

pub fn read_offer(
    descriptor: std::os::fd::BorrowedFd<'_>,
    id: u32,
    deadline: Instant,
) -> Result<Offer> {
    Offer::parse(&read_capability(descriptor, deadline)?, id)
}

pub fn read_capability(
    descriptor: std::os::fd::BorrowedFd<'_>,
    deadline: Instant,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut bytes = Vec::new();
    loop {
        let now = Instant::now();
        ensure!(now < deadline && bytes.len() < 4096, "discovery bounds");
        let mut poll = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe {
            libc::poll(
                &mut poll,
                1,
                deadline.saturating_duration_since(now).as_millis().max(1) as i32,
            )
        };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        ensure!(n > 0, "discovery input");
        let mut chunk = [0; 4096];
        let n = unsafe {
            libc::read(
                descriptor.as_raw_fd(),
                chunk.as_mut_ptr().cast(),
                chunk.len() - bytes.len(),
            )
        };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        ensure!(n > 0, "discovery read");
        bytes.extend_from_slice(&chunk[..n as usize]);
        if bytes.ends_with(b"\x1b\\") {
            return Ok(bytes);
        }
    }
}

pub struct Offer {
    pub id: u32,
    pub epoch: u64,
    pub value: [u8; 16],
}

impl Offer {
    pub fn parse(reply: &[u8], id: u32) -> Result<Self> {
        ensure!(reply.len() <= 4096 && id != 0, "discovery bounds");
        let body = std::str::from_utf8(reply)?
            .strip_prefix("\x1b_Prismattyc;cap;r;")
            .and_then(|body| body.strip_suffix("\x1b\\"))
            .ok_or_else(|| crate::invalid("discovery framing"))?;
        ensure!(
            body.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
            "discovery ASCII"
        );
        let mut fields = BTreeMap::new();
        for field in body.split(';') {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| crate::invalid("discovery field"))?;
            ensure!(
                fields.insert(key, value).is_none(),
                "duplicate discovery field"
            );
        }
        ensure!(fields.get("v") == Some(&"0.4"), "discovery version");
        ensure!(
            fields.get("id").and_then(|value| value.parse().ok()) == Some(id),
            "discovery correlation"
        );
        ensure!(
            fields.get("features").is_some_and(|features| features
                .split(',')
                .any(|feature| feature == "native.images.v1")),
            "images were not offered"
        );
        let epoch = fields
            .get("native.generation")
            .ok_or_else(|| crate::invalid("discovery epoch"))?
            .parse::<u64>()?;
        ensure!(epoch != 0, "discovery epoch zero");
        let text = fields
            .get("native.offer")
            .ok_or_else(|| crate::invalid("discovery offer"))?;
        ensure!(
            text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()),
            "discovery offer bytes"
        );
        let mut value = [0; 16];
        for (index, byte) in value.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)?;
        }
        Ok(Self { id, epoch, value })
    }
}

#[derive(Debug)]
pub struct SessionEnded {
    pub reason: u32,
}
impl std::fmt::Display for SessionEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native session ended")
    }
}
impl std::error::Error for SessionEnded {}

pub struct Session {
    control: Control,
    producer: Producer,
    encoder: FrameEncoder,
    session: u64,
    epoch: u64,
    metrics: u64,
    request_sequence: u64,
    frame_sequence: u64,
    columns: u16,
    rows: u16,
    pub cell: [u32; 2],
}

impl Session {
    pub fn activate(
        stream: UnixStream,
        offer: Offer,
        grid: &Grid,
        clusters: &ClusterTable,
        palette: Vec<lunatui_core::Color>,
    ) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut control = Control::new(stream)?;
        let mut hello = record(1, 56, 0, offer.epoch, 1);
        hello[32..36].copy_from_slice(&offer.id.to_le_bytes());
        hello[36..38].copy_from_slice(&1u16.to_le_bytes());
        hello[38..40].copy_from_slice(&2u16.to_le_bytes());
        hello[40..56].copy_from_slice(&offer.value);
        let setup = control.exchange(hello, None, deadline)?;
        let bytes = &setup.bytes;
        let session = crate::u64_at(bytes, 8);
        let metrics = crate::u64_at(bytes, 64);
        let cell = [crate::u32_at(bytes, 72), crate::u32_at(bytes, 76)];
        ensure!(
            metrics != 0 && cell.into_iter().all(|axis| (1..=4096).contains(&axis)),
            "setup metrics"
        );
        ensure!(
            crate::u32_at(bytes, 80) == u32::from(grid.width()) * cell[0]
                && crate::u32_at(bytes, 84) == u32::from(grid.height()) * cell[1],
            "setup geometry"
        );
        ensure!(
            crate::u64_at(bytes, 88) == 67108864
                && crate::u64_at(bytes, 96) == 134217728
                && crate::u32_at(bytes, 104) == 256
                && crate::u32_at(bytes, 108) == 256,
            "setup image budgets"
        );
        let mut producer = Producer::from_setup(
            setup
                .descriptor
                .ok_or_else(|| crate::invalid("missing ring descriptor"))?,
            bytes,
        )?;
        let mut encoder = FrameEncoder::new();
        encoder.set_token_palette(palette);
        let frame = frame::encode(&mut encoder, grid, clusters, offer.epoch, metrics, 1, &[])?;
        producer.publish(&frame)?;
        let mut activate = record(3, 40, session, offer.epoch, 2);
        activate[32..40].copy_from_slice(&1u64.to_le_bytes());
        let active = control.exchange(activate, None, deadline)?;
        ensure!(
            crate::u64_at(&active.bytes, 32) == 1,
            "activation frame receipt"
        );
        Ok(Self {
            control,
            producer,
            encoder,
            session,
            epoch: offer.epoch,
            metrics,
            request_sequence: 2,
            frame_sequence: 1,
            columns: grid.width(),
            rows: grid.height(),
            cell,
        })
    }

    fn request(&mut self, opcode: u16, bytes: usize) -> Result<Vec<u8>> {
        self.request_sequence = self
            .request_sequence
            .checked_add(1)
            .ok_or_else(|| crate::invalid("request sequence exhausted"))?;
        Ok(record(
            opcode,
            bytes,
            self.session,
            self.epoch,
            self.request_sequence,
        ))
    }

    pub fn ping(&mut self) -> Result<Reply> {
        let request = self.request(7, 32)?;
        let reply =
            self.control
                .exchange(request, None, Instant::now() + Duration::from_millis(100))?;
        if crate::u16_at(&reply.bytes, 4) == 14 {
            return Err(SessionEnded {
                reason: crate::u32_at(&reply.bytes, 40),
            }
            .into());
        }
        ensure!(crate::u16_at(&reply.bytes, 4) == 15, "native session ended");
        Ok(reply)
    }

    pub fn upload(
        &mut self,
        key: Key,
        source: &ImageSource,
        prepared: PreparedImage,
    ) -> Result<Reply> {
        let mut request = self.request(10, 112)?;
        for (offset, value) in [
            (32, key.0),
            (36, source.size().width()),
            (40, source.size().height()),
        ] {
            request[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        request[44..46].copy_from_slice(&1u16.to_le_bytes());
        request[46..48].copy_from_slice(&backing::KIND.to_le_bytes());
        for (offset, value) in [
            (48, key.1),
            (56, source.rgba8_pixels().len() as u64),
            (64, prepared.physical),
        ] {
            request[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        request[72..104].copy_from_slice(&prepared.digest);
        Ok(self.control.exchange(
            request,
            Some(prepared.descriptor),
            Instant::now() + Duration::from_millis(200),
        )?)
    }

    pub fn present(
        &mut self,
        grid: &Grid,
        clusters: &ClusterTable,
        placements: &[Placement],
    ) -> Result<u64> {
        ensure!(
            grid.width() == self.columns && grid.height() == self.rows,
            "frame geometry changed"
        );
        let next = self
            .frame_sequence
            .checked_add(1)
            .ok_or_else(|| crate::invalid("frame sequence exhausted"))?;
        let frame = frame::encode(
            &mut self.encoder,
            grid,
            clusters,
            self.epoch,
            self.metrics,
            next,
            placements,
        )?;
        self.producer.publish(&frame)?;
        self.frame_sequence = next;
        Ok(next)
    }

    pub fn retire(&mut self, handle: u64, omitting: u64) -> Result<Reply> {
        let mut request = self.request(12, 48)?;
        request[32..40].copy_from_slice(&handle.to_le_bytes());
        request[40..48].copy_from_slice(&omitting.to_le_bytes());
        Ok(self
            .control
            .exchange(request, None, Instant::now() + Duration::from_millis(200))?)
    }

    pub fn release(&mut self) -> Result<()> {
        let request = self.request(5, 32)?;
        self.control
            .exchange(request, None, Instant::now() + Duration::from_millis(200))?;
        Ok(())
    }
}

pub fn record(opcode: u16, bytes: usize, session: u64, epoch: u64, sequence: u64) -> Vec<u8> {
    let mut output = vec![0; bytes];
    output[..4].copy_from_slice(&(bytes as u32).to_le_bytes());
    output[4..6].copy_from_slice(&opcode.to_le_bytes());
    for (offset, value) in [(8, session), (16, sequence), (24, epoch)] {
        output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    output
}
