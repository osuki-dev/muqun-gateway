//! Sampling boundaries are not pane incarnations. Only immutable historical
//! prefixes cross a boundary; the screen never does.

use super::scrollback::Row;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const MAX_SEGMENTS: usize = 64;
pub(crate) const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(i64::MAX as u128) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BoundaryReason {
    ObservationStart,
    GatewayRestart,
    CapturePolicyChange,
    Resize,
    EditorChange,
    CaptureReset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentPosition {
    pub segment_id: String,
    pub started_at_ms: i64,
    pub sealed_at_ms: Option<i64>,
    pub boundary_reason: BoundaryReason,
    /// Segment-local row coordinates; trims advance base, never the ID.
    pub base: u64,
    pub len: usize,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub position: SegmentPosition,
    pub changed_at_ms: i64,
    pub rows: Vec<Row>,
}

impl Segment {
    pub(crate) fn new(reason: BoundaryReason) -> Self {
        Self {
            position: SegmentPosition {
                segment_id: Uuid::new_v4().to_string(),
                started_at_ms: now_ms(),
                sealed_at_ms: None,
                boundary_reason: reason,
                base: 0,
                len: 0,
                revision: 0,
            },
            changed_at_ms: now_ms(),
            rows: Vec::new(),
        }
    }

    pub(crate) fn bytes(&self) -> usize {
        self.rows.iter().map(|r| r.len()).sum::<usize>()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + std::mem::size_of::<Self>()
            + self.position.segment_id.capacity()
    }
}

#[derive(Debug)]
pub(crate) struct SegmentCollection {
    /// Rotates only on identity loss/deletion/eviction, not sampling changes.
    pub epoch: Uuid,
    pub sealed: Vec<Segment>,
    pub active: Segment,
    pub revision: u64,
}

impl Default for SegmentCollection {
    fn default() -> Self {
        Self {
            epoch: Uuid::new_v4(),
            sealed: Vec::new(),
            active: Segment::new(BoundaryReason::ObservationStart),
            revision: 0,
        }
    }
}

impl SegmentCollection {
    pub(crate) fn bytes(&self) -> usize {
        self.sealed.iter().map(Segment::bytes).sum::<usize>()
            + self.sealed.capacity() * std::mem::size_of::<Segment>()
            + self.active.rows.capacity() * std::mem::size_of::<Row>()
            + self.active.position.segment_id.capacity()
            + std::mem::size_of::<Self>()
    }
}
