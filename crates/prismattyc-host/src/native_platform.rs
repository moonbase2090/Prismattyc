// SPDX-License-Identifier: MIT
//! One target predicate controls native compilation and capability availability.

pub struct TargetPlatform<'a> {
    pub os: &'a str,
    pub arch: &'a str,
    pub pointer_width: u32,
    pub little_endian: bool,
    pub atomic64: bool,
}

impl TargetPlatform<'_> {
    pub fn supports_native(&self) -> bool {
        self.pointer_width == 64
            && self.little_endian
            && self.atomic64
            && matches!((self.os, self.arch), ("linux", _) | ("macos", "aarch64"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_requires_the_supported_os_and_complete_memory_model() {
        for (os, arch, pointer_width, little_endian, atomic64, expected) in [
            ("linux", "aarch64", 64, true, true, true),
            ("linux", "x86_64", 64, true, true, true),
            ("macos", "aarch64", 64, true, true, true),
            ("macos", "x86_64", 64, true, true, false),
            ("windows", "x86_64", 64, true, true, false),
            ("freebsd", "x86_64", 64, true, true, false),
            ("linux", "x86", 32, true, true, false),
            ("linux", "powerpc64", 64, false, true, false),
            ("linux", "aarch64", 64, true, false, false),
            ("macos", "aarch64", 64, false, true, false),
            ("", "", 0, false, false, false),
        ] {
            let platform = TargetPlatform {
                os,
                arch,
                pointer_width,
                little_endian,
                atomic64,
            };
            assert_eq!(
                platform.supports_native(),
                expected,
                "{os}/{arch}/{pointer_width}"
            );
        }
    }
}
