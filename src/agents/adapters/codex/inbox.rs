//! Gateway-owned queue of prompts waiting for a Codex thread's turn to end.
//!
//! Codex has no inbox of its own, so the Gateway holds one per thread, in
//! memory. It does not survive a Gateway restart or a reconnect to Codex.
//! Items use the same JSON shape as OpenCode's `Session.Inbox.Info`, so the
//! App reads both agents' queues with one parser.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::agents::ports::agent::AgentError;

/// Per thread. A queue this long is a stuck session, not a backlog.
const MAX_ITEMS: usize = 64;
const MAX_THREADS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Queue,
    Steer,
}

impl Delivery {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

/// What the manager publishes for a thread's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InboxSignal {
    /// The queue changed; publish the whole list.
    Changed(String),
    /// A queued item was sent and the transport failed before Codex answered.
    /// It was removed and is not resent; the App must refetch.
    Uncertain(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Queued {
    pub id: String,
    pub created_ms: u64,
    pub delivery: Delivery,
    pub text: String,
    /// `[{uri, name, mime?}]`, as OpenCode lists a prompt's files.
    pub files: Vec<Value>,
    /// The prepared Codex `UserInput` list, validated when it was queued.
    pub input: Vec<Value>,
}

impl Queued {
    fn to_json(&self, thread: &str) -> Value {
        let mut payload = json!({ "text": self.text });
        if !self.files.is_empty() {
            payload["files"] = Value::Array(self.files.clone());
        }
        json!({
            "id": self.id,
            "sessionID": thread,
            "timeCreated": self.created_ms,
            "type": "user",
            "payload": payload,
            "delivery": self.delivery.as_str(),
        })
    }
}

#[derive(Default)]
pub(crate) struct Inbox {
    queues: HashMap<String, Vec<Queued>>,
}

impl Inbox {
    pub(crate) fn push(&mut self, thread: &str, item: Queued) -> Result<(), AgentError> {
        if self.queues.len() >= MAX_THREADS && !self.queues.contains_key(thread) {
            return Err(AgentError::NotAvailable(
                "Codex queued-prompt capacity exhausted".into(),
            ));
        }
        let queue = self.queues.entry(thread.into()).or_default();
        if queue.len() >= MAX_ITEMS {
            return Err(AgentError::InvalidRequest(format!(
                "Codex already has {MAX_ITEMS} prompts waiting; send or cancel one first"
            )));
        }
        queue.push(item);
        Ok(())
    }

    /// The item a normally completed turn hands to Codex next: the oldest one
    /// marked to send now (the App shows it as "Next"), else the oldest queued.
    pub(crate) fn next(&self, thread: &str) -> Option<&Queued> {
        let queue = self.queues.get(thread)?;
        queue
            .iter()
            .find(|item| item.delivery == Delivery::Steer)
            .or_else(|| queue.first())
    }

    pub(crate) fn get(&self, thread: &str, id: &str) -> Option<&Queued> {
        self.queues.get(thread)?.iter().find(|item| item.id == id)
    }

    pub(crate) fn remove(&mut self, thread: &str, id: &str) -> Option<Queued> {
        let queue = self.queues.get_mut(thread)?;
        let index = queue.iter().position(|item| item.id == id)?;
        let item = queue.remove(index);
        if queue.is_empty() {
            self.queues.remove(thread);
        }
        Some(item)
    }

    /// False when the item is not (or no longer) queued.
    pub(crate) fn set_delivery(&mut self, thread: &str, id: &str, delivery: Delivery) -> bool {
        let Some(item) = self
            .queues
            .get_mut(thread)
            .and_then(|queue| queue.iter_mut().find(|item| item.id == id))
        else {
            return false;
        };
        item.delivery = delivery;
        true
    }

    /// True when anything was dropped.
    pub(crate) fn clear(&mut self, thread: &str) -> bool {
        self.queues.remove(thread).is_some()
    }

    pub(crate) fn items(&self, thread: &str) -> Vec<Value> {
        self.queues
            .get(thread)
            .map(|queue| queue.iter().map(|item| item.to_json(thread)).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, delivery: Delivery) -> Queued {
        Queued {
            id: id.into(),
            created_ms: 1,
            delivery,
            text: id.into(),
            files: Vec::new(),
            input: vec![json!({"type":"text","text":id,"text_elements":[]})],
        }
    }

    #[test]
    fn next_delivers_send_now_items_before_queued_items_in_order() {
        let mut inbox = Inbox::default();
        inbox.push("t", item("first", Delivery::Queue)).unwrap();
        inbox.push("t", item("second", Delivery::Queue)).unwrap();
        inbox.push("t", item("steer", Delivery::Steer)).unwrap();
        let mut order = Vec::new();
        while let Some(next) = inbox.next("t").map(|item| item.id.clone()) {
            inbox.remove("t", &next).unwrap();
            order.push(next);
        }
        assert_eq!(order, ["steer", "first", "second"]);
        assert!(inbox.items("t").is_empty());
        assert!(!inbox.clear("t"), "an emptied queue leaves no entry");
    }

    #[test]
    fn cancel_and_delivery_changes_need_a_queued_item() {
        let mut inbox = Inbox::default();
        inbox.push("t", item("a", Delivery::Queue)).unwrap();
        assert!(inbox.set_delivery("t", "a", Delivery::Steer));
        assert_eq!(inbox.items("t")[0]["delivery"], "steer");
        assert!(!inbox.set_delivery("t", "missing", Delivery::Queue));
        assert!(!inbox.set_delivery("other", "a", Delivery::Queue));
        assert!(inbox.remove("other", "a").is_none());
        assert!(inbox.remove("t", "a").is_some());
        assert!(inbox.remove("t", "a").is_none(), "already gone");
    }

    #[test]
    fn a_full_queue_refuses_more_without_dropping_what_it_holds() {
        let mut inbox = Inbox::default();
        for n in 0..MAX_ITEMS {
            inbox
                .push("t", item(&n.to_string(), Delivery::Queue))
                .unwrap();
        }
        assert!(inbox.push("t", item("over", Delivery::Queue)).is_err());
        assert_eq!(inbox.items("t").len(), MAX_ITEMS);
        assert!(inbox.push("u", item("other", Delivery::Queue)).is_ok());
    }

    #[test]
    fn items_use_the_opencode_inbox_shape_with_files() {
        let mut inbox = Inbox::default();
        let mut queued = item("inbox_1", Delivery::Queue);
        queued.text = "Look at this".into();
        queued.created_ms = 1_789_000_000_000;
        queued.files = vec![json!({"uri":"file:///up/a.png","name":"a.png","mime":"image/png"})];
        inbox.push("thread-1", queued).unwrap();
        inbox
            .push("thread-1", item("bare", Delivery::Steer))
            .unwrap();
        let items = inbox.items("thread-1");
        assert_eq!(
            items[0],
            json!({
                "id": "inbox_1",
                "sessionID": "thread-1",
                "timeCreated": 1_789_000_000_000_u64,
                "type": "user",
                "payload": {
                    "text": "Look at this",
                    "files": [{"uri":"file:///up/a.png","name":"a.png","mime":"image/png"}]
                },
                "delivery": "queue"
            })
        );
        assert!(items[1]["payload"].get("files").is_none());
        assert_eq!(items[1]["delivery"], "steer");
    }
}
