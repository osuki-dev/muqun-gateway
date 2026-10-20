//! The bounded, ordered hand-off from a transport pump to the manager.
//!
//! Codex streams about one text delta per token, and a socket read can carry
//! hundreds of them at once. Rather than spend one slot per token, a delta
//! that continues the delta still waiting at the tail of the queue (same
//! method, thread, turn, item and part) is appended to it. Merging only ever
//! touches the newest unconsumed event, so content and order are exactly the
//! uncoalesced stream's; anything in between (an approval, an item boundary,
//! another item's delta) ends the run. A consumer that keeps up never sees a
//! merged event. Events that cannot merge still count against the bound, and
//! a full queue is reported to the caller, never skipped.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::Notify;

use super::CodexEvent;

/// Appended text the manager handles as a pure concatenation.
const COALESCED_METHODS: [&str; 5] = [
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
];
/// One merged event stays an ordinary small row update.
pub(super) const MAX_COALESCED_DELTA_BYTES: usize = 64 * 1024;

struct Queue {
    inner: Mutex<Inner>,
    ready: Notify,
    capacity: usize,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<CodexEvent>,
    sender_closed: bool,
    receiver_closed: bool,
}

/// The pump's half. Dropping it ends the stream once queued events are read.
pub(crate) struct EventSender {
    queue: Arc<Queue>,
}

/// The consumer's half, read in order by one task.
pub struct CodexEvents {
    queue: Arc<Queue>,
}

pub(super) fn channel(capacity: usize) -> (EventSender, CodexEvents) {
    let queue = Arc::new(Queue {
        inner: Mutex::new(Inner::default()),
        ready: Notify::new(),
        capacity,
    });
    (
        EventSender {
            queue: queue.clone(),
        },
        CodexEvents { queue },
    )
}

impl Queue {
    fn locked(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// When the new delta continues `tail`'s text (the same method and every
/// parameter equal except the string `delta`, within the merged size bound),
/// the tail's text to append it to.
fn continued<'a>(tail: &'a mut CodexEvent, method: &str, params: &Value) -> Option<&'a mut String> {
    let CodexEvent::Notification {
        method: tail_method,
        params: Value::Object(tail_params),
    } = tail
    else {
        return None;
    };
    let params = params.as_object()?;
    let new = params.get("delta")?.as_str()?;
    let same_part = *tail_method == method
        && COALESCED_METHODS.contains(&method)
        && tail_params.len() == params.len()
        && params
            .iter()
            .all(|(key, value)| key == "delta" || tail_params.get(key) == Some(value));
    match tail_params.get_mut("delta") {
        Some(Value::String(old))
            if same_part && old.len() + new.len() <= MAX_COALESCED_DELTA_BYTES =>
        {
            Some(old)
        }
        _ => None,
    }
}

impl EventSender {
    pub(super) fn try_send(&self, event: CodexEvent) -> Result<(), TrySendError<CodexEvent>> {
        let mut inner = self.queue.locked();
        if inner.receiver_closed {
            return Err(TrySendError::Closed(event));
        }
        if let CodexEvent::Notification { method, params } = &event {
            if let Some(text) = inner
                .events
                .back_mut()
                .and_then(|tail| continued(tail, method, params))
            {
                // `continued` proved `delta` is a string.
                text.push_str(params["delta"].as_str().unwrap_or_default());
                return Ok(());
            }
        }
        if inner.events.len() >= self.queue.capacity {
            return Err(TrySendError::Full(event));
        }
        inner.events.push_back(event);
        drop(inner);
        self.queue.ready.notify_one();
        Ok(())
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        self.queue.locked().sender_closed = true;
        self.queue.ready.notify_one();
    }
}

impl CodexEvents {
    /// The next event in order; `None` once the pump is gone and the queue is
    /// empty. Cancel-safe: an event is removed only when it is returned.
    pub async fn recv(&mut self) -> Option<CodexEvent> {
        loop {
            match self.try_recv() {
                Ok(event) => return Some(event),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => self.queue.ready.notified().await,
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<CodexEvent, TryRecvError> {
        let mut inner = self.queue.locked();
        match inner.events.pop_front() {
            Some(event) => Ok(event),
            None if inner.sender_closed => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }
}

impl Drop for CodexEvents {
    fn drop(&mut self) {
        let mut inner = self.queue.locked();
        inner.receiver_closed = true;
        inner.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(method: &str, item: &str, part: u64, text: &str) -> CodexEvent {
        let mut params = json!({"threadId":"t","turnId":"u","itemId":item,"delta":text});
        if method.starts_with("item/reasoning/") {
            params["summaryIndex"] = json!(part);
        }
        CodexEvent::Notification {
            method: method.into(),
            params,
        }
    }

    fn drain(events: &mut CodexEvents) -> Vec<(String, String, String)> {
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event {
                CodexEvent::Notification { method, params } => (
                    method,
                    params["itemId"].as_str().unwrap_or("").to_owned(),
                    params["delta"].as_str().unwrap_or("").to_owned(),
                ),
                CodexEvent::Request { method, .. } => (method, String::new(), String::new()),
                _ => ("other".into(), String::new(), String::new()),
            })
            .collect()
    }

    const TEXT: &str = "item/agentMessage/delta";
    const SUMMARY: &str = "item/reasoning/summaryTextDelta";

    #[test]
    fn backlogged_runs_merge_in_order_and_every_boundary_splits_them() {
        let (sender, mut events) = channel(16);
        for event in [
            delta(TEXT, "a", 0, "Hel"),
            delta(TEXT, "a", 0, "lo"),
            delta(TEXT, "b", 0, " other item"),
            delta(TEXT, "a", 0, " again"),
            delta(SUMMARY, "a", 0, "think"),
            delta(SUMMARY, "a", 0, "ing"),
            delta(SUMMARY, "a", 1, "next part"),
            CodexEvent::Request {
                id: json!(7),
                method: "item/commandExecution/requestApproval".into(),
                params: json!({"threadId":"t"}),
            },
            delta(TEXT, "a", 0, " after"),
            delta(TEXT, "a", 0, " approval"),
            CodexEvent::Notification {
                method: "item/completed".into(),
                params: json!({"threadId":"t","turnId":"u","item":{"id":"a"}}),
            },
            delta(TEXT, "a", 0, "late"),
        ] {
            sender.try_send(event).unwrap();
        }
        let s = |m: &str, i: &str, d: &str| (m.to_owned(), i.to_owned(), d.to_owned());
        assert_eq!(
            drain(&mut events),
            [
                s(TEXT, "a", "Hello"),
                s(TEXT, "b", " other item"),
                s(TEXT, "a", " again"),
                s(SUMMARY, "a", "thinking"),
                s(SUMMARY, "a", "next part"),
                s("item/commandExecution/requestApproval", "", ""),
                s(TEXT, "a", " after approval"),
                s("item/completed", "", ""),
                s(TEXT, "a", "late"),
            ]
        );
    }

    #[test]
    fn a_consumed_delta_is_never_extended_and_merges_are_bounded() {
        let (sender, mut events) = channel(4);
        sender.try_send(delta(TEXT, "a", 0, "one")).unwrap();
        assert_eq!(drain(&mut events).len(), 1);
        sender.try_send(delta(TEXT, "a", 0, "two")).unwrap();
        let chunk = "x".repeat(MAX_COALESCED_DELTA_BYTES / 2);
        for _ in 0..3 {
            sender.try_send(delta(TEXT, "a", 0, &chunk)).unwrap();
        }
        let sizes: Vec<_> = drain(&mut events).into_iter().map(|e| e.2.len()).collect();
        assert_eq!(sizes, [3 + chunk.len(), 2 * chunk.len()]);
    }

    #[test]
    fn unmergeable_events_fill_the_bound_and_closure_is_reported() {
        let (sender, mut events) = channel(2);
        let other = || CodexEvent::Notification {
            method: "synthetic/unknown".into(),
            params: json!({"delta":"x"}),
        };
        sender.try_send(other()).unwrap();
        sender.try_send(other()).unwrap();
        assert!(matches!(
            sender.try_send(other()),
            Err(TrySendError::Full(_))
        ));
        // A delta cannot merge into a different method's tail either.
        assert!(matches!(
            sender.try_send(delta(TEXT, "a", 0, "x")),
            Err(TrySendError::Full(_))
        ));
        drop(sender);
        assert_eq!(drain(&mut events).len(), 2);
        assert!(matches!(events.try_recv(), Err(TryRecvError::Disconnected)));
        let (sender, events) = channel(2);
        drop(events);
        assert!(matches!(
            sender.try_send(other()),
            Err(TrySendError::Closed(_))
        ));
    }

    #[tokio::test]
    async fn recv_wakes_for_new_events_and_ends_after_the_sender() {
        let (sender, mut events) = channel(2);
        let reader = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(event) = events.recv().await {
                seen.push(event);
            }
            seen.len()
        });
        tokio::task::yield_now().await;
        sender.try_send(delta(TEXT, "a", 0, "x")).unwrap();
        tokio::task::yield_now().await;
        sender.try_send(CodexEvent::Disconnected).unwrap();
        drop(sender);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), reader).await,
            Ok(Ok(1..=2))
        ));
    }
}
