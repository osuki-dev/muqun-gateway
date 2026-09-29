//! T3 Code (t3.codes) agent adapter.
//!
//! T3 Code is a Node server (`t3 serve`) that wraps Codex, the Claude Agent
//! SDK, OpenCode and ACP agents behind one orchestration model of projects,
//! threads and turns. It serves its clients over Effect RPC (JSON frames) on
//! a WebSocket at `GET /ws`, authenticated with a short-lived ticket minted
//! from a bearer token, which in turn is exchanged from a one-time pairing
//! credential. The wire protocol is written up in `docs/t3-protocol.md`.
//!
//! Layout:
//! - `endpoint.rs`: discovery (`/.well-known/t3/environment`) and the auth
//!   exchanges (pairing credential -> bearer -> WebSocket ticket).
//! - `rpc.rs`: Effect RPC framing over `tokio-tungstenite`: request ids,
//!   response correlation, stream subscriptions with acks, ping/pong,
//!   reconnect with backoff and cancellation.
//! - `client.rs`: typed wrappers for the orchestration commands and queries.
//! - `mapper.rs`: T3 read-model and event payloads into the neutral domain.
//! - `driver.rs`: `AgentPort` implementation.
//! - `stream.rs`: shell and thread subscriptions parsed into typed events for
//!   the manager (`agents/manager/t3.rs`) to fold into `AgentDomainEvent`s.
//!
//! The runtime (`agents/runtime/t3.rs`) attaches it when `t3.enabled` or
//! `t3.url` is set in `config.json`, and keeps the bearer it pairs for.

pub mod client;
pub mod driver;
pub mod endpoint;
pub mod mapper;
pub mod rpc;
pub mod stream;

pub use client::T3Client;
pub use driver::T3Driver;
pub use endpoint::{T3Credential, T3Endpoint};
pub use stream::{T3StreamEvent, T3StreamListener};

/// The agent id the runtime registers this adapter under.
pub const KIND: &str = "t3";

/// The orchestration wire version this adapter was written against. A
/// descriptor that omits the field means protocol 1.
pub const ORCHESTRATION_PROTOCOL_VERSION: u64 = 1;

/// Upper bound on any HTTP body or WebSocket frame this adapter will parse.
/// Thread snapshots carry every message and activity of a thread, so this is
/// generous, but a runaway payload must not grow the gateway without limit.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
