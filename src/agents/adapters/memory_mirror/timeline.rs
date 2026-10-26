//! ID lookup and chronological lookup share one mutation boundary. Content is
//! stored once; the ordered index contains only the existing domain sort key.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Included, Unbounded};

use crate::agents::domain::TimelineItem;

type OrderKey = (String, u64, u8, String);

fn order_key(row: &TimelineItem) -> OrderKey {
    let (message, ordinal, kind, id) = row.sort_key();
    (message.into(), ordinal, kind, id.into())
}

#[derive(Default)]
pub(super) struct Timeline {
    rows: BTreeMap<String, TimelineItem>,
    order: BTreeSet<OrderKey>,
}

impl Timeline {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn get(&self, id: &str) -> Option<&TimelineItem> {
        self.rows.get(id)
    }

    pub fn contains_key(&self, id: &str) -> bool {
        self.rows.contains_key(id)
    }

    pub fn values(&self) -> impl Iterator<Item = &TimelineItem> {
        self.rows.values()
    }

    pub fn insert(&mut self, row: TimelineItem) {
        self.remove(&row.id);
        self.order.insert(order_key(&row));
        self.rows.insert(row.id.clone(), row);
    }

    pub fn remove(&mut self, id: &str) -> Option<TimelineItem> {
        let row = self.rows.remove(id)?;
        self.order.remove(&order_key(&row));
        Some(row)
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.order.clear();
    }

    pub fn update<R>(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut TimelineItem) -> R,
    ) -> Option<R> {
        let row = self.rows.get_mut(id)?;
        let before = order_key(row);
        let result = change(row);
        let after = order_key(row);
        debug_assert_eq!(row.id, id, "an update cannot change a row identity");
        if before != after {
            self.order.remove(&before);
            self.order.insert(after);
        }
        Some(result)
    }

    pub fn update_each(&mut self, mut change: impl FnMut(&mut TimelineItem)) {
        for row in self.rows.values_mut() {
            let before = order_key(row);
            change(row);
            let after = order_key(row);
            if before != after {
                self.order.remove(&before);
                self.order.insert(after);
            }
        }
    }

    pub fn trim(&mut self, limit: usize) {
        while self.rows.len() > limit {
            let Some(key) = self.order.pop_first() else {
                break;
            };
            self.rows.remove(&key.3);
        }
    }

    pub fn ordered(&self) -> impl DoubleEndedIterator<Item = &TimelineItem> {
        self.order.iter().filter_map(|key| self.rows.get(&key.3))
    }

    pub fn from(&self, boundary: &TimelineItem) -> impl Iterator<Item = &TimelineItem> {
        self.order
            .range((Included(order_key(boundary)), Unbounded))
            .filter_map(|key| self.rows.get(&key.3))
    }

    pub fn before(
        &self,
        boundary: &TimelineItem,
    ) -> impl DoubleEndedIterator<Item = &TimelineItem> {
        self.order
            .range((Unbounded, Excluded(order_key(boundary))))
            .filter_map(|key| self.rows.get(&key.3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineRole};

    fn row(n: usize) -> TimelineItem {
        TimelineItem {
            id: format!("id-{n:04}"),
            message_id: format!("message-{:04}", (n * 37) % 503),
            ordinal: (n % 5) as u64,
            role: TimelineRole::Assistant,
            part: AgentPart::Text {
                text: "value".into(),
            },
            seq: 0,
            updated_ms: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        }
    }

    fn assert_order(timeline: &Timeline) {
        let mut expected: Vec<_> = timeline.values().collect();
        expected.sort_by_key(|row| row.sort_key());
        assert_eq!(timeline.order.len(), expected.len());
        assert_eq!(timeline.ordered().collect::<Vec<_>>(), expected);
        for anchor in expected.iter().step_by(31) {
            assert_eq!(
                timeline.before(anchor).collect::<Vec<_>>(),
                expected
                    .iter()
                    .copied()
                    .filter(|row| row.sort_key() < anchor.sort_key())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                timeline.from(anchor).collect::<Vec<_>>(),
                expected
                    .iter()
                    .copied()
                    .filter(|row| row.sort_key() >= anchor.sort_key())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn index_tracks_replacements_reordering_removals_and_oldest_eviction() {
        let mut timeline = Timeline::default();
        for n in (0..503).rev() {
            timeline.insert(row(n));
        }
        assert_order(&timeline);
        for n in (0..503).step_by(7) {
            let mut replacement = row(n);
            replacement.message_id = format!("changed-{:04}", 503 - n);
            replacement.ordinal += 10;
            replacement.part = AgentPart::Reasoning {
                text: "reasoning".into(),
                duration_ms: None,
            };
            timeline.insert(replacement);
        }
        timeline.update_each(|row| row.message_id = format!("rebased-{}", row.message_id));
        timeline.update("id-0001", |row| {
            row.ordinal = 99;
        });
        for n in (0..503).step_by(11) {
            timeline.remove(&row(n).id);
        }
        assert_order(&timeline);
        let expected: Vec<_> = timeline
            .ordered()
            .rev()
            .take(80)
            .map(|row| row.id.clone())
            .collect();
        timeline.trim(80);
        assert_eq!(
            timeline
                .ordered()
                .rev()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_order(&timeline);
        timeline.clear();
        assert_eq!(timeline.len(), 0);
        assert_order(&timeline);
    }
}
