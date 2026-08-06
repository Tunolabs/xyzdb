// SPDX-License-Identifier: BUSL-1.1
// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod ast;
pub mod parser;

#[cfg(test)]
mod tests;

pub use parser::{parse, parse_multi};
