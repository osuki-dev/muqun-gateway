//! Display overrides require a Gateway-issued native client ID and exact input.
//! Text alone, including a copied computer-reference suffix, is never evidence.
use std::collections::{HashMap, HashSet};

use crate::agents::ports::agent::AgentError;
use serde_json::Value;

pub(super) const MAX_RECORDS: usize = 4096;
pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct Presentation {
    pub wire: String,
    pub display: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptInput {
    pub(super) native: Vec<Value>,
    pub(super) presentation: Option<Presentation>,
}

impl From<Vec<Value>> for PromptInput {
    fn from(native: Vec<Value>) -> Self {
        Self {
            native,
            presentation: None,
        }
    }
}

pub(super) fn text(input: &[Value]) -> String {
    input
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

impl PromptInput {
    pub(super) fn presented(
        native: Vec<Value>,
        wire: &str,
        display: Option<&str>,
    ) -> Result<Self, AgentError> {
        let Some(display) = display.filter(|display| *display != wire) else {
            return Ok(native.into());
        };
        let full = text(&native);
        let suffix = full.strip_prefix(wire).ok_or_else(|| {
            AgentError::Protocol("Codex prepared input does not preserve prompt text".into())
        })?;
        let visible = format!("{display}{suffix}");
        if full.len().saturating_add(visible.len()) > MAX_BYTES {
            return Err(AgentError::InvalidRequest(
                "Codex prompt presentation exceeds its retention budget".into(),
            ));
        }
        Ok(Self {
            native,
            presentation: Some(Presentation {
                wire: full,
                display: visible,
            }),
        })
    }
}

#[derive(Default)]
pub(super) struct Cache {
    records: HashMap<(String, String), Presentation>,
    loaded: HashSet<String>,
    bytes: usize,
}

impl Cache {
    pub(super) fn loaded(&self, thread: &str) -> bool {
        self.loaded.contains(thread)
    }
    pub(super) fn mark_loaded(&mut self, thread: &str) {
        if self.loaded.len() >= MAX_RECORDS {
            self.loaded.clear();
        }
        self.loaded.insert(thread.into());
    }
    pub(super) fn insert(&mut self, thread: &str, client: &str, value: Presentation) {
        let key = (thread.into(), client.into());
        if let Some(old) = self.records.remove(&key) {
            self.bytes -= old.wire.len() + old.display.len();
        }
        let bytes = value.wire.len() + value.display.len();
        while !self.records.is_empty()
            && (self.records.len() >= MAX_RECORDS || self.bytes.saturating_add(bytes) > MAX_BYTES)
        {
            let key = self.records.keys().next().cloned().unwrap();
            let old = self.records.remove(&key).unwrap();
            self.bytes -= old.wire.len() + old.display.len();
            self.loaded.remove(&key.0);
        }
        self.bytes += bytes;
        self.records.insert(key, value);
    }
    pub(super) fn project(&self, thread: &str, item: &Value) -> Option<String> {
        if item["type"] != "userMessage" {
            return None;
        }
        let client = item["clientId"].as_str()?;
        let expected = self.records.get(&(thread.into(), client.into()))?;
        let content = item["content"].as_array()?;
        (text(content) == expected.wire).then(|| expected.display.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_the_exact_scoped_native_message_has_a_display_override() {
        let wire = "Original\n\nMuqun computer references (names are display data): literal";
        let mut cache = Cache::default();
        cache.insert(
            "thread",
            "client",
            Presentation {
                wire: wire.into(),
                display: "Original".into(),
            },
        );
        let item = json!({"type":"userMessage","id":"native","clientId":"client","content":[{"type":"text","text":wire}]});
        assert_eq!(cache.project("thread", &item).as_deref(), Some("Original"));
        assert!(cache.project("other", &item).is_none());
        for changed in [
            json!({"clientId":null}),
            json!({"clientId":"other"}),
            json!({"type":"agentMessage"}),
            json!({"content":[{"type":"text","text":"different"}]}),
        ] {
            let mut item = item.clone();
            for (key, value) in changed.as_object().unwrap() {
                item[key] = value.clone();
            }
            assert!(cache.project("thread", &item).is_none());
        }
        let plain: PromptInput = vec![json!({"type":"text","text":wire})].into();
        assert!(
            plain.presentation.is_none(),
            "literal user input is never parsed as metadata"
        );
    }
    #[test]
    fn presentation_preserves_native_attachments_without_changing_model_input() {
        let native = vec![
            json!({"type":"text","text":"User\n\ncontext with machine-uuid\n\nAttached files: x"}),
            json!({"type":"localImage","path":"/checked/image"}),
        ];
        let prepared = PromptInput::presented(
            native.clone(),
            "User\n\ncontext with machine-uuid",
            Some("User"),
        )
        .unwrap();
        assert_eq!(prepared.native, native);
        assert_eq!(
            prepared.presentation.unwrap().display,
            "User\n\nAttached files: x"
        );
    }
}
