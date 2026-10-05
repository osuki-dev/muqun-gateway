#![allow(dead_code)] // the port and adapter surface for capabilities not yet routed

pub mod adapters;
pub mod agent_events;
pub mod approvals;
pub mod directories;
pub mod domain;
pub mod manager;
pub mod ports;
pub mod routes;
pub mod runtime;
pub mod session_routes;
pub mod tasks;
pub mod use_cases;
pub mod ws_routes;

pub use domain::*;
pub use runtime::{AgentRuntime, DeepseekConfig, OpencodeConfig, T3Config};
