//! Platform infrastructure domain: discovery, git porcelain, i18n, service management, and state locks.

pub mod assets;
pub mod config;
pub mod discovery;
pub mod git;
pub mod http;
pub mod i18n;
pub mod lifecycle;
pub mod manage;
pub mod metadata;
pub mod openapi;
pub mod parts;
pub mod routes;
pub mod server;
pub mod service;
pub mod setup;
pub mod state_lock;
pub mod store;
pub mod uploads;
pub mod vcs_routes;

#[allow(unused_imports)]
pub use openapi::{openapi_spec, DOCS_HTML};

#[cfg(all(test, unix))]
pub mod installer_tests;
