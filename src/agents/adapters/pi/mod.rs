//! Pi (pi.dev) agent adapter.
//!
//! Pi is a local coding agent with no server: the gateway drives it through
//! its RPC mode (`pi --mode rpc`), strict JSONL over a child's stdin/stdout,
//! one child per live session. Pi owns its session files
//! (`~/.pi/agent/sessions/**.jsonl`); listings, titles and history are read
//! from them without a child; explicit foreground selection or a write starts
//! a child. The
//! wire contract is written up in `docs/pi-protocol.md`.
//!
//! Layout:
//! - `rpc.rs`: the process host: JSONL framing, request/response
//!   correlation, event fan-out, idle reaping.
//! - `sessions.rs`: the read-only view of the session files.
//! - `mapper.rs`: session entries and live RPC events into the neutral
//!   domain, plus the bundled extension (`muqun-extension.js`) that asks the
//!   gateway before `bash`/`edit`/`write` and registers the Muqun MCP server.
//! - `driver.rs`: `AgentPort` implementation; owns the children and the
//!   per-session live state, and forwards every child record to the manager
//!   (`agents/manager/pi.rs`), which folds it into `AgentDomainEvent`s.
//!
//! The runtime (`agents/runtime/pi.rs`) attaches it when `pi.enabled` (the
//! default) and a `pi` binary is found.

pub mod driver;
mod fork;
pub mod mapper;
mod prompt;
pub mod rpc;
pub mod sessions;

pub use driver::PiDriver;
pub use rpc::PiCommandLine;
pub use sessions::SessionStore;

/// The agent id the runtime registers this adapter under.
pub const KIND: &str = "pi";

/// The endpoint discovery reports: there is no URL, only a child process.
pub const ENDPOINT: &str = "stdio://pi";
