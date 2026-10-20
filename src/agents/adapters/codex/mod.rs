//! Shared-daemon adapter. Codex 0.162.1 is the oldest accepted app-server.
mod account_limits;
mod async_questions;
mod budget;
pub mod client;
pub(crate) mod daemon;
mod driver;
pub(crate) mod inbox;
mod input;
mod items;
mod launch;
pub mod mapper;
pub(crate) mod native_sessions;
pub(crate) mod process_tree;
pub(crate) mod rollout;
pub(crate) mod rollout_projection;
pub(crate) mod rollout_tail;
pub(crate) mod usage;

pub use client::{CodexClient, CodexEvent};
pub use driver::CodexDriver;

pub const KIND: &str = "codex";
/// Oldest Codex app-server this Gateway will drive. Newer releases stay connected.
pub const VERSION: &str = "0.162.1";
const MINIMUM_VERSION: (u64, u64, u64) = (0, 162, 1);

/// Whether `user_agent` names Codex 0.162.1 or anything newer.
pub(crate) fn accepts_server_version(user_agent: &str) -> bool {
    user_agent
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(numeric_version)
        .any(|version| version >= MINIMUM_VERSION)
}

fn numeric_version(token: &str) -> Option<(u64, u64, u64)> {
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}
pub(crate) const MAX_REASONING_PARTS: usize = 512;

#[cfg(test)]
pub(crate) mod tests;
