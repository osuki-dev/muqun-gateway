//! Shared-daemon or private stdio adapter, pinned to Codex rust-v0.162.1.
mod async_questions;
mod budget;
pub mod client;
pub(crate) mod daemon;
mod driver;
mod input;
mod items;
mod launch;
pub mod mapper;
pub(crate) mod process_tree;
pub(crate) mod rollout;
pub(crate) mod rollout_projection;
pub(crate) mod rollout_tail;
pub(crate) mod usage;

pub use client::{CodexClient, CodexEvent};
pub use driver::CodexDriver;

pub const KIND: &str = "codex";
pub const VERSION: &str = "0.162.1";
pub(crate) const MAX_REASONING_PARTS: usize = 512;

#[cfg(test)]
pub(crate) mod tests;
