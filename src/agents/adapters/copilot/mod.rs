//! GitHub Copilot CLI's official ACP entry point; no cloud pull-request wrapper.
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider, ConfigBypass};

mod store;

pub struct CopilotProvider;

impl AcpProvider for CopilotProvider {
    const KIND: &'static str = "copilot";
    const LABEL: &'static str = "GitHub Copilot CLI";
    const ARGS: &'static [&'static str] = &["--acp", "--stdio", "--no-auto-update"];
    const ENV: &'static [(&'static str, &'static str)] = &[("MISE_AUTO_INSTALL", "false")];
    const REQUIRE_MESSAGE_ID: bool = false;
    const MODEL_SELECTION: bool = true;
    const SESSION_MODES: bool = true;
    const PROVIDER_CONFIG: bool = false;
    const EFFORT_CONFIG: &'static str = "reasoning_effort";
    const NON_INFERENCE_COMMANDS: &'static [&'static str] = &["context", "usage"];
    // Autopilot is an autonomy mode; permissions have an independent selector.
    const BYPASS_CONFIG: Option<ConfigBypass> = Some(ConfigBypass {
        id: "allow_all",
        enabled: "on",
        disabled: "off",
    });
    // No login/authenticate request or blanket permission flag is sent.
    const TASK_SENTENCE_IN_PROMPT: bool = true;
    const PREVIEW_TIMELINE: bool = true;
    fn preview_timeline<'a>(
        _directory: &'a str,
        id: &'a str,
        limit: usize,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> crate::agents::ports::agent::AgentFuture<'a, Vec<crate::agents::domain::TimelineItem>>
    {
        Box::pin(store::tail_timeline(id, limit, stop))
    }
}

pub type CopilotDriver = AcpDriver<CopilotProvider>;

#[cfg(all(test, unix))]
mod tests;
