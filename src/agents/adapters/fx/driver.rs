//! FX-specific ACP launch policy. The session lifecycle and reducer are shared.
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};
use serde_json::Value;
pub struct FxProvider;
impl AcpProvider for FxProvider {
    const KIND: &'static str = "fx";
    const LABEL: &'static str = "FX";
    const ARGS: &'static [&'static str] = &["acp"];
    const REQUIRE_MESSAGE_ID: bool = true;
    const MODEL_SELECTION: bool = true;
    fn safe_error_message(error: &Value) -> Option<&'static str> {
        super::client::safe_error_message(error)
    }
}
pub type FxDriver = AcpDriver<FxProvider>;
