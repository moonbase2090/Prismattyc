// SPDX-License-Identifier: MIT
//! Linux-only minor-0 interoperability using the existing public Lunatui codec.
use crate::{control::Control, ring::Producer, session::record};
use anyhow::{ensure, Result};
use lunatui_core::{ClusterTable, Color, Grid};
use lunatui_prismattyc::{discover_from_input, FrameEncoder, NativeDiscovery, NativePolicy};
use std::{
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

pub struct LegacySession {
    control: Control,
    producer: Producer,
    encoder: FrameEncoder,
    session: u64,
    pub epoch: u64,
    request: u64,
    frame: u64,
    columns: u16,
    rows: u16,
}

impl LegacySession {
    pub fn activate(
        stream: UnixStream,
        capability_id: u32,
        reply: &[u8],
        grid: &Grid,
        clusters: &ClusterTable,
        palette: Vec<Color>,
    ) -> Result<Self> {
        let decoded = discover_from_input(NativePolicy::DirectLocal, capability_id, reply);
        ensure!(
            !decoded.alpha_offered && decoded.preserved_input.is_empty(),
            "legacy discovery features"
        );
        let NativeDiscovery::Advertised {
            id,
            offer,
            generation: epoch,
        } = decoded.discovery
        else {
            anyhow::bail!("legacy grid was not offered");
        };
        let mut encoder = FrameEncoder::new();
        encoder.offer_alpha(decoded.alpha_offered);
        encoder.set_token_palette(palette);
        ensure!(encoder.hello_ring_minor() == 0, "legacy minor negotiation");
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut control = Control::legacy(stream)?;
        let mut hello = record(1, 56, 0, epoch, 1);
        hello[32..36].copy_from_slice(&id.to_le_bytes());
        hello[36..38].copy_from_slice(&1u16.to_le_bytes());
        hello[38..40].copy_from_slice(&encoder.hello_ring_minor().to_le_bytes());
        hello[40..56].copy_from_slice(&offer);
        let setup = control.exchange(hello, None, deadline)?;
        ensure!(crate::u16_at(&setup.bytes, 4) == 2, "legacy setup ended");
        let session = crate::u64_at(&setup.bytes, 8);
        let mut producer = Producer::from_legacy_setup(
            setup
                .descriptor
                .ok_or_else(|| crate::invalid("missing legacy ring descriptor"))?,
            &setup.bytes,
        )?;
        producer.publish(encoder.encode(grid, clusters, epoch, 1, None)?)?;
        let mut activate = record(3, 40, session, epoch, 2);
        activate[32..40].copy_from_slice(&1u64.to_le_bytes());
        let active = control.exchange(activate, None, deadline)?;
        ensure!(
            crate::u16_at(&active.bytes, 4) == 4 && crate::u64_at(&active.bytes, 32) == 1,
            "legacy activation receipt"
        );
        ensure!(
            encoder.affirm_alpha(crate::u16_at(&active.bytes, 6))
                == lunatui_prismattyc::AttributeMode::Stripped,
            "legacy attributes must remain stripped"
        );
        Ok(Self {
            control,
            producer,
            encoder,
            session,
            epoch,
            request: 2,
            frame: 1,
            columns: grid.width(),
            rows: grid.height(),
        })
    }

    pub fn present(&mut self, grid: &Grid, clusters: &ClusterTable) -> Result<u64> {
        ensure!(
            (grid.width(), grid.height()) == (self.columns, self.rows),
            "legacy geometry changed"
        );
        let next = self
            .frame
            .checked_add(1)
            .ok_or_else(|| crate::invalid("legacy frame sequence exhausted"))?;
        self.producer.publish(
            self.encoder
                .encode(grid, clusters, self.epoch, next, None)?,
        )?;
        self.frame = next;
        Ok(next)
    }

    fn exchange(&mut self, opcode: u16, expected: u16) -> Result<()> {
        self.request = self
            .request
            .checked_add(1)
            .ok_or_else(|| crate::invalid("legacy request sequence exhausted"))?;
        let reply = self.control.exchange(
            record(opcode, 32, self.session, self.epoch, self.request),
            None,
            Instant::now() + Duration::from_millis(100),
        )?;
        ensure!(
            crate::u16_at(&reply.bytes, 4) == expected,
            "legacy session ended"
        );
        Ok(())
    }

    pub fn ping(&mut self) -> Result<()> {
        self.exchange(7, 8)
    }
    pub fn release(&mut self) -> Result<()> {
        self.exchange(5, 6)
    }
}
