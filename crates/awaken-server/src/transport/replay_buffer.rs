//! Ring buffer for SSE event replay on client reconnection.

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Thread-safe ring buffer that stores recent SSE frames with sequence IDs.
///
/// When a client reconnects with `Last-Event-ID`, frames after that ID
/// can be replayed to catch up. New subscribers can also receive live
/// frames as they are pushed.
pub struct EventReplayBuffer {
    frames: Mutex<VecDeque<(u64, Bytes)>>,
    next_seq: AtomicU64,
    capacity: usize,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Bytes>>>,
}

impl EventReplayBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(capacity)),
            next_seq: AtomicU64::new(1),
            capacity,
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Allocate a sequence ID, format the JSON as a complete SSE frame
    /// (`id: {seq}\ndata: {json}\n\n`), store it, notify subscribers,
    /// and return `(seq, frame)`.
    pub fn push_json(&self, json: &str) -> (u64, Bytes) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let frame = Bytes::from(format!("id: {seq}\ndata: {json}\n\n"));
        {
            let mut frames = self.frames.lock();
            frames.push_back((seq, frame.clone()));
            while frames.len() > self.capacity {
                frames.pop_front();
            }
        }
        // Notify live subscribers, removing any whose receiver was dropped.
        {
            let mut subs = self.subscribers.lock();
            subs.retain(|tx| tx.send(frame.clone()).is_ok());
        }
        (seq, frame)
    }

    /// Replay all stored frames after `last_seen_seq`.
    /// Returns empty vec if `last_seen_seq` is ahead of buffer or buffer is empty.
    pub fn replay_after(&self, last_seen_seq: u64) -> Vec<Bytes> {
        self.frames
            .lock()
            .iter()
            .filter(|(seq, _)| *seq > last_seen_seq)
            .map(|(_, frame)| frame.clone())
            .collect()
    }

    /// Subscribe to receive new frames as they are pushed.
    /// Only frames pushed *after* this call are delivered.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Bytes> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().push(tx);
        rx
    }

    /// Atomically replay buffered frames after `last_seen_seq` AND subscribe
    /// for new frames, under a single lock hold. This guarantees no duplicates
    /// and no gaps between the replayed set and the live stream.
    pub fn subscribe_after(
        &self,
        last_seen_seq: u64,
    ) -> (Vec<Bytes>, mpsc::UnboundedReceiver<Bytes>) {
        let (tx, rx) = mpsc::unbounded_channel();
        // Hold both locks to prevent push_json from inserting between
        // replay snapshot and subscriber registration.
        // Lock order: frames first, then subscribers (same as push_json).
        let frames = self.frames.lock();
        let mut subs = self.subscribers.lock();
        let replayed: Vec<Bytes> = frames
            .iter()
            .filter(|(seq, _)| *seq > last_seen_seq)
            .map(|(_, frame)| frame.clone())
            .collect();
        subs.push(tx);
        (replayed, rx)
    }

    /// Close all live subscribers by dropping their senders.
    /// Call this when the run completes so reconnected clients get a clean EOF.
    pub fn close_subscribers(&self) {
        self.subscribers.lock().clear();
    }

    /// Current highest sequence number (0 if no frames pushed yet).
    pub fn current_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// Number of frames currently in the buffer.
    pub fn len(&self) -> usize {
        self.frames.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_replay() {
        let buf = EventReplayBuffer::new(10);
        buf.push_json(r#"{"a":1}"#);
        buf.push_json(r#"{"a":2}"#);
        buf.push_json(r#"{"a":3}"#);

        let replayed = buf.replay_after(1);
        assert_eq!(replayed.len(), 2);
        assert!(String::from_utf8_lossy(&replayed[0]).contains("id: 2\n"));
        assert!(String::from_utf8_lossy(&replayed[1]).contains("id: 3\n"));
    }

    #[test]
    fn replay_all_from_zero() {
        let buf = EventReplayBuffer::new(10);
        buf.push_json(r#"{"x":1}"#);
        buf.push_json(r#"{"x":2}"#);

        let replayed = buf.replay_after(0);
        assert_eq!(replayed.len(), 2);
    }

    #[test]
    fn replay_empty_buffer() {
        let buf = EventReplayBuffer::new(10);
        let replayed = buf.replay_after(0);
        assert!(replayed.is_empty());
    }

    #[test]
    fn replay_future_seq() {
        let buf = EventReplayBuffer::new(10);
        buf.push_json(r#"{"a":1}"#);
        buf.push_json(r#"{"a":2}"#);
        buf.push_json(r#"{"a":3}"#);

        let replayed = buf.replay_after(999);
        assert!(replayed.is_empty());
    }

    #[test]
    fn ring_buffer_eviction() {
        let buf = EventReplayBuffer::new(3);
        for i in 1..=5 {
            buf.push_json(&format!(r#"{{"n":{i}}}"#));
        }
        assert_eq!(buf.len(), 3);
        // Oldest frames (seq 1, 2) should be evicted; remaining are seq 3, 4, 5.
        let replayed = buf.replay_after(0);
        assert_eq!(replayed.len(), 3);
        assert!(String::from_utf8_lossy(&replayed[0]).contains("id: 3\n"));
        assert!(String::from_utf8_lossy(&replayed[2]).contains("id: 5\n"));
    }

    #[test]
    fn current_seq_starts_at_zero() {
        let buf = EventReplayBuffer::new(10);
        assert_eq!(buf.current_seq(), 0);
    }

    #[test]
    fn current_seq_increments() {
        let buf = EventReplayBuffer::new(10);
        buf.push_json("{}");
        assert_eq!(buf.current_seq(), 1);
        buf.push_json("{}");
        assert_eq!(buf.current_seq(), 2);
    }

    #[tokio::test]
    async fn subscriber_receives_new_frames() {
        let buf = EventReplayBuffer::new(10);
        let mut rx = buf.subscribe();
        buf.push_json(r#"{"event":"hello"}"#);

        let frame = rx.try_recv().unwrap();
        let s = String::from_utf8_lossy(&frame);
        assert!(s.contains("id: 1\n"));
        assert!(s.contains(r#"{"event":"hello"}"#));
    }

    #[tokio::test]
    async fn subscriber_gets_only_new_frames() {
        let buf = EventReplayBuffer::new(10);
        buf.push_json(r#"{"n":1}"#);
        buf.push_json(r#"{"n":2}"#);

        let mut rx = buf.subscribe();
        buf.push_json(r#"{"n":3}"#);

        let frame = rx.try_recv().unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("id: 3\n"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dead_subscriber_cleaned_up() {
        let buf = EventReplayBuffer::new(10);
        let rx = buf.subscribe();
        drop(rx);
        // Should not panic; dead subscriber is removed.
        buf.push_json("{}");
        assert_eq!(buf.current_seq(), 1);
    }

    #[tokio::test]
    async fn concurrent_push() {
        use std::sync::Arc;

        let buf = Arc::new(EventReplayBuffer::new(2000));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let buf = Arc::clone(&buf);
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    buf.push_json("{}");
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(buf.current_seq(), 1000);
    }

    #[tokio::test]
    async fn subscribe_after_no_duplicates_no_gaps() {
        let buf = EventReplayBuffer::new(100);
        buf.push_json(r#"{"n":1}"#);
        buf.push_json(r#"{"n":2}"#);
        buf.push_json(r#"{"n":3}"#);

        // Subscribe after seq 1 — should get frames 2, 3 replayed
        let (replayed, mut live_rx) = buf.subscribe_after(1);
        assert_eq!(replayed.len(), 2);
        assert!(String::from_utf8_lossy(&replayed[0]).contains("id: 2\n"));
        assert!(String::from_utf8_lossy(&replayed[1]).contains("id: 3\n"));

        // Push a new frame — live_rx should receive it
        buf.push_json(r#"{"n":4}"#);
        let frame = live_rx.try_recv().unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("id: 4\n"));

        // No extra frames (no duplicates)
        assert!(live_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscribe_after_zero_replays_all() {
        let buf = EventReplayBuffer::new(100);
        buf.push_json("{}");
        buf.push_json("{}");

        let (replayed, _rx) = buf.subscribe_after(0);
        assert_eq!(replayed.len(), 2);
    }

    #[tokio::test]
    async fn close_subscribers_terminates_live_stream() {
        let buf = EventReplayBuffer::new(100);
        let mut rx = buf.subscribe();

        buf.push_json("{}");
        assert!(rx.try_recv().is_ok());

        // Close all subscribers
        buf.close_subscribers();

        // Pushing after close should not panic
        buf.push_json("{}");

        // The old receiver should see channel closed (recv returns None)
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn close_subscribers_then_new_subscribe() {
        let buf = EventReplayBuffer::new(100);
        let mut rx1 = buf.subscribe();
        buf.close_subscribers();
        assert!(rx1.recv().await.is_none());

        // New subscriber after close should still work
        let mut rx2 = buf.subscribe();
        buf.push_json("{}");
        assert!(rx2.try_recv().is_ok());
    }
}
