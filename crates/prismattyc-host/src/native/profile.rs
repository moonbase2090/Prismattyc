// SPDX-License-Identifier: MIT
//! Transport tokens correspond to live dialects, independently of cell alpha.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
    Grid0,
    Images2,
}

impl Dialect {
    pub fn minor(self) -> u16 {
        match self {
            Self::Grid0 => 0,
            Self::Images2 => 2,
        }
    }
    pub fn token(self) -> &'static str {
        match self {
            Self::Grid0 => "native.grid.v1",
            Self::Images2 => "native.images.v1",
        }
    }
    pub fn supported(self) -> bool {
        dialects().contains(&self)
    }
}

pub fn dialects() -> &'static [Dialect] {
    let platform = crate::native_platform::TargetPlatform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        pointer_width: usize::BITS,
        little_endian: cfg!(target_endian = "little"),
        atomic64: cfg!(target_has_atomic = "64"),
    };
    if !platform.supports_native() {
        return &[];
    }
    if cfg!(target_os = "linux") {
        &[Dialect::Grid0, Dialect::Images2]
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        &[Dialect::Images2]
    } else {
        &[]
    }
}

pub fn feature_list() -> String {
    dialects()
        .iter()
        .map(|dialect| dialect.token())
        .collect::<Vec<_>>()
        .join(",")
}

pub fn encode_reply(offer: &super::authority::PreparedOffer) -> Option<Vec<u8>> {
    use std::fmt::Write;
    let features = feature_list();
    if features.is_empty() {
        return None;
    }
    let mut nonce = String::with_capacity(32);
    for byte in offer.value {
        write!(nonce, "{byte:02x}").unwrap();
    }
    Some(format!("\x1b_Prismattyc;cap;r;id={};v=0.4;features={features};native.offer={nonce};native.generation={}\x1b\\",
        offer.capability_id.get(), offer.epoch.get()).into_bytes())
}
