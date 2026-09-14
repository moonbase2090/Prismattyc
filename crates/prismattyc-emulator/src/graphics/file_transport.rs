//! Safe, bounded read of a child-supplied file (Kitty `t=f`). Opens the path,
//! then validates the OPEN fd (no re-lookup) to defeat TOCTOU.

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileError {
    Open,
    NotRegular,
    WrongOwner,
    TooLarge,
    Io,
}

#[cfg(unix)]
pub(crate) fn read_file_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, FileError> {
    use std::os::unix::fs::MetadataExt;

    let mut file = File::open(path).map_err(|_| FileError::Open)?;
    // fstat the open handle — not the path.
    let meta = file.metadata().map_err(|_| FileError::Io)?;
    if !meta.file_type().is_file() {
        return Err(FileError::NotRegular);
    }
    // Owner must be us.
    let our_uid = own_uid();
    if meta.uid() != our_uid {
        return Err(FileError::WrongOwner);
    }
    if meta.len() > max_bytes {
        return Err(FileError::TooLarge);
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    // Cap the read too, in case the file grew between fstat and read.
    file.by_ref()
        .take(max_bytes)
        .read_to_end(&mut buf)
        .map_err(|_| FileError::Io)?;
    if buf.len() as u64 > max_bytes {
        return Err(FileError::TooLarge);
    }
    Ok(buf)
}

#[cfg(unix)]
fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub(crate) fn read_file_bounded(_path: &Path, _max_bytes: u64) -> Result<Vec<u8>, FileError> {
    Err(FileError::Open) // t=f unsupported off-unix in the first cut
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write;

    #[cfg(unix)]
    #[test]
    fn reads_regular_file_within_cap() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.bin");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        assert_eq!(read_file_bounded(&p, 1024).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_oversize() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.bin");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(&[0u8; 4096])
            .unwrap();
        assert!(matches!(
            read_file_bounded(&p, 16),
            Err(FileError::TooLarge)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_directory() {
        let dir = std::env::temp_dir().join(format!("prism-gfx-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            read_file_bounded(&dir, 1024),
            Err(FileError::NotRegular) | Err(FileError::Open)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
