//! Bounded replay buffer for tile-delta recovery.
//!
//! D-4d keeps recovery bounded by both time and bytes. The tile-delta
//! channel is intentionally unreliable/latest-wins; when the browser
//! notices a sequence gap it can ask for the missing recent updates.
//! If the gap has already fallen out of this buffer, the server must
//! send a fresh snapshot instead of trying to reconstruct unbounded
//! history.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const GAP_RINGBUFFER_DEPTH: Duration = Duration::from_millis(250);
pub const GAP_RINGBUFFER_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayFrame {
    pub epoch: u32,
    pub seq: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayDecision {
    Frames(Vec<ReplayFrame>),
    SnapshotRequired,
    NoGap,
}

#[derive(Clone, Debug)]
struct Entry {
    frame: ReplayFrame,
    inserted_at: Instant,
}

#[derive(Clone, Debug)]
pub struct TileUpdateReplayBuffer {
    depth: Duration,
    max_bytes: usize,
    total_bytes: usize,
    entries: VecDeque<Entry>,
}

impl TileUpdateReplayBuffer {
    pub fn new() -> Self {
        Self::with_limits(GAP_RINGBUFFER_DEPTH, GAP_RINGBUFFER_MAX_BYTES)
    }

    pub fn with_limits(depth: Duration, max_bytes: usize) -> Self {
        Self {
            depth,
            max_bytes: max_bytes.max(1),
            total_bytes: 0,
            entries: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn push(&mut self, epoch: u32, seq: u32, bytes: Vec<u8>, now: Instant) {
        self.prune_expired(now);
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        self.entries.push_back(Entry {
            frame: ReplayFrame { epoch, seq, bytes },
            inserted_at: now,
        });
        self.prune_to_size();
    }

    /// Return all buffered frames with `last_seen_seq < seq < expected_seq`.
    ///
    /// The browser sends this shape when it observes a jump from
    /// `last_seen_seq` to `expected_seq` on the same epoch. Missing
    /// frames are replayable only if the complete gap is still in the
    /// buffer; partial replay risks painting a stale tile state, so a
    /// partial hit returns [`ReplayDecision::SnapshotRequired`].
    pub fn replay_gap(
        &mut self,
        epoch: u32,
        last_seen_seq: u32,
        expected_seq: u32,
        now: Instant,
    ) -> ReplayDecision {
        self.prune_expired(now);
        if expected_seq <= last_seen_seq.saturating_add(1) {
            return ReplayDecision::NoGap;
        }

        let wanted_start = last_seen_seq.saturating_add(1);
        let wanted_end = expected_seq;
        let mut frames = Vec::new();
        for seq in wanted_start..wanted_end {
            let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.frame.epoch == epoch && entry.frame.seq == seq)
            else {
                return ReplayDecision::SnapshotRequired;
            };
            frames.push(entry.frame.clone());
        }

        if frames.is_empty() {
            ReplayDecision::NoGap
        } else {
            ReplayDecision::Frames(frames)
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        while self
            .entries
            .front()
            .is_some_and(|entry| now.saturating_duration_since(entry.inserted_at) > self.depth)
        {
            if let Some(entry) = self.entries.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(entry.frame.bytes.len());
            }
        }
    }

    fn prune_to_size(&mut self) {
        while self.total_bytes > self.max_bytes {
            let Some(entry) = self.entries.pop_front() else {
                self.total_bytes = 0;
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(entry.frame.bytes.len());
        }
    }
}

impl Default for TileUpdateReplayBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn payload(byte: u8, len: usize) -> Vec<u8> {
        vec![byte; len]
    }

    #[test]
    fn replay_gap_returns_complete_missing_range() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_secs(1), 1024);
        buf.push(7, 10, payload(10, 4), now);
        buf.push(7, 11, payload(11, 4), now);
        buf.push(7, 12, payload(12, 4), now);

        let out = buf.replay_gap(7, 9, 13, now);
        let ReplayDecision::Frames(frames) = out else {
            panic!("expected replay frames");
        };
        assert_eq!(
            frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert_eq!(frames[1].bytes, payload(11, 4));
    }

    #[test]
    fn partial_gap_requires_snapshot() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_secs(1), 1024);
        buf.push(3, 10, payload(10, 4), now);
        buf.push(3, 12, payload(12, 4), now);

        assert_eq!(
            buf.replay_gap(3, 9, 13, now),
            ReplayDecision::SnapshotRequired
        );
    }

    #[test]
    fn no_gap_is_noop() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_secs(1), 1024);
        assert_eq!(buf.replay_gap(1, 10, 11, now), ReplayDecision::NoGap);
        assert_eq!(buf.replay_gap(1, 10, 10, now), ReplayDecision::NoGap);
    }

    #[test]
    fn expired_entries_are_not_replayed() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_millis(100), 1024);
        buf.push(1, 1, payload(1, 4), now);
        assert_eq!(
            buf.replay_gap(1, 0, 2, now + Duration::from_millis(101)),
            ReplayDecision::SnapshotRequired
        );
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.total_bytes(), 0);
    }

    #[test]
    fn byte_cap_prunes_oldest_entries() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_secs(1), 10);
        buf.push(1, 1, payload(1, 6), now);
        buf.push(1, 2, payload(2, 6), now);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.total_bytes(), 6);
        assert_eq!(
            buf.replay_gap(1, 0, 3, now),
            ReplayDecision::SnapshotRequired
        );
        let ReplayDecision::Frames(frames) = buf.replay_gap(1, 1, 3, now) else {
            panic!("seq 2 should remain replayable");
        };
        assert_eq!(frames[0].seq, 2);
    }

    #[test]
    fn epoch_mismatch_requires_snapshot() {
        let now = t0();
        let mut buf = TileUpdateReplayBuffer::with_limits(Duration::from_secs(1), 1024);
        buf.push(2, 1, payload(1, 4), now);
        assert_eq!(
            buf.replay_gap(3, 0, 2, now),
            ReplayDecision::SnapshotRequired
        );
    }
}
