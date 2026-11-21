//! Read-only mapped font data shared by the font loaders.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

pub(crate) fn map(path: &Path) -> io::Result<Mmap> {
    let file = File::open(path)?;
    // SAFETY: callers retain the mapping for every borrow. Font directories
    // are treated as immutable while a font is loaded and in use; replacing a
    // font should publish a new file instead of truncating one in place.
    unsafe { memmap2::MmapOptions::new().map(&file) }
}

#[derive(Clone)]
pub(crate) enum Bytes {
    Mapped(Arc<memmap2::Mmap>),
    Baked(&'static [u8]),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mapped(data) => data,
            Self::Baked(data) => data,
        }
    }
}
