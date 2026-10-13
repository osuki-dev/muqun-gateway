//! Async assistant questions use ordinary user input, not server-request replies.
//! Wire identities/envelopes follow pinned TUI async_questions/state.rs and
//! context-fragments/answered_question.rs. Recovery requires exact live-turn ownership.
use super::CodexClient;
use crate::agents::domain::*;
use crate::agents::ports::agent::AgentError;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
struct Pending {
    ownership: Option<u64>,
    form: FormRequest,
    turn: String,
    message: String,
    question: String,
    answered: bool,
    answering: bool,
}
#[derive(Default)]
pub(super) struct Registry {
    pending: HashMap<String, Pending>,
    // Replies can arrive before a historical question is recovered. Remember
    // exact identities until turn cleanup, bounded independently of form count.
    answered: HashSet<(String, String, String)>,
}
impl Registry {
    pub(super) fn insert(
        &mut self,
        client: &CodexClient,
        thread: &str,
        turn: &str,
        item: &Value,
    ) -> Vec<FormRequest> {
        if item["type"] != "agentMessage" {
            return vec![];
        }
        let (Some(message), Some(questions)) = (item["id"].as_str(), item["questions"].as_array())
        else {
            return vec![];
        };
        if questions.len() > 32 || message.len() > 256 {
            return vec![];
        }
        let ownership = client.subscription_revision(thread);
        self.retain_subscription(thread, ownership);
        let mut forms = vec![];
        for (index, question) in questions.iter().enumerate() {
            let Some(title) = question["title"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 4096)
            else {
                continue;
            };
            let labels = question["options"].as_array();
            if labels.is_some_and(|options| {
                options.len() > 32
                    || options
                        .iter()
                        .any(|v| v.as_str().is_none_or(|s| s.len() > 512))
            }) {
                continue;
            }
            let question_id = json!(["request_user_input_async", message, index]).to_string();
            let Some(key) =
                client.request_id(&json!(
                    json!([thread, turn, &question_id, ownership]).to_string()
                ))
            else {
                continue;
            };
            let id = format!("async:{key}");
            if self.pending.contains_key(&id)
                || self.pending.len() >= 32
                || self.answered.len() >= 1024
                || [question_id.as_str(), message].iter().any(|question| {
                    self.answered
                        .contains(&(thread.into(), turn.into(), (*question).into()))
                })
            {
                continue;
            }
            let form = FormRequest {
                nonblocking: true,
                id: id.clone(),
                asid: AgentSessionId(thread.into()),
                title: "Codex question".into(),
                fields: vec![FormField::String {
                    key: "answer".into(),
                    title: title.into(),
                    description: None,
                    required: true,
                    when: vec![],
                    placeholder: None,
                    default: None,
                    options: labels
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|label| FormOption {
                            value: label.into(),
                            label: label.into(),
                            description: None,
                        })
                        .collect(),
                    format: None,
                    min_length: Some(1),
                    max_length: Some(32_768),
                    pattern: None,
                    custom: true,
                }],
            };
            self.pending.insert(
                id,
                Pending {
                    ownership,
                    form: form.clone(),
                    turn: turn.into(),
                    message: message.into(),
                    question: question_id,
                    answered: false,
                    answering: false,
                },
            );
            forms.push(form);
        }
        forms
    }
    pub(super) fn retain_subscription(&mut self, thread: &str, ownership: Option<u64>) {
        self.pending
            .retain(|_, p| p.form.asid.0 != thread || p.ownership == ownership);
    }

    pub(super) fn pending(&self, thread: &str, turn: Option<&str>) -> Vec<FormRequest> {
        self.pending
            .values()
            .filter(|p| p.form.asid.0 == thread && Some(p.turn.as_str()) == turn && !p.answered)
            .map(|p| p.form.clone())
            .collect()
    }
    pub(super) fn finish_turn(
        &mut self,
        thread: &str,
        turn: &str,
        ownership: Option<u64>,
        owned: bool,
    ) -> HashMap<String, FormResolutionReason> {
        let reasons = self
            .pending
            .iter()
            .filter_map(|(id, p)| {
                if !owned || p.form.asid.0 != thread || p.turn != turn || p.ownership != ownership {
                    return None;
                }
                let reason = if p.answered {
                    FormResolutionReason::Answered
                } else if !p.answering {
                    FormResolutionReason::Expired
                } else {
                    return None;
                }; // An in-flight answer has an unknown outcome.
                Some((id.clone(), reason))
            })
            .collect();
        self.clear(thread);
        reasons
    }

    pub(super) fn clear(&mut self, thread: &str) {
        self.pending.retain(|_, p| p.form.asid.0 != thread);
        self.answered.retain(|(owner, _, _)| owner != thread);
    }
    pub(super) fn claim(
        &mut self,
        thread: &str,
        turn: &str,
        id: &str,
        answers: &Value,
    ) -> Result<String, AgentError> {
        let p = self
            .pending
            .get_mut(id)
            .filter(|p| p.form.asid.0 == thread && p.turn == turn && !p.answered && !p.answering)
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "Codex asynchronous question expired or was already answered".into(),
                )
            })?;
        let answer = answers
            .as_object()
            .filter(|a| a.len() == 1)
            .and_then(|a| a.get("answer"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 32_768)
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "Codex question requires one nonempty text answer".into(),
                )
            })?;
        let FormField::String { title, .. } = &p.form.fields[0] else {
            unreachable!()
        };
        let end = title.floor_char_boundary(title.len().min(512));
        let title = title[..end].replace(['\n', '\r'], " ");
        let body = json!([{"questionItemId":p.question,"question":title,"answer":answer}]);
        p.answering = true;
        Ok(format!(
            "<send_user_message_question_reply>\n{body}\n</send_user_message_question_reply>"
        ))
    }
    pub(super) fn accept(&mut self, id: &str) {
        if let Some(p) = self.pending.get_mut(id) {
            p.answered = true;
            p.answering = false;
        }
    }
    pub(super) fn rejected(&mut self, id: &str) {
        if let Some(p) = self.pending.get_mut(id) {
            p.answering = false;
        }
    }
    pub(super) fn observe_reply(&mut self, thread: &str, turn: &str, input: &Value) -> Vec<String> {
        let Some(content) = input.as_array() else {
            return vec![];
        };
        let mut content = content
            .iter()
            .filter(|v| !matches!(v["type"].as_str(), Some("skill" | "mention")));
        let Some(text) = content
            .next()
            .filter(|v| v["type"] == "text")
            .and_then(|v| v["text"].as_str())
        else {
            return vec![];
        };
        if content.next().is_some() {
            return vec![];
        }
        let Some(replies) = parse_reply(text) else {
            return vec![];
        };
        for reply in &replies {
            if self.answered.len() < 1024 && reply.question_item_id.len() <= 2048 {
                self.answered
                    .insert((thread.into(), turn.into(), reply.question_item_id.clone()));
            }
        }
        let mut resolved = vec![];
        for (id, p) in &mut self.pending {
            if p.form.asid.0 == thread
                && p.turn == turn
                && !p.answered
                && replies
                    .iter()
                    .any(|r| r.question_item_id == p.question || r.question_item_id == p.message)
            {
                p.answered = true;
                resolved.push(id.clone());
            }
        }
        resolved
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    question_item_id: String,
    question: String,
    answer: String,
}
fn parse_reply(text: &str) -> Option<Vec<Reply>> {
    let mut text = text.trim();
    if text.starts_with("# Context from my IDE setup:\n") {
        text = text.rsplit_once("\n## My request for Codex:\n")?.1.trim();
    }
    let body = text
        .strip_prefix("<send_user_message_question_reply>")?
        .strip_suffix("</send_user_message_question_reply>")?;
    let value: Value = serde_json::from_str(body).ok()?;
    let replies: Vec<Reply> = if value.is_array() {
        serde_json::from_value(value).ok()?
    } else {
        vec![serde_json::from_value(value).ok()?]
    };
    (!replies.is_empty()).then_some(replies)
}
pub(super) fn display_reply(text: &str) -> Option<String> {
    Some(
        parse_reply(text)?
            .iter()
            .map(|r| format!("> {}\n\n{}", r.question, r.answer))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::tests::Fixture;
    #[tokio::test]
    async fn per_question_identity_pending_until_acceptance_and_replay_fences() {
        let mut fixture = Fixture::new(|_| panic!("pure registry must not write RPC")).await;
        fixture.ready().await;
        let mut registry = Registry::default();
        let item = json!({"type":"agentMessage","id":"message","questions":[{"title":"First?","options":["Yes","No"]},{"title":"Second?","options":null}]});
        let forms = registry.insert(&fixture.client, "thread", "turn", &item);
        assert_eq!(forms.len(), 2);
        assert!(forms.iter().all(|f| f.nonblocking));
        assert!(registry
            .claim("other", "turn", &forms[0].id, &json!({"answer":"Yes"}))
            .is_err());
        assert!(registry
            .claim(
                "thread",
                "next-turn",
                &forms[0].id,
                &json!({"answer":"Yes"})
            )
            .is_err());
        assert!(registry
            .claim("thread", "turn", &forms[0].id, &json!({"answer":""}))
            .is_err());
        let text = registry
            .claim(
                "thread",
                "turn",
                &forms[0].id,
                &json!({"answer":"Custom answer"}),
            )
            .unwrap();
        let replies = parse_reply(&text).unwrap();
        assert_eq!(
            replies[0].question_item_id,
            json!(["request_user_input_async", "message", 0]).to_string()
        );
        assert_eq!(replies[0].question, "First?");
        assert_eq!(replies[0].answer, "Custom answer");
        assert_eq!(
            registry.pending("thread", Some("turn")).len(),
            2,
            "in-flight answer remains visible"
        );
        assert!(registry
            .claim(
                "thread",
                "turn",
                &forms[0].id,
                &json!({"answer":"duplicate"})
            )
            .is_err());
        registry.accept(&forms[0].id);
        assert_eq!(registry.pending("thread", Some("turn")).len(), 1);
        assert!(registry
            .insert(&fixture.client, "thread", "turn", &item)
            .is_empty());
        let desktop = format!(
            "<send_user_message_question_reply>{}</send_user_message_question_reply>",
            json!({"questionItemId":json!(["request_user_input_async","message",1]).to_string(),"question":"Second?","answer":"Other client"})
        );
        assert!(registry
            .observe_reply("other", "turn", &json!([{"type":"text","text":desktop}]))
            .is_empty());
        assert_eq!(
            registry.observe_reply("thread", "turn", &json!([{"type":"text","text":desktop}])),
            vec![forms[1].id.clone()]
        );
        assert!(registry.pending("thread", Some("turn")).is_empty());
        assert!(parse_reply(&format!("Quoted: {desktop}")).is_none());
    }
    #[tokio::test]
    async fn recovered_answer_tombstones_are_turn_scoped_and_handles_keep_connection_identity() {
        let mut first = Fixture::new(|_| panic!("registry must not send RPC")).await;
        let mut second = Fixture::new(|_| panic!("registry must not send RPC")).await;
        first.ready().await;
        second.ready().await;
        let item = json!({"type":"agentMessage","id":"message","questions":[{"title":"Pick?"}]});
        let reply = json!([{"type":"text","text":format!("<send_user_message_question_reply>{}</send_user_message_question_reply>",json!({"questionItemId":"message","question":"Pick?","answer":"A"}))}]);
        let mut registry = Registry::default();
        registry.observe_reply("thread", "old-turn", &reply);
        let first_form = registry
            .insert(&first.client, "thread", "turn", &item)
            .remove(0);
        let mut restored = Registry::default();
        restored.observe_reply("thread", "turn", &reply);
        assert!(
            restored
                .insert(&second.client, "thread", "turn", &item)
                .is_empty(),
            "whole-message TUI answer is also authoritative before hydration"
        );
        restored.clear("thread");
        let second_form = restored
            .insert(&second.client, "thread", "turn", &item)
            .remove(0);
        assert_ne!(first_form.id, second_form.id);
        assert!(restored
            .claim("thread", "turn", &first_form.id, &json!({"answer":"stale"}))
            .is_err());
        assert!(restored
            .claim(
                "thread",
                "turn",
                &second_form.id,
                &json!({"answer":"current"})
            )
            .is_ok());
    }

    #[tokio::test]
    async fn form_resolution_reason_requires_known_unanswered_turn_and_ownership() {
        let mut fixture = Fixture::new(|_| panic!("registry must not send RPC")).await;
        fixture.ready().await;
        for (turn, ownership, owned) in [
            ("turn", None, true),
            ("other", None, true),
            ("turn", Some(9), true),
            ("turn", None, false),
        ] {
            let mut registry = Registry::default();
            let forms = registry.insert(&fixture.client, "thread", "turn", &json!({"type":"agentMessage","id":"q","questions":[{"title":"Pending?"},{"title":"In flight?"},{"title":"Accepted?"}]}));
            registry
                .claim("thread", "turn", &forms[1].id, &json!({"answer":"sending"}))
                .unwrap();
            registry.accept(&forms[2].id);
            let reasons = registry.finish_turn("thread", turn, ownership, owned);
            if turn == "turn" && ownership.is_none() && owned {
                assert_eq!(
                    reasons.get(&forms[0].id),
                    Some(&FormResolutionReason::Expired)
                );
                assert_eq!(
                    reasons.get(&forms[2].id),
                    Some(&FormResolutionReason::Answered)
                );
                assert!(
                    !reasons.contains_key(&forms[1].id),
                    "an in-flight answer must never be restored as expired"
                );
            } else {
                assert!(reasons.is_empty());
            }
            assert!(registry.pending("thread", Some("turn")).is_empty());
        }
    }

    #[test]
    fn transcript_preserves_questions_hooks_and_readable_reply_without_actionable_forms() {
        use crate::agents::adapters::codex::mapper;
        let rows = mapper::item(
            "thread",
            "turn",
            &json!({"type":"agentMessage","id":"message","text":"I can continue.","questions":[{"title":"Pick?","options":["A","B"]}]}),
            0,
        );
        assert!(matches!(&rows[0].part,AgentPart::Text{text} if text.contains("Pick?\n- A\n- B")));
        assert!(!rows
            .iter()
            .any(|row| matches!(row.part, AgentPart::Form { .. })));
        let rows = mapper::item(
            "thread",
            "turn",
            &json!({"type":"hookPrompt","id":"hook","fragments":[{"text":"Check formatting","hookRunId":"hook-run"}]}),
            0,
        );
        assert!(matches!(&rows[0].part,AgentPart::Synthetic{text,..} if text=="Check formatting"));
    }
}
