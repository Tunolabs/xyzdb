// SPDX-License-Identifier: BUSL-1.1
// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod distance;
pub mod error;
pub mod field_dict;
pub mod key;
pub mod lid;
pub mod lobe;
pub mod record;
pub mod result;
pub mod value;
pub mod zorder;
