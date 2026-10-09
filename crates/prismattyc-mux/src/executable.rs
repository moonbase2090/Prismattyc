//! Locate companion binaries in the same installation.

use std::path::{Path, PathBuf};

pub fn sibling(executable: &Path, names: &[&str]) -> Option<PathBuf> {
    let directory = executable.parent()?;
    names
        .iter()
        .map(|name| directory.join(crate::platform::executable_name(name)))
        .find(|path| path.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn companions_follow_the_real_binary_through_a_path_symlink() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("pmux-sibling-{}", std::process::id()));
        let bundle = root.join("Prismattyc.app/Contents/MacOS");
        let bin = root.join("bin");
        fs::create_dir_all(&bundle).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(bundle.join("pmux"), b"binary").unwrap();
        fs::write(bundle.join("pmuxd"), b"daemon").unwrap();
        fs::write(bundle.join("pmux-attach"), b"attach").unwrap();
        let link = bin.join("pmux");
        symlink("../Prismattyc.app/Contents/MacOS/pmux", &link).unwrap();
        let daemon = sibling(&link, &["pmuxd"]);
        let attach = sibling(&link, &["pmux-attach"]);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(daemon, Some(bundle.join("pmuxd")));
        assert_eq!(attach, Some(bundle.join("pmux-attach")));
    }
}
