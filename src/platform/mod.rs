//! Platform infrastructure domain: discovery, git porcelain, i18n, service management, and state locks.

pub mod assets;
pub(crate) mod chunked_uploads;
pub mod config;
pub mod discovery;
pub(crate) mod file_ranges;
pub(crate) mod git;
pub(crate) mod http;
pub mod i18n;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod launchd;
pub(crate) mod lifecycle;
pub(crate) mod local_client;
pub(crate) mod manage;
pub(crate) mod metadata;
pub(crate) mod openapi;
pub(crate) mod parts;
pub(crate) mod routes;
pub(crate) mod server;
pub(crate) mod service;
pub(crate) mod setup;
pub(crate) mod state_lock;
pub(crate) mod store;
pub(crate) mod update;
pub(crate) mod uploads;
pub(crate) mod vcs_routes;
pub(crate) mod video;

#[allow(unused_imports)]
pub use openapi::{openapi_spec, DOCS_HTML};
pub use store::set_directories;
pub use uploads::{store_upload, StoredUpload, UploadRefusal};

#[cfg(all(test, unix))]
pub(crate) mod installer_tests;
