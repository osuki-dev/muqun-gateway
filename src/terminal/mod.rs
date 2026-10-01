//! Terminal business domain: workspaces, panes, multiplexers, and terminal I/O.

pub mod backend;
pub mod backend_startup;
pub mod command_catalog;
pub mod composer;
pub mod factory;
pub mod login_env;
pub mod native;
pub mod routes;
pub mod scrollback;
pub mod shortcuts;
pub mod supervision;
