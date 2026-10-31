//! Qoder CLI uses ACP on its explicit headless entry point.
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};
pub struct QoderProvider;
impl AcpProvider for QoderProvider {
    const KIND: &'static str = "qoder";
    const LABEL: &'static str = "Qoder";
    const ARGS: &'static [&'static str] = &["--acp"];
    fn session_metadata<'a>(
        rows: Vec<serde_json::Value>,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(super::native_sessions::recover(rows, stop))
    }
    const REQUIRE_MESSAGE_ID: bool = false;
    const MODEL_SELECTION: bool = true;
    const IDLE_RETIREMENT: bool = true;
    fn recovery_snapshot<'a>(
        _binary: &'a str,
        _directory: &'a str,
        id: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<
        'a,
        crate::agents::adapters::acp::recovery::NativeSnapshot,
    > {
        Box::pin(super::recovery::inspect(id, stop))
    }
    // Every `qodercli` launch leaves a native log session behind, so the
    // model list is reused longer; failures still retry within seconds.
    const CATALOG_TTL: std::time::Duration = std::time::Duration::from_secs(600);
    fn session_store_fingerprint() -> Option<u64> {
        super::native_sessions::store_fingerprint()
    }
    const PROVIDER_CONFIG: bool = false;
    const EFFORT_CONFIG: &'static str = "reasoning_effort";
    fn global_catalog<'a>(
        binary: &'a str,
        directory: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<'a, crate::agents::domain::AgentCatalog> {
        Box::pin(async move {
            let bytes = crate::agents::adapters::acp::session::read_metadata_output(
                binary,
                &["--list-models"],
                directory,
                stop,
            )
            .await?;
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                crate::agents::ports::agent::AgentError::Protocol(
                    "Qoder model list was not UTF-8".into(),
                )
            })?;
            catalog(text)
        })
    }
}
pub type QoderDriver = AcpDriver<QoderProvider>;

fn catalog(
    text: &str,
) -> Result<crate::agents::domain::AgentCatalog, crate::agents::ports::agent::AgentError> {
    use crate::agents::domain::{AgentCatalog, ModelInfo};
    use crate::agents::ports::agent::AgentError;
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("MODEL") {
        return Err(AgentError::Protocol(
            "Qoder model list has an unknown format".into(),
        ));
    }
    let mut models = std::collections::BTreeMap::new();
    for id in lines.map(str::trim).filter(|line| !line.is_empty()) {
        if id.len() > 256 || id.chars().any(char::is_control) {
            return Err(AgentError::Protocol(
                "Qoder model list contains an invalid model id".into(),
            ));
        }
        models.entry(id.to_owned()).or_insert_with(|| ModelInfo {
            id: id.into(),
            name: id.into(),
            provider_id: "qoder".into(),
            family: None,
            limit: None,
            variants: None,
            cost: None,
            enabled: true,
            status: None,
        });
    }
    Ok(AgentCatalog {
        models: models.into_values().collect(),
        ..Default::default()
    })
}
