//! Owner-only files for local runtime and persisted state.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Create a private directory, or repair an existing owned directory.
/// An explicit path in a system-owned sticky temp directory keeps that shared
/// parent unchanged. Other final components must not be symlinks.
pub fn create_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0 {
                return Ok(());
            }
        }
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = directory.metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o1000 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private directory must be owned by the current user and not shared",
            ));
        }
        if metadata.mode() & 0o7777 != 0o700 {
            directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Open without following symlinks and repair existing modes before use.
/// Callers must truncate only after this function has validated the file.
pub fn open(path: &Path, options: &OpenOptions) -> io::Result<File> {
    #[allow(unused_mut)]
    let mut options = options.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private file must be a singly linked regular file owned by the current user",
            ));
        }
        if metadata.mode() & 0o7777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let mut file = open(path.as_ref(), OpenOptions::new().write(true).create(true))?;
    file.set_len(0)?;
    file.write_all(contents.as_ref())
}

pub fn append(path: &Path) -> io::Result<File> {
    open(path, OpenOptions::new().append(true).create(true))
}

/// Repair an older file without creating an absent one or changing its bytes.
pub fn repair_if_exists(path: &Path) -> io::Result<()> {
    match open(path, OpenOptions::new().read(true)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn directory() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pmux-private-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn repairs_directory_and_files_without_losing_content() {
        let dir = directory();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        create_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = dir.join("state");
        fs::write(&path, b"retained").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        repair_if_exists(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"retained");
        append(&path).unwrap().write_all(b"\nnext").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"retained\nnext");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let fresh = dir.join("fresh");
        write(&fresh, b"private").unwrap();
        assert_eq!(
            fs::metadata(fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_links_without_modifying_the_target() {
        let dir = directory();
        let target = dir.join("target");
        fs::write(&target, b"keep").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("symlink");
        symlink(&target, &link).unwrap();
        assert!(write(&link, b"overwrite").is_err());
        let hard = dir.join("hardlink");
        fs::hard_link(&target, &hard).unwrap();
        assert!(write(&hard, b"overwrite").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let dir_link = dir.join("dirlink");
        symlink(&dir, &dir_link).unwrap();
        assert!(create_dir(&dir_link).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
