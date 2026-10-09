//! The task entity: what the gateway was asked to get done, where it stands and
//! how it got there. Persisted through the `TaskStore` port; driven by
//! `TaskService`.
pub(crate) mod domain;
pub(crate) mod driver;
pub(crate) mod events;
pub(crate) mod ports;
pub(crate) mod routes;
pub(crate) mod run_command;
pub(crate) mod service;
pub(crate) mod store_sqlite;
