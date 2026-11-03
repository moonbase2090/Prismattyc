// SPDX-License-Identifier: MIT
//! Native host for versioned contained components. No WASI imports are linked.

mod component;

pub use component::{ActiveComponent, ActivityProvider, PreparedComponent, Runtime};
