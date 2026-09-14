//! Package version plus the git stamp from [`build.rs`](../build.rs).
//!
//! Workspace Cargo.toml was frozen at 0.1.1 with the classic claim, so
//! `CARGO_PKG_VERSION` alone does not move when `prismattyc update`
//! installs a new tip. The git hash does.

/// Workspace package version (`CARGO_PKG_VERSION`).
#[must_use]
pub fn package_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Short git SHA baked at compile time. Empty when git is unavailable.
#[must_use]
pub fn git_hash() -> &'static str {
    option_env!("PRISMATTYC_GIT_HASH").unwrap_or("")
}

/// ` (2c82cd4a0e20)` or empty. Splash header and `--version` share this.
#[must_use]
pub fn git_suffix() -> String {
    let hash = git_hash();
    if hash.is_empty() {
        String::new()
    } else {
        format!(" ({hash})")
    }
}

/// `0.1.32 (2c82cd4a0e20)` — splash line and `--version` share this.
#[must_use]
pub fn release_label() -> String {
    format!("{}{}", package_version(), git_suffix())
}

/// `prismattyc 0.1.32 (2c82cd4a0e20)` — first line help2man can parse.
#[must_use]
pub fn bin_version(name: &str) -> String {
    format!("{name} {}", release_label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_version_is_semverish() {
        let v = package_version();
        assert!(v.split('.').count() >= 2, "expected major.minor: {v:?}");
        assert!(v.chars().next().is_some_and(|c| c.is_ascii_digit()));
    }

    #[test]
    fn bin_version_starts_with_name_and_contains_package() {
        let line = bin_version("prismattyc");
        assert!(line.starts_with("prismattyc "), "{line:?}");
        assert!(line.contains(package_version()), "{line:?}");
        assert_eq!(line, format!("prismattyc {}", release_label()));
    }
}
