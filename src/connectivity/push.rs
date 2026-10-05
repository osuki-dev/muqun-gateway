//! Push notification payloads and the device records that receive them.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::approvals;
use crate::platform::i18n::{self, Locale};
use crate::platform::manage::truncate;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PushTokenRecord {
    pub(crate) token: String,
    pub(crate) platform: String,
    pub(crate) device_name: Option<String>,
    /// The language this device asked to be notified in, as the exact code the
    /// app persists (`en`, `zh-TW`, `zh-CN`, `ru` and so on; see `i18n.rs`).
    ///
    /// A push is built in a watcher spawned at startup, where there is no
    /// request and therefore no header to read, so the language has to have
    /// been remembered from the last one there was. It is optional and
    /// `#[serde(default)]` because a `push-tokens.json` written before this
    /// field existed -- and a client too old to send it -- must keep working;
    /// both simply get English.
    #[serde(default)]
    pub(crate) locale: Option<String>,
    pub(crate) updated_unix_ms: u128,
}

impl PushTokenRecord {
    /// The language to write this device's pushes in. Anything unrecognized,
    /// including nothing at all, is English.
    pub(crate) fn locale(&self) -> Locale {
        self.locale
            .as_deref()
            .and_then(Locale::from_code)
            .unwrap_or_default()
    }
}

/// A push after it has been put into words: exactly what one Expo message
/// carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AgentPushNotification {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) data: serde_json::Map<String, Value>,
}

/// What happened, in the only three ways this gateway ever raises a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentNotice {
    ApprovalPending,
    AgentBlocked,
    AgentCompleted,
}

/// A push *before* it has been put into words.
///
/// The watchers that raise these are spawned at startup and run for the life of
/// the process; there is no request in scope and therefore no header to read a
/// language off. Choosing the words there would mean choosing one language for
/// every phone. So the ingredients travel instead, and [`AgentPushNotice::render`]
/// is called once per distinct locale among the registered devices -- each phone
/// is notified in the language it registered with.
///
/// Everything held here is either locale-free or a name a person chose: the ids
/// and urls in `data`, the server label the user typed, the agent's own name,
/// and the answers as [`approvals::PushChoice`], which is an index and a
/// decision and nothing the agent wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPushNotice {
    pub(crate) notice: AgentNotice,
    /// The label the user gave this server. Never translated -- it is theirs.
    pub(crate) server_label: String,
    /// The agent's own name. `None` renders as the reader's word for an agent.
    pub(crate) agent_name: Option<String>,
    pub(crate) data: serde_json::Map<String, Value>,
    /// Only an approval has these, and only ever as indices and decisions.
    pub(crate) choices: Vec<approvals::PushChoice>,
    /// The agent's own words, present only when the owner of the machine turned
    /// `rich_agent_pushes` on. The single exception to the rule above, and the
    /// reason the flag exists rather than the behaviour.
    pub(crate) detail: Option<PushDetail>,
}

/// What a blocked push says when the operator has opted into it: the question
/// the agent asked, and the answers it is offering, both verbatim and both cut
/// short.
///
/// Verbatim because a paraphrase of "Run `rm -rf build/`?" is not something to
/// approve from a lock screen, and cut short because a notification is a
/// glance: past a line or two the reader is opening the app anyway, which is
/// the outcome this is trying to make unnecessary rather than the one it is
/// trying to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushDetail {
    pub(crate) question: String,
    pub(crate) option_labels: Vec<String>,
}

/// A glance's worth of the agent's question.
pub(crate) const MAX_PUSH_QUESTION_CHARS: usize = 120;

/// Three answers is what a notification's action row shows; a menu with more
/// is one the user opens the app for.
pub(crate) const MAX_PUSH_OPTIONS: usize = 3;

pub(crate) const MAX_PUSH_OPTION_CHARS: usize = 40;

impl PushDetail {
    pub(crate) fn from_approval(approval: &approvals::Approval) -> Self {
        Self {
            question: truncate(approval.prompt.trim(), MAX_PUSH_QUESTION_CHARS),
            option_labels: approval
                .options
                .iter()
                .take(MAX_PUSH_OPTIONS)
                .map(|option| truncate(option.label.trim(), MAX_PUSH_OPTION_CHARS))
                .collect(),
        }
    }

    /// A question with no answers to offer, such as a form's title; `None`
    /// when there is no question to quote.
    pub(crate) fn from_question(question: &str) -> Option<Self> {
        let question = question.trim();
        (!question.is_empty()).then(|| Self {
            question: truncate(question, MAX_PUSH_QUESTION_CHARS),
            option_labels: Vec::new(),
        })
    }
}

impl AgentPushNotice {
    pub(crate) fn render(&self, locale: Locale) -> AgentPushNotification {
        let (heading, body) = match self.notice {
            AgentNotice::ApprovalPending => {
                ("Approval needed", "{name} is waiting for your approval.")
            }
            AgentNotice::AgentBlocked => ("Agent blocked", "{name} needs your input."),
            AgentNotice::AgentCompleted => ("Agent done", "{name} finished running."),
        };
        // Title carries which server so a multi-server user knows where to
        // look; body carries which agent and what happened. Only the server
        // label (which the user set) and the agent name -- never terminal
        // output or prompts.
        let heading = i18n::t(locale, heading);
        let title = if self.server_label.is_empty() {
            heading.to_owned()
        } else {
            format!("{heading} · {}", truncate(&self.server_label, 32))
        };
        let agent = match &self.agent_name {
            Some(name) => name.clone(),
            None => i18n::t(locale, "Agent").to_owned(),
        };
        let mut data = self.data.clone();
        if !self.choices.is_empty() {
            data.insert(
                "options".into(),
                json!(approvals::push_options(&self.choices, locale)),
            );
        }
        // The agent's own words are not translated and never will be: they are
        // a quotation. When they are here at all, they are also the body -- the
        // question is the whole reason the owner turned this on, and "{name}
        // needs your input." above it would be a line saying nothing.
        let body = match &self.detail {
            Some(detail) => {
                data.insert("question".into(), json!(detail.question));
                if !detail.option_labels.is_empty() {
                    data.insert("option_labels".into(), json!(detail.option_labels));
                }
                detail.question.clone()
            }
            None => i18n::t_slots(locale, body, &[("name", &agent)]),
        };
        AgentPushNotification { title, body, data }
    }
}
