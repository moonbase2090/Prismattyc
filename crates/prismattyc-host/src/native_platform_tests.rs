// SPDX-License-Identifier: MIT
use crate::native_platform::TargetPlatform;

#[test]
fn compilation_gate_matches_the_actual_target_and_advertised_dialects() {
    let platform = TargetPlatform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        pointer_width: usize::BITS,
        little_endian: cfg!(target_endian = "little"),
        atomic64: cfg!(target_has_atomic = "64"),
    };
    assert_eq!(cfg!(native_transport), platform.supports_native());
    #[cfg(native_transport)]
    assert!(!crate::native::profile::dialects().is_empty());
}
