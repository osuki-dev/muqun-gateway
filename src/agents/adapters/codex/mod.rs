//! Private stdio app-server adapter, pinned to Codex rust-v0.162.1.
mod budget;
pub mod client;
mod driver;
pub mod mapper;
pub(crate) mod process_tree;

pub use client::{CodexClient, CodexEvent};
pub use driver::CodexDriver;

pub const KIND: &str = "codex";
pub const VERSION: &str = "0.162.1";
pub(crate) const MAX_REASONING_PARTS: usize = 512;

#[cfg(test)]
pub(crate) mod tests;
