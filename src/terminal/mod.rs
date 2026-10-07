//! Terminal business domain: workspaces, panes, multiplexers, and terminal I/O.

pub mod backend;
pub mod backend_startup;
pub mod command_catalog;
pub mod composer;
pub mod factory;
pub(crate) mod history;
pub(crate) mod history_memory;
pub(crate) mod history_routes;
pub(crate) mod history_sqlite;
pub mod login_env;
pub mod native;
pub mod routes;
pub mod scrollback;
#[cfg(test)]
mod scrollback_replay;
pub mod shortcuts;
pub mod supervision;
