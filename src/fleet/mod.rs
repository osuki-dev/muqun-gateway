//! This machine's capability manifest: identity, the discovery planes, a small
//! toolchain probe and what the machine is suitable for.
pub(crate) mod manifest;
pub(crate) mod peer_client;
pub(crate) mod peer_routes;
pub(crate) mod peers;
pub(crate) mod peers_sqlite;
pub(crate) mod probes;
pub(crate) mod routes;

pub(crate) use manifest::FleetService;
