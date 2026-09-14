pub(crate) mod base64;
pub(crate) mod command;
pub(crate) mod decode;
pub(crate) mod file_transport;
pub mod placeholder;

use std::sync::Arc;

use prismattyc_core::Screen;
use prismattyc_protocol::GraphicsApc;
use serde::{Deserialize, Serialize};

use command::{Action, Transport};
use decode::{decode_png_bounded, MaxDims};

const MAX_W: u32 = 2048;
const MAX_H: u32 = 1024;
const MAX_FRAME_BYTES: usize = 8 << 20; // 8 MiB
const MAX_RETAINED_BYTES: usize = 16 << 20; // 16 MiB per generation
const PNG_FORMAT: u16 = 100;
/// Nominal cell size (px) used when no viewer font is available (mux-server
/// spawn, XTWINOPS before `set_cell_pixels`, Kitty grid footprint when the
/// child omits `c=`/`r=`). The windowed host overwrites these from FontMetrics.
pub const NOMINAL_CELL_W_PX: u32 = 10;
pub const NOMINAL_CELL_H_PX: u32 = 20;

fn trace_ignored(control: &str, reason: &str) {
    if std::env::var("PRISMATTYC_GRAPHICS_TRACE").ok().as_deref() == Some("1") {
        eprintln!("prismattyc graphics ignored: control={control:?} reason={reason}");
    }
}

/// A decoded image placed at a cell anchor (direct `a=T`, not unicode).
#[derive(Debug, Clone)]
pub struct PlacedImage {
    pub id: u32,
    pub image_number: Option<u32>,
    pub placement_id: u32,
    pub z: i32,
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub cols: u16,
    pub rows: u16,
    pub anchor_abs_line: u64,
    pub anchor_col: u16,
}

/// Raster keyed by Kitty image id. Shared by cursor and unicode placements.
#[derive(Debug, Clone)]
pub struct StoredImage {
    pub id: u32,
    pub image_number: Option<u32>,
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// Virtual placement (`U=1` / `a=p`) for `U+10EEEE` cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub cols: u16,
    pub rows: u16,
}

/// Versioned DTO for retained Kitty graphics state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsStateV1 {
    pub images: Vec<PlacedImageStateV1>,
    pub registry: Vec<StoredImageStateV1>,
    pub virtuals: Vec<VirtualPlacementStateV1>,
    pub retained_bytes: u64,
    pub next_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPlacementStateV1 {
    pub image_id: u32,
    pub placement_id: u32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedImageStateV1 {
    pub id: u32,
    pub image_number: Option<u32>,
    pub placement_id: u32,
    pub z: i32,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub cols: u16,
    pub rows: u16,
    pub anchor_abs_line: u64,
    pub anchor_col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredImageStateV1 {
    pub id: u32,
    pub image_number: Option<u32>,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default)]
pub(crate) struct GraphicsState {
    images: Vec<PlacedImage>,
    registry: Vec<StoredImage>,
    virtuals: Vec<VirtualPlacement>,
    retained_bytes: usize,
    next_id: u32,
    /// In-flight chunk reassembly (accumulated base64 + first-chunk cmd).
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    cmd: command::GraphicsCommand,
    base64: Vec<u8>,
}

fn screen_position(screen: &Screen, x: i32, y: i32) -> Option<(u64, u16)> {
    if x < 0 || y < 0 || x >= screen.columns() as i32 || y >= screen.rows() as i32 {
        return None;
    }
    Some((
        screen.scrolled_lines().saturating_add(y as u64),
        u16::try_from(x).ok()?,
    ))
}

fn row_contains(image: &PlacedImage, row: u64) -> bool {
    row >= image.anchor_abs_line
        && row
            < image
                .anchor_abs_line
                .saturating_add(u64::from(image.rows.max(1)))
}

fn column_contains(image: &PlacedImage, col: u16) -> bool {
    u32::from(col) >= u32::from(image.anchor_col)
        && u32::from(col) < u32::from(image.anchor_col).saturating_add(u32::from(image.cols.max(1)))
}

fn validate_raster(width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("graphics raster dimensions must be non-zero".into());
    }
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "graphics raster dimensions overflow".to_owned())?;
    if expected != rgba.len() || rgba.len() > MAX_FRAME_BYTES {
        return Err("graphics raster byte length is invalid".into());
    }
    Ok(())
}

impl GraphicsState {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    fn allocate_id(&mut self) -> u32 {
        let start = self.next_id.max(1);
        let mut candidate = start;
        loop {
            if !self.registry.iter().any(|image| image.id == candidate)
                && !self.images.iter().any(|image| image.id == candidate)
            {
                self.next_id = candidate.wrapping_add(1).max(1);
                return candidate;
            }
            candidate = candidate.wrapping_add(1).max(1);
            if candidate == start {
                return 1;
            }
        }
    }

    pub fn images(&self) -> &[PlacedImage] {
        &self.images
    }

    pub fn image_by_id(&self, id: u32) -> Option<&StoredImage> {
        self.registry.iter().find(|img| img.id == id)
    }

    /// Placement id 0: first virtual placement of `image_id`.
    pub fn virtual_placement(&self, image_id: u32, placement_id: u32) -> Option<&VirtualPlacement> {
        if placement_id == 0 {
            self.virtuals.iter().find(|v| v.image_id == image_id)
        } else {
            self.virtuals
                .iter()
                .find(|v| v.image_id == image_id && v.placement_id == placement_id)
        }
    }

    pub(crate) fn export_state(&self) -> Result<GraphicsStateV1, &'static str> {
        if self.pending.is_some() {
            return Err("graphics upload is not complete");
        }
        Ok(GraphicsStateV1 {
            images: self
                .images
                .iter()
                .map(|image| PlacedImageStateV1 {
                    id: image.id,
                    image_number: image.image_number,
                    placement_id: image.placement_id,
                    z: image.z,
                    rgba: image.rgba.to_vec(),
                    width: image.width,
                    height: image.height,
                    cols: image.cols,
                    rows: image.rows,
                    anchor_abs_line: image.anchor_abs_line,
                    anchor_col: image.anchor_col,
                })
                .collect(),
            registry: self
                .registry
                .iter()
                .map(|image| StoredImageStateV1 {
                    id: image.id,
                    image_number: image.image_number,
                    rgba: image.rgba.to_vec(),
                    width: image.width,
                    height: image.height,
                })
                .collect(),
            virtuals: self
                .virtuals
                .iter()
                .map(|placement| VirtualPlacementStateV1 {
                    image_id: placement.image_id,
                    placement_id: placement.placement_id,
                    cols: placement.cols,
                    rows: placement.rows,
                })
                .collect(),
            retained_bytes: self.retained_bytes as u64,
            next_id: self.next_id,
        })
    }

    pub(crate) fn import_state(state: GraphicsStateV1) -> Result<Self, String> {
        if state.next_id == 0 {
            return Err("graphics next id must be non-zero".into());
        }
        if state.registry.len() > MAX_RETAINED_BYTES / 4 {
            return Err("graphics registry is too large".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut retained_bytes = 0usize;
        let mut registry = Vec::with_capacity(state.registry.len());
        for image in state.registry {
            validate_raster(image.width, image.height, &image.rgba)?;
            if !ids.insert(image.id) {
                return Err("duplicate graphics image id".into());
            }
            retained_bytes = retained_bytes
                .checked_add(image.rgba.len())
                .ok_or_else(|| "graphics retained byte count overflow".to_owned())?;
            registry.push(StoredImage {
                id: image.id,
                image_number: image.image_number,
                rgba: Arc::from(image.rgba.into_boxed_slice()),
                width: image.width,
                height: image.height,
            });
        }
        if retained_bytes > MAX_RETAINED_BYTES || state.retained_bytes != retained_bytes as u64 {
            return Err("graphics retained byte count is invalid".into());
        }
        let known_ids: std::collections::BTreeSet<u32> =
            registry.iter().map(|image| image.id).collect();
        let mut images = Vec::with_capacity(state.images.len());
        for image in state.images {
            validate_raster(image.width, image.height, &image.rgba)?;
            if !known_ids.contains(&image.id) {
                return Err("placed graphics image is not retained".into());
            }
            images.push(PlacedImage {
                id: image.id,
                image_number: image.image_number,
                placement_id: image.placement_id,
                z: image.z,
                rgba: Arc::from(image.rgba.into_boxed_slice()),
                width: image.width,
                height: image.height,
                cols: image.cols,
                rows: image.rows,
                anchor_abs_line: image.anchor_abs_line,
                anchor_col: image.anchor_col,
            });
        }
        for placement in &state.virtuals {
            if !known_ids.contains(&placement.image_id) {
                return Err("virtual graphics placement is not retained".into());
            }
        }
        Ok(Self {
            images,
            registry,
            virtuals: state
                .virtuals
                .into_iter()
                .map(|placement| VirtualPlacement {
                    image_id: placement.image_id,
                    placement_id: placement.placement_id,
                    cols: placement.cols,
                    rows: placement.rows,
                })
                .collect(),
            retained_bytes,
            next_id: state.next_id,
            pending: None,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.images.clear();
        self.registry.clear();
        self.virtuals.clear();
        self.retained_bytes = 0;
        self.pending = None;
    }

    pub(crate) fn is_query(apc: &GraphicsApc) -> bool {
        command::parse(&apc.control).action == Action::Query
    }

    pub(crate) fn handle(
        &mut self,
        apc: &GraphicsApc,
        screen: &Screen,
        replies: &mut Vec<Vec<u8>>,
    ) {
        let mut cmd = command::parse(&apc.control);
        match cmd.action {
            Action::Query => {
                if cmd.quiet == 0 {
                    replies.push(format!("\x1b_Gi={};OK\x1b\\", cmd.id).into_bytes());
                }
            }
            Action::Transmit => {
                if cmd.continuation && self.pending.is_none() {
                    trace_ignored(&apc.control, "continuation without a pending upload");
                    return;
                }
                let auto_image_number = cmd.image_number.is_some() && !cmd.id_present;
                if auto_image_number && !cmd.continuation {
                    cmd.id = self.allocate_id();
                }
                if cmd.format != PNG_FORMAT && self.pending.is_none() {
                    // First chunk of a non-PNG image: reject.
                    if !cmd.more {
                        trace_ignored(&apc.control, "unsupported image format");
                        return;
                    }
                }
                let completed = if cmd.more || self.pending.is_some() {
                    self.accumulate(&cmd, &apc.control, &apc.payload, screen)
                } else {
                    let stored = match cmd.transport {
                        Transport::Direct => self.transmit_inline(&cmd, &apc.payload, screen),
                        Transport::File => self.transmit_file(&cmd, &apc.payload, screen),
                        Transport::Other => {
                            trace_ignored(&apc.control, "unsupported transport");
                            false
                        }
                    };
                    Some((cmd, stored))
                };
                if let Some((completed_cmd, stored)) = completed {
                    if stored && completed_cmd.quiet == 0 && !completed_cmd.id_present {
                        if let Some(number) = completed_cmd.image_number {
                            replies.push(
                                format!("\x1b_Gi={},I={};OK\x1b\\", completed_cmd.id, number)
                                    .into_bytes(),
                            );
                        }
                    }
                }
            }
            Action::Put => {
                if cmd.unicode_placement {
                    self.upsert_virtual(&cmd);
                } else {
                    trace_ignored(&apc.control, "put without a unicode placement");
                }
            }
            Action::Delete => self.delete(&cmd, screen, &apc.control),
            Action::Other => trace_ignored(&apc.control, "unsupported action"),
        }
    }

    fn accumulate(
        &mut self,
        cmd: &command::GraphicsCommand,
        control: &str,
        payload_b64: &[u8],
        screen: &Screen,
    ) -> Option<(command::GraphicsCommand, bool)> {
        // A fresh transmit with a different image id means the previous
        // reassembly never finalized — drop it so we never merge two images.
        // (A real continuation chunk carries id==0 or the same id.)
        if let Some(p) = self.pending.as_ref() {
            if cmd.id != 0 && cmd.id != p.cmd.id {
                self.pending = None;
            }
        }
        // Start a new reassembly on a first chunk (carries the real control).
        if self.pending.is_none() {
            if cmd.transport != Transport::Direct || cmd.format != PNG_FORMAT {
                trace_ignored(control, "chunked upload is not inline PNG");
                return Some((*cmd, false));
            }
            self.pending = Some(Pending {
                cmd: *cmd,
                base64: Vec::new(),
            });
        }
        let over = {
            let p = self.pending.as_mut().expect("pending set above");
            if p.base64.len() + payload_b64.len() > prismattyc_protocol::MAX_GRAPHICS_APC_BYTES {
                true
            } else {
                p.base64.extend_from_slice(payload_b64);
                false
            }
        };
        if over {
            let dropped = self.pending.take().expect("pending");
            trace_ignored(control, "upload exceeds the APC byte limit");
            return Some((dropped.cmd, false));
        }
        if cmd.more {
            return None;
        }
        let Pending { cmd, base64 } = self.pending.take().expect("pending");
        Some((cmd, self.transmit_inline(&cmd, &base64, screen)))
    }

    fn transmit_inline(
        &mut self,
        cmd: &command::GraphicsCommand,
        payload_b64: &[u8],
        screen: &Screen,
    ) -> bool {
        let Ok(raw) = base64::decode(payload_b64) else {
            return false;
        };
        let max = MaxDims {
            w: MAX_W,
            h: MAX_H,
            bytes: MAX_FRAME_BYTES,
        };
        let Ok(img) = decode_png_bounded(&raw, max) else {
            return false;
        };
        self.store(cmd, img, screen)
    }

    fn transmit_file(
        &mut self,
        cmd: &command::GraphicsCommand,
        payload_b64: &[u8],
        screen: &Screen,
    ) -> bool {
        let Ok(path_bytes) = base64::decode(payload_b64) else {
            return false;
        };
        let Ok(path_str) = String::from_utf8(path_bytes) else {
            return false;
        };
        let Ok(raw) = file_transport::read_file_bounded(
            std::path::Path::new(&path_str),
            MAX_FRAME_BYTES as u64,
        ) else {
            return false;
        };
        let max = MaxDims {
            w: MAX_W,
            h: MAX_H,
            bytes: MAX_FRAME_BYTES,
        };
        let Ok(img) = decode_png_bounded(&raw, max) else {
            return false;
        };
        self.store(cmd, img, screen)
    }

    fn store(
        &mut self,
        cmd: &command::GraphicsCommand,
        img: decode::DecodedImage,
        screen: &Screen,
    ) -> bool {
        let Some(raster) = self.upsert_raster(cmd, img) else {
            return false;
        };
        let screen_cols = screen.columns() as u16;
        let screen_rows = screen.rows() as u16;
        let cols = if cmd.cols > 0 {
            cmd.cols.min(screen_cols.max(1))
        } else {
            (raster.width.div_ceil(NOMINAL_CELL_W_PX) as u16).clamp(1, screen_cols.max(1))
        };
        let rows = if cmd.rows > 0 {
            cmd.rows.min(screen_rows.max(1))
        } else {
            (raster.height.div_ceil(NOMINAL_CELL_H_PX) as u16).clamp(1, screen_rows.max(1))
        };
        if cmd.unicode_placement {
            self.upsert_virtual_grid(cmd.id, cmd.placement_id, cols, rows);
            return true;
        }
        if !cmd.place {
            return true;
        }
        let cursor = screen.cursor();
        let anchor_abs_line = screen.scrolled_lines() + cursor.row as u64;
        if let Some(slot) = self
            .images
            .iter_mut()
            .find(|p| p.id == cmd.id && cmd.id != 0)
        {
            slot.rgba = Arc::clone(&raster.rgba);
            slot.width = raster.width;
            slot.height = raster.height;
            slot.image_number = cmd.image_number;
            slot.placement_id = cmd.placement_id;
            slot.z = cmd.z.unwrap_or(0);
            slot.cols = cols;
            slot.rows = rows;
            slot.anchor_abs_line = anchor_abs_line;
            slot.anchor_col = cursor.column as u16;
            return true;
        }
        self.images.push(PlacedImage {
            id: cmd.id,
            image_number: cmd.image_number,
            placement_id: cmd.placement_id,
            z: cmd.z.unwrap_or(0),
            rgba: Arc::clone(&raster.rgba),
            width: raster.width,
            height: raster.height,
            cols,
            rows,
            anchor_abs_line,
            anchor_col: cursor.column as u16,
        });
        true
    }

    fn upsert_raster(
        &mut self,
        cmd: &command::GraphicsCommand,
        img: decode::DecodedImage,
    ) -> Option<StoredImage> {
        let id = cmd.id;
        let frame = img.rgba.len();
        if frame > MAX_FRAME_BYTES {
            return None;
        }
        if let Some(slot) = self.registry.iter_mut().find(|p| p.id == id && id != 0) {
            self.retained_bytes = self.retained_bytes.saturating_sub(slot.rgba.len());
            slot.rgba = Arc::from(img.rgba.into_boxed_slice());
            slot.width = img.width;
            slot.height = img.height;
            slot.image_number = cmd.image_number;
            self.retained_bytes += frame;
            return Some(slot.clone());
        }
        while self.retained_bytes + frame > MAX_RETAINED_BYTES && !self.registry.is_empty() {
            let dropped = self.registry.remove(0);
            self.retained_bytes = self.retained_bytes.saturating_sub(dropped.rgba.len());
            self.images.retain(|p| p.id != dropped.id);
            self.virtuals.retain(|v| v.image_id != dropped.id);
        }
        if self.retained_bytes + frame > MAX_RETAINED_BYTES {
            return None;
        }
        let stored = StoredImage {
            id,
            image_number: cmd.image_number,
            rgba: Arc::from(img.rgba.into_boxed_slice()),
            width: img.width,
            height: img.height,
        };
        self.registry.push(stored.clone());
        self.retained_bytes += frame;
        Some(stored)
    }

    fn upsert_virtual(&mut self, cmd: &command::GraphicsCommand) {
        let cols = cmd.cols;
        let rows = cmd.rows;
        self.upsert_virtual_grid(cmd.id, cmd.placement_id, cols, rows);
    }

    fn upsert_virtual_grid(&mut self, image_id: u32, placement_id: u32, cols: u16, rows: u16) {
        if let Some(slot) = self
            .virtuals
            .iter_mut()
            .find(|v| v.image_id == image_id && v.placement_id == placement_id)
        {
            slot.cols = cols;
            slot.rows = rows;
            return;
        }
        self.virtuals.push(VirtualPlacement {
            image_id,
            placement_id,
            cols,
            rows,
        });
    }

    fn delete(&mut self, cmd: &command::GraphicsCommand, screen: &Screen, control: &str) {
        // Kitty deletes also cancel an in-flight upload. This prevents a stale
        // continuation from recreating an image after the delete.
        self.pending = None;
        let Some(mode) = cmd.delete_mode else {
            trace_ignored(control, "delete without a supported delete mode");
            return;
        };
        let placement = cmd.placement_id_present.then_some(cmd.placement_id);
        match mode.kind {
            command::DeleteKind::All => {
                self.images.clear();
                if mode.free {
                    self.free_unreferenced();
                }
            }
            command::DeleteKind::ImageId => {
                if cmd.id != 0 {
                    self.delete_id_placements(cmd.id, placement, mode.free);
                }
            }
            command::DeleteKind::ImageNumber => {
                let Some(number) = cmd.image_number else {
                    return;
                };
                let Some(id) = self
                    .registry
                    .iter()
                    .rev()
                    .find(|image| image.image_number == Some(number))
                    .map(|image| image.id)
                else {
                    return;
                };
                self.delete_id_placements(id, placement, mode.free);
            }
            command::DeleteKind::Cursor => {
                let cursor = screen.cursor();
                let row = screen.scrolled_lines().saturating_add(cursor.row as u64);
                let col = cursor.column as u16;
                self.delete_at(row, col, None, mode.free);
            }
            command::DeleteKind::Position => {
                let (Some(x), Some(y)) = (cmd.x, cmd.y) else {
                    return;
                };
                if let Some((row, col)) = screen_position(screen, x, y) {
                    self.delete_at(row, col, None, mode.free);
                }
            }
            command::DeleteKind::PositionZ => {
                let (Some(x), Some(y), Some(z)) = (cmd.x, cmd.y, cmd.z) else {
                    return;
                };
                if let Some((row, col)) = screen_position(screen, x, y) {
                    self.delete_at(row, col, Some(z), mode.free);
                }
            }
            command::DeleteKind::Column => {
                let Some(x) = cmd.x else {
                    return;
                };
                if let Ok(col) = u16::try_from(x) {
                    if col < screen.columns() as u16 {
                        self.images.retain(|image| !column_contains(image, col));
                        if mode.free {
                            self.free_unreferenced();
                        }
                    }
                }
            }
            command::DeleteKind::Row => {
                let Some(y) = cmd.y else {
                    return;
                };
                if let Some((row, _)) = screen_position(screen, 0, y) {
                    self.images.retain(|image| !row_contains(image, row));
                    if mode.free {
                        self.free_unreferenced();
                    }
                }
            }
            command::DeleteKind::Z => {
                let Some(z) = cmd.z else {
                    return;
                };
                self.images.retain(|image| image.z != z);
                if mode.free {
                    self.free_unreferenced();
                }
            }
            command::DeleteKind::Range => {
                let (Some(low), Some(high)) = (cmd.x, cmd.y) else {
                    return;
                };
                if low > high {
                    return;
                }
                self.images.retain(|image| {
                    !(i64::from(image.id) >= i64::from(low)
                        && i64::from(image.id) <= i64::from(high))
                });
                self.virtuals.retain(|placement| {
                    !(i64::from(placement.image_id) >= i64::from(low)
                        && i64::from(placement.image_id) <= i64::from(high))
                });
                if mode.free {
                    self.free_unreferenced();
                }
            }
            command::DeleteKind::Frame => {}
        }
    }

    fn delete_at(&mut self, row: u64, col: u16, z: Option<i32>, free: bool) {
        self.images.retain(|image| {
            !(row_contains(image, row)
                && column_contains(image, col)
                && z.is_none_or(|expected| image.z == expected))
        });
        if free {
            self.free_unreferenced();
        }
    }

    fn delete_id_placements(&mut self, id: u32, placement: Option<u32>, free: bool) {
        self.images
            .retain(|image| image.id != id || placement.is_some_and(|p| image.placement_id != p));
        self.virtuals.retain(|item| {
            item.image_id != id || placement.is_some_and(|p| item.placement_id != p)
        });
        if free {
            self.free_unreferenced();
        }
    }

    fn free_unreferenced(&mut self) {
        let mut retained = 0usize;
        self.registry.retain(|image| {
            let referenced = self.images.iter().any(|placed| placed.id == image.id)
                || self
                    .virtuals
                    .iter()
                    .any(|placed| placed.image_id == image.id);
            if referenced {
                retained += image.rgba.len();
            }
            referenced
        });
        self.retained_bytes = retained;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prismattyc_core::Screen;
    use prismattyc_protocol::GraphicsApc;

    const RED_1X1_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";

    fn apc(control: &str, payload_b64: &str) -> GraphicsApc {
        GraphicsApc {
            control: control.into(),
            payload: payload_b64.as_bytes().to_vec(),
        }
    }

    #[test]
    fn query_queues_ok_reply() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("i=31,a=q,t=d,f=24", "AAAA"), &screen, &mut replies);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0], b"\x1b_Gi=31;OK\x1b\\");
        assert!(g.images().is_empty());
    }

    #[test]
    fn query_with_quiet_suppresses_reply() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("i=9,a=q,q=1", "AAAA"), &screen, &mut replies);
        assert!(replies.is_empty());
    }

    #[test]
    fn image_number_only_transmit_allocates_id_and_replies() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(
            &apc("a=t,t=d,f=100,I=73", RED_1X1_PNG_B64),
            &screen,
            &mut replies,
        );
        assert_eq!(replies, vec![b"\x1b_Gi=1,I=73;OK\x1b\\".to_vec()]);
        assert_eq!(
            g.image_by_id(1).and_then(|image| image.image_number),
            Some(73)
        );
        assert!(g.images().is_empty(), "a=t must not place at the cursor");
    }

    #[test]
    fn image_number_only_quiet_two_suppresses_reply() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(
            &apc("a=t,t=d,f=100,I=75,q=2", RED_1X1_PNG_B64),
            &screen,
            &mut replies,
        );
        assert!(replies.is_empty());
        assert!(
            g.image_by_id(1).is_some(),
            "q=2 stores the image but suppresses OK"
        );
    }

    #[test]
    fn image_number_only_chunked_transmit_replies_after_completion() {
        let (first, final_chunk) = RED_1X1_PNG_B64.split_at(40);
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("a=t,t=d,f=100,I=74,m=1", first), &screen, &mut replies);
        assert!(replies.is_empty());
        g.handle(&apc("m=0", final_chunk), &screen, &mut replies);
        assert_eq!(replies, vec![b"\x1b_Gi=1,I=74;OK\x1b\\".to_vec()]);
        assert_eq!(
            g.image_by_id(1).and_then(|image| image.image_number),
            Some(74)
        );
    }

    #[test]
    fn transmit_display_stores_decoded_image() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(
            &apc("a=T,t=d,f=100,i=5", RED_1X1_PNG_B64),
            &screen,
            &mut replies,
        );
        assert_eq!(g.images().len(), 1);
        let img = &g.images()[0];
        assert_eq!(img.id, 5);
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
    }

    #[test]
    fn bad_payload_is_dropped_not_panicked() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(&apc("a=T,t=d,f=100,i=1", "@@@@"), &screen, &mut replies);
        assert!(g.images().is_empty());
    }

    #[test]
    fn reassembles_chunked_inline_png() {
        let (a, b) = RED_1X1_PNG_B64.split_at(40);
        let mut g = GraphicsState::new();
        let screen = prismattyc_core::Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        // First chunk: full control, m=1.
        g.handle(
            &prismattyc_protocol::GraphicsApc {
                control: "a=T,t=d,f=100,i=8,m=1".into(),
                payload: a.as_bytes().to_vec(),
            },
            &screen,
            &mut replies,
        );
        assert!(g.images().is_empty(), "no image until final chunk");
        // Final chunk: continuation, m=0.
        g.handle(
            &prismattyc_protocol::GraphicsApc {
                control: "m=0".into(),
                payload: b.as_bytes().to_vec(),
            },
            &screen,
            &mut replies,
        );
        assert_eq!(g.images().len(), 1);
        assert_eq!(g.images()[0].id, 8);
    }

    #[cfg(unix)]
    #[test]
    fn transmit_file_transport_reads_and_decodes() {
        use std::io::Write;
        // Reuse the known-good 1x1 red PNG (already validated by
        // transmit_display_stores_decoded_image) rather than a fresh byte
        // literal, so this test isn't tripped up by a transcription error.
        let red_1x1_png = base64::decode(RED_1X1_PNG_B64.as_bytes()).unwrap();
        let dir = std::env::temp_dir().join(format!("prism-gfx-tf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("logo.png");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(&red_1x1_png)
            .unwrap();
        // Payload for t=f is base64 of the path string.
        let path_b64 = base64::base64_encode_for_test(p.to_str().unwrap().as_bytes());

        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        let apc = GraphicsApc {
            control: "a=T,t=f,f=100,i=3".into(),
            payload: path_b64.into_bytes(),
        };
        g.handle(&apc, &screen, &mut replies);
        assert_eq!(g.images().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_by_id_removes_only_that_image() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        for id in [1u32, 2] {
            g.handle(
                &GraphicsApc {
                    control: format!("a=T,t=d,f=100,i={id}"),
                    payload: RED_1X1_PNG_B64.as_bytes().to_vec(),
                },
                &screen,
                &mut r,
            );
        }
        assert_eq!(g.images().len(), 2);
        g.handle(
            &GraphicsApc {
                control: "a=d,d=i,i=1".into(),
                payload: vec![],
            },
            &screen,
            &mut r,
        );
        assert_eq!(g.images().len(), 1);
        assert_eq!(g.images()[0].id, 2);
    }

    fn seed_delete_state() -> (GraphicsState, Screen, Vec<Vec<u8>>) {
        let mut g = GraphicsState::new();
        let mut screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        for (id, extra) in [
            (1, "c=2,r=2,I=11,p=1,z=0"),
            (2, "c=2,r=1,I=22,p=2,z=-1"),
            (3, "c=1,r=1,I=11,p=3,z=2"),
        ] {
            g.handle(
                &apc(&format!("a=T,t=d,f=100,i={id},{extra}"), RED_1X1_PNG_B64),
                &screen,
                &mut replies,
            );
            if id == 1 {
                for _ in 0..3 {
                    screen.put_char('x');
                }
            } else if id == 2 {
                screen.carriage_return();
                screen.line_feed();
            }
        }
        g.handle(&apc("a=p,U=1,i=2,p=7,c=1,r=1", ""), &screen, &mut replies);
        (g, screen, replies)
    }

    fn direct_ids(g: &GraphicsState) -> Vec<u32> {
        let mut ids = g.images.iter().map(|image| image.id).collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn registry_ids(g: &GraphicsState) -> Vec<u32> {
        let mut ids = g.registry.iter().map(|image| image.id).collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    fn virtual_ids(g: &GraphicsState) -> Vec<u32> {
        let mut ids = g
            .virtuals
            .iter()
            .map(|image| image.image_id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn delete_matrix_removes_expected_placements_per_variant() {
        let cases = [
            ("a=d,d=a", vec![], vec![1, 2, 3], vec![2]),
            ("a=d,d=A", vec![], vec![2], vec![2]),
            ("a=d,d=i,i=1", vec![2, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=I,i=1", vec![2, 3], vec![2, 3], vec![2]),
            ("a=d,d=n,I=11", vec![1, 2], vec![1, 2, 3], vec![2]),
            ("a=d,d=N,I=11,p=3", vec![1, 2], vec![1, 2], vec![2]),
            ("a=d,d=c", vec![2], vec![1, 2, 3], vec![2]),
            ("a=d,d=C", vec![2], vec![2], vec![2]),
            ("a=d,d=p,x=3,y=0", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=P,x=3,y=0", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=q,x=3,y=0,z=-1", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=Q,x=3,y=0,z=-1", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=x,x=3", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=X,x=3", vec![1, 3], vec![1, 2, 3], vec![2]),
            ("a=d,d=y,y=1", vec![2], vec![1, 2, 3], vec![2]),
            ("a=d,d=Y,y=1", vec![2], vec![2], vec![2]),
            ("a=d,d=z,z=2", vec![1, 2], vec![1, 2, 3], vec![2]),
            ("a=d,d=Z,z=2", vec![1, 2], vec![1, 2], vec![2]),
            ("a=d,d=r,x=1,y=2", vec![3], vec![1, 2, 3], vec![]),
            ("a=d,d=R,x=1,y=2", vec![3], vec![3], vec![]),
            ("a=d,d=f", vec![1, 2, 3], vec![1, 2, 3], vec![2]),
        ];
        for (control, expected_direct, expected_registry, expected_virtual) in cases {
            let (mut g, screen, mut replies) = seed_delete_state();
            g.handle(&apc(control, ""), &screen, &mut replies);
            assert_eq!(direct_ids(&g), expected_direct, "direct: {control}");
            assert_eq!(registry_ids(&g), expected_registry, "registry: {control}");
            assert_eq!(virtual_ids(&g), expected_virtual, "virtual: {control}");
        }
    }

    #[test]
    fn delete_matrix_trace_ignores_unrelated_controls() {
        let (mut g, screen, mut replies) = seed_delete_state();
        // The trace retains these controls to make the ignored-control contract
        // visible when a failure prints the complete APC control string.
        let control = "a=d,d=x,x=3,i=1,I=11,p=7,y=1,z=2";
        g.handle(&apc(control, ""), &screen, &mut replies);
        assert_eq!(direct_ids(&g), vec![1, 3], "ignored controls: {control}");
        assert_eq!(
            registry_ids(&g),
            vec![1, 2, 3],
            "ignored controls: {control}"
        );
        assert_eq!(virtual_ids(&g), vec![2], "ignored controls: {control}");
    }

    #[test]
    fn delete_cancels_pending_upload_and_ignores_continuation() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut replies = Vec::new();
        g.handle(
            &apc("a=T,t=d,f=100,i=9,m=1", "iVBORw0K"),
            &screen,
            &mut replies,
        );
        assert!(g.pending.is_some());
        g.handle(&apc("a=d,d=i,i=9", ""), &screen, &mut replies);
        assert!(g.pending.is_none());
        g.handle(&apc("m=0", RED_1X1_PNG_B64), &screen, &mut replies);
        assert!(g.images.is_empty());
        assert!(g.registry.is_empty());
    }

    #[test]
    fn retained_bytes_never_exceed_cap() {
        // Store many distinct-id images; retained_bytes must never exceed the cap.
        let mut g = GraphicsState::new();
        let screen = prismattyc_core::Screen::new(80, 24, 0);
        let mut r = Vec::new();
        for id in 0..50u32 {
            g.handle(
                &prismattyc_protocol::GraphicsApc {
                    control: format!("a=T,t=d,f=100,i={}", id + 1),
                    payload: RED_1X1_PNG_B64.as_bytes().to_vec(),
                },
                &screen,
                &mut r,
            );
        }
        assert!(g.retained_bytes <= MAX_RETAINED_BYTES);
    }

    #[test]
    fn explicit_span_clamped_to_screen() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(
            &GraphicsApc {
                control: "a=T,t=d,f=100,c=65535,r=65535,i=1".into(),
                payload: RED_1X1_PNG_B64.as_bytes().to_vec(),
            },
            &screen,
            &mut r,
        );
        assert_eq!(g.images().len(), 1);
        assert!(g.images()[0].cols <= 80, "cols must clamp to screen width");
        assert!(g.images()[0].rows <= 24, "rows must clamp to screen height");
    }

    #[test]
    fn stale_reassembly_does_not_corrupt_new_image() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        // Start a chunked transfer for id=1 that never finalizes (m=1).
        g.handle(
            &GraphicsApc {
                control: "a=T,t=d,f=100,i=1,m=1".into(),
                payload: b"iVBORw0K".to_vec(),
            },
            &screen,
            &mut r,
        );
        assert!(g.images().is_empty(), "partial chunk stores nothing yet");
        // A fresh standalone image id=2 arrives before id=1 finished.
        g.handle(
            &GraphicsApc {
                control: "a=T,t=d,f=100,i=2".into(),
                payload: RED_1X1_PNG_B64.as_bytes().to_vec(),
            },
            &screen,
            &mut r,
        );
        assert_eq!(
            g.images().len(),
            1,
            "id=2 must decode cleanly, not merge with id=1"
        );
        assert_eq!(g.images()[0].id, 2);
        assert_eq!(g.images()[0].width, 1);
        assert_eq!(g.images()[0].height, 1);
    }

    #[test]
    fn transmit_only_stores_registry_without_cursor_place() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(&apc("a=t,t=d,f=100,i=4", RED_1X1_PNG_B64), &screen, &mut r);
        assert!(g.images().is_empty());
        assert_eq!(g.image_by_id(4).map(|i| i.width), Some(1));
    }

    #[test]
    fn put_unicode_creates_virtual_placement() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(&apc("a=t,t=d,f=100,i=4", RED_1X1_PNG_B64), &screen, &mut r);
        g.handle(&apc("a=p,U=1,i=4,c=2,r=1", ""), &screen, &mut r);
        let v = g.virtual_placement(4, 0).expect("virtual");
        assert_eq!((v.cols, v.rows), (2, 1));
        assert!(g.images().is_empty());
    }

    #[test]
    fn transmit_unicode_does_not_cursor_place() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(
            &apc("a=T,U=1,t=d,f=100,i=5,c=3,r=1", RED_1X1_PNG_B64),
            &screen,
            &mut r,
        );
        assert!(g.images().is_empty());
        assert!(g.virtual_placement(5, 0).is_some());
    }

    #[test]
    fn delete_all_keeps_registry_and_virtuals() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(&apc("a=T,t=d,f=100,i=1", RED_1X1_PNG_B64), &screen, &mut r);
        g.handle(
            &apc("a=T,U=1,t=d,f=100,i=2,c=1,r=1", RED_1X1_PNG_B64),
            &screen,
            &mut r,
        );
        g.handle(&apc("a=d,d=a", ""), &screen, &mut r);
        assert!(g.images().is_empty());
        assert!(g.image_by_id(2).is_some());
        assert!(g.virtual_placement(2, 0).is_some());
    }

    #[test]
    fn put_without_unicode_is_ignored() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(&apc("a=t,t=d,f=100,i=4", RED_1X1_PNG_B64), &screen, &mut r);
        g.handle(&apc("a=p,i=4,c=2,r=1", ""), &screen, &mut r);
        assert!(g.virtual_placement(4, 0).is_none());
        assert!(g.images().is_empty());
        assert!(g.image_by_id(4).is_some());
    }

    #[test]
    fn two_virtual_placements_of_one_image() {
        let mut g = GraphicsState::new();
        let screen = Screen::new(80, 24, 0);
        let mut r = Vec::new();
        g.handle(&apc("a=t,t=d,f=100,i=4", RED_1X1_PNG_B64), &screen, &mut r);
        g.handle(&apc("a=p,U=1,i=4,p=1,c=2,r=1", ""), &screen, &mut r);
        g.handle(&apc("a=p,U=1,i=4,p=2,c=4,r=2", ""), &screen, &mut r);
        let a = g.virtual_placement(4, 1).expect("p=1");
        let b = g.virtual_placement(4, 2).expect("p=2");
        assert_eq!((a.cols, a.rows), (2, 1));
        assert_eq!((b.cols, b.rows), (4, 2));
        let any = g.virtual_placement(4, 0).expect("p=0 picks first");
        assert_eq!(any.placement_id, 1);
    }
}
