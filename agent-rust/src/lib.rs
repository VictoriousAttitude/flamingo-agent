//! Flamingo agent library. Everything except the binary entry point lives here so it can be
//! exercised by unit and integration tests.
//!
//! Module layout (see `docs/design.md` §3): portable modules never contain `cfg`; every
//! platform call lives in `platform/` or `service/`.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod child;
pub mod cli;
pub mod config;
pub mod logging;
pub mod metrics;
pub mod platform;
