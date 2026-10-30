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
    fn global_catalog<'a>(
        binary: &'a str,
        directory: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<'a, crate::agents::domain::AgentCatalog> {
        Box::pin(super::native_cli::read_catalog(binary, directory, stop))
    }
    fn account_limits<'a>(
        binary: &'a str,
        directory: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<
        'a,
        crate::agents::domain::account_limits::AccountLimits,
    > {
        Box::pin(async move {
            let value =
                super::native_cli::read_json(binary, &["usage", "--json"], directory, stop).await?;
            Ok(super::native_cli::account_limits(&value))
        })
    }
    fn safe_error_message(error: &Value) -> Option<&'static str> {
        super::client::safe_error_message(error)
    }
}
pub type FxDriver = AcpDriver<FxProvider>;
