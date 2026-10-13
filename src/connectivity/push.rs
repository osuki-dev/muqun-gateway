//! Push notification payloads and the device records that receive them.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::approvals;
use crate::platform::i18n::{self, Locale};
use crate::platform::manage::truncate;
use crate::{AppState, DeviceRecord};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PushTokenRecord {
    pub(crate) token: String,
    /// Only a currently paired App may receive pushes. Legacy unowned records
    /// remain readable but must be authenticated and registered again to send.
    #[serde(default)]
    pub(crate) device_id: Option<String>,
    /// Distinguishes re-registration even after unregistering in the same ms.
    #[serde(default)]
    pub(crate) registration_id: Option<String>,
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

fn has_paired_owner(record: &PushTokenRecord, devices: &[DeviceRecord]) -> bool {
    record.device_id.as_deref().is_some_and(|id| {
        devices
            .iter()
            .any(|device| device.id == id && device.kind.is_app())
    })
}

/// All push readers take devices before tokens; poisoning either fails closed.
pub(crate) fn registered_recipients(state: &AppState) -> anyhow::Result<Vec<PushTokenRecord>> {
    let devices = state
        .devices
        .lock()
        .map_err(|_| anyhow::anyhow!("device lock failed"))?;
    let tokens = state
        .push_tokens
        .lock()
        .map_err(|_| anyhow::anyhow!("push token lock failed"))?;
    Ok(tokens
        .iter()
        .filter(|record| has_paired_owner(record, &devices))
        .cloned()
        .collect())
}

/// Admission is repeated immediately before every Expo batch, not just when a
/// watcher snapshots recipients. Revoke/replacement and registration changes
/// are serialized by devices -> tokens. Locks never cross the network await.
/// Once admitted, an in-flight batch cannot be recalled by a later revocation.
fn admit_batch(
    state: &AppState,
    candidates: &[PushTokenRecord],
) -> anyhow::Result<Vec<PushTokenRecord>> {
    let devices = state
        .devices
        .lock()
        .map_err(|_| anyhow::anyhow!("device lock failed"))?;
    let tokens = state
        .push_tokens
        .lock()
        .map_err(|_| anyhow::anyhow!("push token lock failed"))?;
    Ok(candidates
        .iter()
        .filter(|record| {
            has_paired_owner(record, &devices) && tokens.iter().any(|current| current == *record)
        })
        .cloned()
        .collect())
}

pub(crate) async fn send_registered_push_notifications(
    state: &AppState,
    candidates: &[PushTokenRecord],
    title: String,
    body: String,
    data: serde_json::Map<String, Value>,
) -> anyhow::Result<Value> {
    send_batches(state, candidates, |admitted| {
        let title = title.clone();
        let body = body.clone();
        let data = data.clone();
        async move { crate::send_expo_push_notifications(&admitted, title, body, data).await }
    })
    .await
}

async fn send_batches<F, Fut>(
    state: &AppState,
    candidates: &[PushTokenRecord],
    mut send: F,
) -> anyhow::Result<Value>
where
    F: FnMut(Vec<PushTokenRecord>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Value>>,
{
    let mut tickets = Vec::new();
    // Expo accepts at most 100 messages per request.
    for batch in candidates.chunks(100) {
        let admitted = admit_batch(state, batch)?;
        if admitted.is_empty() {
            continue;
        }
        let result = send(admitted).await?;
        if let Some(data) = result.get("data").and_then(Value::as_array) {
            tickets.extend(data.iter().cloned());
        }
    }
    Ok(json!({ "data": tickets }))
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

/// What happened, before a registered device chooses its language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentNotice {
    ApprovalPending,
    AgentBlocked,
    AgentInputRequired,
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
            AgentNotice::AgentInputRequired => ("Needs your input", "{name} needs your input."),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    fn registration(token: &str, owner: Option<&str>) -> PushTokenRecord {
        PushTokenRecord {
            token: token.into(),
            device_id: owner.map(str::to_owned),
            registration_id: Some(uuid::Uuid::new_v4().to_string()),
            platform: "ios".into(),
            device_name: None,
            locale: Some("zh-TW".into()),
            updated_unix_ms: 1,
        }
    }

    #[test]
    fn recipients_require_current_app_membership_and_exact_registration() {
        let state = test_state(
            "admin",
            vec![
                test_device("app", "app-token"),
                test_peer_device("peer", "peer-token"),
            ],
        );
        let active = registration("ExpoPushToken[active]", Some("app"));
        let candidates = vec![
            active.clone(),
            registration("ExpoPushToken[revoked]", Some("gone")),
            registration("ExpoPushToken[legacy]", None),
            registration("ExpoPushToken[peer]", Some("peer")),
        ];
        *state.push_tokens.lock().unwrap() = candidates.clone();
        assert_eq!(registered_recipients(&state).unwrap(), vec![active.clone()]);
        assert_eq!(
            admit_batch(&state, &candidates).unwrap(),
            vec![active.clone()]
        );
        state.push_tokens.lock().unwrap()[0].registration_id = Some("new-generation".into());
        assert!(admit_batch(&state, &[active]).unwrap().is_empty());
        state.devices.lock().unwrap().clear();
        assert!(registered_recipients(&state).unwrap().is_empty());
    }

    #[test]
    fn install_replacement_and_capacity_eviction_cut_off_old_pushes() {
        for replace_install in [false, true] {
            let mut old = test_device("old", "old-token");
            old.install_id = Some("install".into());
            let state = test_state("admin", vec![old]);
            let record = registration("ExpoPushToken[old]", Some("old"));
            state.push_tokens.lock().unwrap().push(record.clone());
            let mut new = test_device("new", "new-token");
            new.paired_unix_ms = 2_000;
            new.install_id = replace_install.then(|| "install".into());
            authority::enroll_device(&mut state.devices.lock().unwrap(), new, 1).unwrap();
            assert!(admit_batch(&state, &[record]).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn every_batch_rechecks_membership_without_holding_locks_over_send() {
        let state = test_state("admin", vec![test_device("app", "token")]);
        let candidates: Vec<_> = (0..101)
            .map(|n| registration(&format!("ExpoPushToken[{n}]"), Some("app")))
            .collect();
        *state.push_tokens.lock().unwrap() = candidates.clone();
        let mut calls = 0;
        let result = send_batches(&state, &candidates, |batch| {
            calls += 1;
            assert_eq!(batch.len(), 100);
            let mut devices = state.devices.try_lock().expect("no device lock over send");
            let _tokens = state
                .push_tokens
                .try_lock()
                .expect("no token lock over send");
            devices.clear();
            std::future::ready(Ok(json!({ "data": [{ "status": "ok" }] })))
        })
        .await
        .unwrap();
        assert_eq!(calls, 1, "second batch is suppressed after revoke");
        assert_eq!(result["data"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn next_batch_rechecks_registration_even_when_the_owner_is_still_paired() {
        let state = test_state("admin", vec![test_device("app", "token")]);
        let candidates: Vec<_> = (0..101)
            .map(|n| registration(&format!("ExpoPushToken[{n}]"), Some("app")))
            .collect();
        *state.push_tokens.lock().unwrap() = candidates.clone();
        let mut calls = 0;
        send_batches(&state, &candidates, |_| {
            calls += 1;
            state.push_tokens.lock().unwrap()[100].registration_id = Some("reregistered".into());
            std::future::ready(Ok(json!({ "data": [] })))
        })
        .await
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn admission_fails_closed_if_either_lock_is_poisoned() {
        for poison_devices in [false, true] {
            let state = test_state("admin", vec![test_device("app", "token")]);
            let record = registration("ExpoPushToken[active]", Some("app"));
            state.push_tokens.lock().unwrap().push(record.clone());
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if poison_devices {
                    let _guard = state.devices.lock().unwrap();
                    panic!("poison devices");
                } else {
                    let _guard = state.push_tokens.lock().unwrap();
                    panic!("poison tokens");
                }
            }));
            assert!(registered_recipients(&state).is_err());
            assert!(admit_batch(&state, &[record]).is_err());
        }
    }

    #[tokio::test]
    async fn stale_and_legacy_records_never_reach_the_sender() {
        let state = test_state("admin", vec![]);
        let candidates = vec![
            registration("ExpoPushToken[old]", Some("old")),
            registration("ExpoPushToken[legacy]", None),
        ];
        *state.push_tokens.lock().unwrap() = candidates.clone();
        let result = send_batches(&state, &candidates, |_| async {
            anyhow::bail!("no Expo request may be made")
        })
        .await
        .unwrap();
        assert_eq!(result, json!({ "data": [] }));
        // Drive both public routes without any eligible recipient: no traffic.
        let response = crate::connectivity::routes::send_test_notification(
            State(test_state("admin", vec![test_device("app", "token")])),
            bearer_headers("token"),
            Json(crate::connectivity::routes::SendPushNotificationBody {
                title: None,
                body: None,
                data: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.0["device_count"], 0);
        crate::terminal::routes::deliver_agent_notification(
            &state,
            AgentPushNotice {
                notice: AgentNotice::AgentCompleted,
                server_label: "test".into(),
                agent_name: None,
                data: Default::default(),
                choices: vec![],
                detail: None,
            },
        )
        .await;
    }

    #[test]
    fn all_push_paths_use_authoritative_admission() {
        // Prevent a future route/watcher from reverting to the raw HTTP sender.
        assert!(!include_str!("routes.rs").contains("send_expo_push_notifications"));
        assert!(!include_str!("../terminal/routes.rs").contains("send_expo_push_notifications"));
        assert_eq!(
            include_str!("../terminal/routes.rs")
                .matches("send_registered_push_notifications(")
                .count(),
            2
        );
    }
}
