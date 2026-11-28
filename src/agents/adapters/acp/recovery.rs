//! Read-only evidence for retiring an owned process, never permission to replay
//! a prompt. Native history must extend the checkpoint from before this turn.
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub entries: usize,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct TurnEvidence {
    pub user_digest: String,
    pub assistant_digest: String,
}

pub struct NativeSnapshot {
    pub session_id: String,
    pub entries: Vec<Value>,
    pub streamed: Option<(Checkpoint, Checkpoint)>,
    pub last_user: Option<String>,
    pub last_assistant: Option<String>,
}

pub fn text_digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn entries_digest(entries: &[Value]) -> String {
    let mut digest = Sha256::new();
    for entry in entries {
        digest_entry(&mut digest, entry);
    }
    hex::encode(digest.finalize())
}

fn digest_entry(digest: &mut Sha256, entry: &Value) {
    // Value's object keys are ordered without the preserve_order feature.
    // Length framing prevents two different entry sequences colliding by
    // concatenation before the cryptographic digest is applied.
    let bytes = serde_json::to_vec(entry).expect("JSON Value is serializable");
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

/// Only the full checkpoint and its immediately preceding prefix are needed
/// to prove a single newly completed turn.
#[derive(Default)]
pub struct RecoveryEntries {
    count: usize,
    digest: Sha256,
    previous: Sha256,
}

impl RecoveryEntries {
    pub fn push(&mut self, entry: Value) {
        self.previous = self.digest.clone();
        digest_entry(&mut self.digest, &entry);
        self.count += 1;
    }
    pub fn finish(self) -> (Checkpoint, Checkpoint) {
        (
            Checkpoint {
                entries: self.count,
                digest: hex::encode(self.digest.finalize()),
            },
            Checkpoint {
                entries: self.count.saturating_sub(1),
                digest: hex::encode(self.previous.finalize()),
            },
        )
    }
}

impl NativeSnapshot {
    pub fn empty(session_id: &str) -> Self {
        Self {
            session_id: session_id.into(),
            entries: Vec::new(),
            streamed: None,
            last_user: None,
            last_assistant: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.streamed {
            Some((full, _)) => full.entries == 0,
            None => self.entries.is_empty(),
        }
    }

    pub fn checkpoint(&self) -> Checkpoint {
        if let Some((full, _)) = &self.streamed {
            return full.clone();
        }
        Checkpoint {
            entries: self.entries.len(),
            digest: entries_digest(&self.entries),
        }
    }

    pub fn proves_turn(&self, id: &str, before: &Checkpoint, expected: &TurnEvidence) -> bool {
        self.session_id == id
            && match &self.streamed {
                Some((full, prefix)) => {
                    full.entries.checked_sub(1) == Some(before.entries) && prefix == before
                }
                None => {
                    self.entries.len().checked_sub(1) == Some(before.entries)
                        && entries_digest(&self.entries[..before.entries]) == before.digest
                }
            }
            && self
                .last_user
                .as_deref()
                .is_some_and(|text| text_digest(text) == expected.user_digest)
            && self.last_assistant.as_deref().is_some_and(|text| {
                !text.is_empty() && text_digest(text) == expected.assistant_digest
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn latest_turn_proof_requires_same_identity_prior_prefix_and_complete_current_text() {
        let mut native = NativeSnapshot::empty("same-id");
        native.entries.push(json!({"old":"context"}));
        let before = native.checkpoint();
        let expected = TurnEvidence {
            user_digest: text_digest("question"),
            assistant_digest: text_digest("answer"),
        };
        native.last_user = Some("question".into());
        native.last_assistant = Some("answer".into());
        assert!(
            !native.proves_turn("same-id", &before, &expected),
            "an old identical answer is not the new turn"
        );
        native
            .entries
            .push(json!({"user":"question", "assistant":"answer", "execution":{"tool_steps":[]}}));
        assert!(native.proves_turn("same-id", &before, &expected));
        assert!(!native.proves_turn("other-id", &before, &expected));
        native.entries[0] = json!({"old":"changed context"});
        assert!(!native.proves_turn("same-id", &before, &expected));
    }
}
