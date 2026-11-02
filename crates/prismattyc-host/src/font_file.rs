//! Read-only mapped font data shared by the font loaders.

use std::fs::File;
use std::io;
use std::path::Path;

use memmap2::Mmap;

pub(crate) fn map(path: &Path) -> io::Result<Mmap> {
    let file = File::open(path)?;
    // SAFETY: callers retain the mapping for every borrow. Font directories
    // are treated as immutable while a font is loaded and in use; replacing a
    // font should publish a new file instead of truncating one in place.
    unsafe { memmap2::MmapOptions::new().map(&file) }
}
