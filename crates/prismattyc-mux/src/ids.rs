//! Opaque stable typed IDs. Never reused for the lifetime of a [`crate::Domain`].

/// Session identity (named grouping of windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

/// Window identity (tab within a session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(u64);

/// Pane identity (leaf that will own one PTY+emulator at runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

/// Attached GUI/control client (controller lease holder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

macro_rules! id_impl {
    ($name:ident) => {
        impl $name {
            /// Construct from a raw counter value (crate-internal minting).
            #[inline]
            pub(crate) const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// Stable numeric view for logging and control-plane wire formats.
            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

id_impl!(SessionId);
id_impl!(WindowId);
id_impl!(PaneId);
id_impl!(ClientId);
