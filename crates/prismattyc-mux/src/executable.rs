//! Locate companion binaries in the same installation.

use std::path::{Path, PathBuf};

pub fn sibling(executable: &Path, names: &[&str]) -> Option<PathBuf> {
    let resolved = executable.canonicalize().ok();
    let executable = resolved.as_deref().unwrap_or(executable);
    let directory = executable.parent()?;
    names
        .iter()
        .map(|name| directory.join(crate::platform::executable_name(name)))
        .find(|path| path.is_file())
}

/// Finder launches have a minimal PATH. A standalone CLI can still use the
/// helpers installed in the user's app bundle, then the system app bundle.
pub fn app_companion(names: &[&str]) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut bundles = Vec::new();
        if let Some(home) = crate::platform::home_dir() {
            bundles.push(PathBuf::from(home).join("Applications/Prismattyc.app"));
        }
        bundles.push(PathBuf::from("/Applications/Prismattyc.app"));
        for bundle in bundles {
            for name in names {
                let path = bundle.join("Contents/MacOS").join(name);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    let _ = names;
    None
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
