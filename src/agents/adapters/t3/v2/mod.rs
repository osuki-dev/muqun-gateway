//! Protocol 2, pinned to stable v0.1.0 (81493efb); contract-additive over
//! the v0.0.46-nightly.20261013.3042 (5d6e4192) it was written against.
//! These are V2 commands and projections, not V1-shaped wire payloads.
pub mod commands;
pub mod mapper;
pub mod projection;

#[cfg(test)]
mod tests;
