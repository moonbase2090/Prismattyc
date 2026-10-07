// SPDX-License-Identifier: MPL-2.0
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PresentTiming {
    pub(crate) write_us: u64,
    pub(crate) commit_us: u64,
    pub(crate) dirty_tiles: usize,
    pub(crate) changed_tiles: Option<usize>,
    pub(crate) write_bytes: usize,
}
