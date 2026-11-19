//! GitHub Copilot CLI's official ACP entry point; no cloud pull-request wrapper.
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};

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
    // Autopilot is an autonomy mode, not the App's permission toggle. The
    // separate allow_all config is not exposed as auto-approve in this adapter.
    // No login/authenticate request or blanket permission flag is sent.
    const TASK_SENTENCE_IN_PROMPT: bool = true;
}

pub type CopilotDriver = AcpDriver<CopilotProvider>;
