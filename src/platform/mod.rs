//! Platform infrastructure domain: discovery, git porcelain, i18n, service management, and state locks.

pub mod discovery;
pub mod git;
pub mod i18n;
pub mod openapi;
pub mod parts;
pub mod service;
pub mod state_lock;

pub use openapi::{openapi_spec, DOCS_HTML};

#[cfg(all(test, unix))]
pub mod installer_tests;
