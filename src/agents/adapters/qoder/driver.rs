//! Qoder CLI uses ACP on its explicit headless entry point.
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};
pub struct QoderProvider;
impl AcpProvider for QoderProvider {
    const KIND: &'static str = "qoder";
    const LABEL: &'static str = "Qoder";
    const ARGS: &'static [&'static str] = &["--acp"];
    const REQUIRE_MESSAGE_ID: bool = false;
    const MODEL_SELECTION: bool = false;
}
pub type QoderDriver = AcpDriver<QoderProvider>;
