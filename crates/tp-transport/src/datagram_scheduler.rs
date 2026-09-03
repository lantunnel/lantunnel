use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tp_core::protocol::PackedMessage;

pub const DEFAULT_DATAGRAM_DRR_QUANTUM: usize = 1452;

#[derive(Clone, Debug)]
pub struct DatagramFrame {
    pub conn_id: String,
    pub packed: PackedMessage,
    pub bytes: usize,
    pub fragment_group: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct DatagramSchedulerConfig {
    pub per_association_packet_limit: usize,
    pub per_association_byte_limit: usize,
    pub global_packet_limit: usize,
    pub global_byte_limit: usize,
}

impl DatagramSchedulerConfig {
    pub fn for_test() -> Self {
        Self {
            per_association_packet_limit: 128,
            per_association_byte_limit: 1024 * 1024,
            global_packet_limit: 128,
            global_byte_limit: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatagramEnqueueOutcome {
    pub accepted: bool,
    pub accepted_packets: usize,
    pub per_association_evicted: usize,
    pub global_budget_evicted: usize,
}

impl DatagramEnqueueOutcome {
    fn merge(&mut self, other: QueueEviction) {
        self.per_association_evicted += other.packets;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatagramSchedulerSnapshot {
    pub associations: usize,
    pub total_packets: usize,
    pub total_bytes: usize,
}

#[derive(Default)]
struct QueueEviction {
    packets: usize,
    bytes: usize,
}

enum QueueItem {
    Single(DatagramFrame),
    FragmentGroup {
        frames: VecDeque<DatagramFrame>,
        bytes: usize,
    },
}

impl QueueItem {
    fn packets(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::FragmentGroup { frames, .. } => frames.len(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Single(frame) => frame.bytes,
            Self::FragmentGroup { bytes, .. } => *bytes,
        }
    }

    fn into_frames(self) -> VecDeque<DatagramFrame> {
        match self {
            Self::Single(frame) => VecDeque::from([frame]),
            Self::FragmentGroup { frames, .. } => frames,
        }
    }
}

struct AssociationQueue {
    items: VecDeque<QueueItem>,
    packets: usize,
    bytes: usize,
    deficit: usize,
    active: bool,
}

impl AssociationQueue {
    fn new() -> Self {
        Self {
            items: VecDeque::new(),
            packets: 0,
            bytes: 0,
            deficit: 0,
            active: false,
        }
    }

    fn push_item(&mut self, item: QueueItem) {
        self.packets += item.packets();
        self.bytes += item.bytes();
        self.items.push_back(item);
    }

    fn pop_oldest_item(&mut self) -> QueueEviction {
        let Some(item) = self.items.pop_front() else {
            return QueueEviction::default();
        };
        let evicted = QueueEviction {
            packets: item.packets(),
            bytes: item.bytes(),
        };
        self.packets = self.packets.saturating_sub(evicted.packets);
        self.bytes = self.bytes.saturating_sub(evicted.bytes);
        if self.items.is_empty() {
            self.deficit = 0;
        }
        evicted
    }

    fn is_empty(&self) -> bool {
        self.packets == 0
    }
}

struct State {
    queues: HashMap<String, AssociationQueue>,
    ready: VecDeque<String>,
    total_packets: usize,
    total_bytes: usize,
}

impl State {
    fn new() -> Self {
        Self {
            queues: HashMap::new(),
            ready: VecDeque::new(),
            total_packets: 0,
            total_bytes: 0,
        }
    }
}

struct Inner {
    state: Mutex<State>,
    config: DatagramSchedulerConfig,
    notify: Notify,
    sender_count: AtomicUsize,
    receiver_closed: AtomicBool,
}

pub struct DatagramSchedulerSender {
    inner: Arc<Inner>,
}

pub struct DatagramSchedulerReceiver {
    inner: Arc<Inner>,
    pending: VecDeque<DatagramFrame>,
}

pub fn datagram_scheduler_channel(
    config: DatagramSchedulerConfig,
) -> (DatagramSchedulerSender, DatagramSchedulerReceiver) {
    assert!(
        config.per_association_packet_limit > 0,
        "per-association packet limit must be non-zero"
    );
    assert!(
        config.per_association_byte_limit > 0,
        "per-association byte limit must be non-zero"
    );
    assert!(
        config.global_packet_limit > 0,
        "global packet limit must be non-zero"
    );
    assert!(
        config.global_byte_limit > 0,
        "global byte limit must be non-zero"
    );
    let inner = Arc::new(Inner {
        state: Mutex::new(State::new()),
        config,
        notify: Notify::new(),
        sender_count: AtomicUsize::new(1),
        receiver_closed: AtomicBool::new(false),
    });
    (
        DatagramSchedulerSender {
            inner: inner.clone(),
        },
        DatagramSchedulerReceiver {
            inner,
            pending: VecDeque::new(),
        },
    )
}

impl Clone for DatagramSchedulerSender {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for DatagramSchedulerSender {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.notify.notify_waiters();
        }
    }
}

impl DatagramSchedulerSender {
    pub fn is_closed(&self) -> bool {
        self.inner.receiver_closed.load(Ordering::Acquire)
    }

    pub fn enqueue(&self, frame: DatagramFrame) -> DatagramEnqueueOutcome {
        self.enqueue_group(vec![frame])
    }

    pub fn enqueue_group(&self, frames: Vec<DatagramFrame>) -> DatagramEnqueueOutcome {
        let mut outcome = DatagramEnqueueOutcome::default();
        if frames.is_empty() || self.is_closed() {
            return outcome;
        }

        let conn_id = frames[0].conn_id.clone();
        let fragment_group = frames[0].fragment_group;
        let packets = frames.len();
        let bytes = frames.iter().map(|f| f.bytes).sum::<usize>();
        if frames.iter().any(|f| f.conn_id != conn_id)
            || frames.iter().any(|f| f.fragment_group != fragment_group)
            || packets > self.inner.config.per_association_packet_limit
            || bytes > self.inner.config.per_association_byte_limit
            || packets > self.inner.config.global_packet_limit
            || bytes > self.inner.config.global_byte_limit
        {
            return outcome;
        }

        let item = if frames.len() == 1 && fragment_group.is_none() {
            QueueItem::Single(frames.into_iter().next().unwrap())
        } else {
            QueueItem::FragmentGroup {
                frames: frames.into(),
                bytes,
            }
        };

        {
            let mut state = self.inner.state.lock();
            if self.inner.receiver_closed.load(Ordering::Acquire) {
                return outcome;
            }

            loop {
                let evicted = {
                    let queue = state
                        .queues
                        .entry(conn_id.clone())
                        .or_insert_with(AssociationQueue::new);
                    if queue.packets + packets <= self.inner.config.per_association_packet_limit
                        && queue.bytes + bytes <= self.inner.config.per_association_byte_limit
                    {
                        break;
                    }
                    queue.pop_oldest_item()
                };
                if evicted.packets == 0 {
                    break;
                }
                state.total_packets = state.total_packets.saturating_sub(evicted.packets);
                state.total_bytes = state.total_bytes.saturating_sub(evicted.bytes);
                outcome.merge(evicted);
            }

            while state.total_packets + packets > self.inner.config.global_packet_limit
                || state.total_bytes + bytes > self.inner.config.global_byte_limit
            {
                let by_bytes = state.total_bytes + bytes > self.inner.config.global_byte_limit;
                let Some((noisy_conn_id, evicted)) = evict_noisiest(&mut state, by_bytes) else {
                    break;
                };
                outcome.global_budget_evicted += evicted.packets;
                let _ = noisy_conn_id;
            }

            let queue = state
                .queues
                .entry(conn_id.clone())
                .or_insert_with(AssociationQueue::new);
            queue.push_item(item);
            state.total_packets += packets;
            state.total_bytes += bytes;
            outcome.accepted = true;
            outcome.accepted_packets = packets;
            mark_ready(&mut state, &conn_id);
        }

        self.inner.notify.notify_one();
        outcome
    }

    pub fn remove_association(&self, conn_id: &str) -> usize {
        let mut state = self.inner.state.lock();
        let Some(mut queue) = state.queues.remove(conn_id) else {
            return 0;
        };
        let packets = queue.packets;
        state.total_packets = state.total_packets.saturating_sub(queue.packets);
        state.total_bytes = state.total_bytes.saturating_sub(queue.bytes);
        queue.items.clear();
        state.ready.retain(|ready| ready != conn_id);
        packets
    }

    pub fn snapshot(&self) -> DatagramSchedulerSnapshot {
        let state = self.inner.state.lock();
        DatagramSchedulerSnapshot {
            associations: state.queues.len(),
            total_packets: state.total_packets,
            total_bytes: state.total_bytes,
        }
    }
}

impl Drop for DatagramSchedulerReceiver {
    fn drop(&mut self) {
        self.inner.receiver_closed.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }
}

impl DatagramSchedulerReceiver {
    pub async fn recv_with_quantum(&mut self, quantum: usize) -> Option<DatagramFrame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        let mut batch = self.recv_batch_with_quantum(quantum).await?;
        let first = batch.pop_front()?;
        self.pending = batch;
        Some(first)
    }

    pub async fn recv_batch_with_quantum(
        &mut self,
        quantum: usize,
    ) -> Option<VecDeque<DatagramFrame>> {
        if !self.pending.is_empty() {
            return Some(std::mem::take(&mut self.pending));
        }
        let quantum = quantum.max(1);
        loop {
            let notified = self.inner.notify.notified();
            if let Some(frames) = self.try_pop_with_quantum(quantum) {
                return Some(frames);
            }
            if self.inner.sender_count.load(Ordering::Acquire) == 0 {
                return None;
            }
            notified.await;
        }
    }

    fn try_pop_with_quantum(&self, quantum: usize) -> Option<VecDeque<DatagramFrame>> {
        let mut state = self.inner.state.lock();
        let ready_len = state.ready.len();
        let max_head_bytes = state
            .queues
            .values()
            .filter_map(|queue| queue.items.front().map(QueueItem::bytes))
            .max()
            .unwrap_or(0);
        let visits_per_queue = max_head_bytes
            .saturating_add(quantum.saturating_sub(1))
            .checked_div(quantum)
            .unwrap_or(1)
            .max(1);
        let mut attempts = ready_len
            .saturating_mul(visits_per_queue)
            .saturating_add(ready_len)
            .max(1);
        while attempts > 0 {
            attempts -= 1;
            let conn_id = state.ready.pop_front()?;
            let Some(queue) = state.queues.get_mut(&conn_id) else {
                continue;
            };
            queue.active = false;
            if queue.is_empty() {
                state.queues.remove(&conn_id);
                continue;
            }
            queue.deficit = queue.deficit.saturating_add(quantum);
            let Some(item_bytes) = queue.items.front().map(QueueItem::bytes) else {
                state.queues.remove(&conn_id);
                continue;
            };
            if queue.deficit < item_bytes {
                mark_ready(&mut state, &conn_id);
                continue;
            }
            queue.deficit -= item_bytes;
            let item = queue.items.pop_front()?;
            let packets = item.packets();
            let bytes = item.bytes();
            let frames = item.into_frames();
            queue.packets = queue.packets.saturating_sub(packets);
            queue.bytes = queue.bytes.saturating_sub(bytes);
            state.total_packets = state.total_packets.saturating_sub(packets);
            state.total_bytes = state.total_bytes.saturating_sub(bytes);
            if state
                .queues
                .get(&conn_id)
                .is_some_and(|queue| !queue.is_empty())
            {
                mark_ready(&mut state, &conn_id);
            } else {
                state.queues.remove(&conn_id);
            }
            return Some(frames);
        }
        None
    }
}

fn mark_ready(state: &mut State, conn_id: &str) {
    let Some(queue) = state.queues.get_mut(conn_id) else {
        return;
    };
    if queue.active || queue.is_empty() {
        return;
    }
    queue.active = true;
    state.ready.push_back(conn_id.to_string());
}

fn evict_noisiest(state: &mut State, by_bytes: bool) -> Option<(String, QueueEviction)> {
    let conn_id = if by_bytes {
        state
            .queues
            .iter()
            .max_by_key(|(_, queue)| (queue.bytes, queue.packets))
            .map(|(conn_id, _)| conn_id.clone())?
    } else {
        state
            .queues
            .iter()
            .max_by_key(|(_, queue)| (queue.packets, queue.bytes))
            .map(|(conn_id, _)| conn_id.clone())?
    };
    let queue = state.queues.get_mut(&conn_id)?;
    let evicted = queue.pop_oldest_item();
    state.total_packets = state.total_packets.saturating_sub(evicted.packets);
    state.total_bytes = state.total_bytes.saturating_sub(evicted.bytes);
    if queue.is_empty() {
        state.queues.remove(&conn_id);
        state.ready.retain(|ready| ready != &conn_id);
    }
    Some((conn_id, evicted))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tp_core::protocol::{pack, BinaryMessage};

    use super::*;

    fn frame(conn_id: &str, payload: Bytes) -> DatagramFrame {
        let packed = pack(&BinaryMessage::UdpData {
            conn_id: conn_id.to_string(),
            payload,
        });
        DatagramFrame {
            conn_id: conn_id.to_string(),
            bytes: packed.total_len(),
            packed,
            fragment_group: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_association_drop_oldest_is_per_association() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 2,
            global_packet_limit: 8,
            ..DatagramSchedulerConfig::for_test()
        });

        assert!(
            tx.enqueue(frame("noisy", Bytes::from_static(b"n1")))
                .accepted
        );
        assert!(
            tx.enqueue(frame("quiet", Bytes::from_static(b"q1")))
                .accepted
        );
        assert!(
            tx.enqueue(frame("noisy", Bytes::from_static(b"n2")))
                .accepted
        );
        let outcome = tx.enqueue(frame("noisy", Bytes::from_static(b"n3")));

        assert!(outcome.accepted);
        assert_eq!(outcome.per_association_evicted, 1);
        let mut seen = Vec::new();
        while let Ok(Some(next)) = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            rx.recv_with_quantum(1452),
        )
        .await
        {
            let BinaryMessage::UdpData { conn_id, payload } =
                tp_core::protocol::unpack(&next.packed.to_bytes()).unwrap()
            else {
                panic!("expected udp data");
            };
            seen.push((conn_id, payload));
        }

        assert!(seen.contains(&("quiet".to_string(), Bytes::from_static(b"q1"))));
        assert!(!seen.contains(&("noisy".to_string(), Bytes::from_static(b"n1"))));
        assert!(seen.contains(&("noisy".to_string(), Bytes::from_static(b"n2"))));
        assert!(seen.contains(&("noisy".to_string(), Bytes::from_static(b"n3"))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_datagram_scheduler_round_robins_associations() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 8,
            global_packet_limit: 16,
            ..DatagramSchedulerConfig::for_test()
        });

        for n in 0..3 {
            assert!(tx.enqueue(frame("a", Bytes::from(vec![b'a' + n]))).accepted);
            assert!(tx.enqueue(frame("b", Bytes::from(vec![b'0' + n]))).accepted);
        }

        let mut conn_ids = Vec::new();
        for _ in 0..6 {
            conn_ids.push(rx.recv_with_quantum(1452).await.unwrap().conn_id);
        }

        assert_eq!(conn_ids, ["a", "b", "a", "b", "a", "b"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_datagram_scheduler_global_budget_is_bounded() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 8,
            global_packet_limit: 4,
            ..DatagramSchedulerConfig::for_test()
        });

        assert!(
            tx.enqueue(frame("noisy", Bytes::from_static(b"n1")))
                .accepted
        );
        assert!(
            tx.enqueue(frame("noisy", Bytes::from_static(b"n2")))
                .accepted
        );
        assert!(
            tx.enqueue(frame("noisy", Bytes::from_static(b"n3")))
                .accepted
        );
        assert!(
            tx.enqueue(frame("quiet", Bytes::from_static(b"q1")))
                .accepted
        );
        let outcome = tx.enqueue(frame("quiet", Bytes::from_static(b"q2")));

        assert!(outcome.accepted);
        assert_eq!(outcome.global_budget_evicted, 1);
        assert_eq!(tx.snapshot().total_packets, 4);

        let mut seen = Vec::new();
        while let Ok(Some(next)) = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            rx.recv_with_quantum(1452),
        )
        .await
        {
            let BinaryMessage::UdpData { conn_id, payload } =
                tp_core::protocol::unpack(&next.packed.to_bytes()).unwrap()
            else {
                panic!("expected udp data");
            };
            seen.push((conn_id, payload));
        }

        assert!(!seen.contains(&("noisy".to_string(), Bytes::from_static(b"n1"))));
        assert_eq!(seen.len(), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_datagram_scheduler_byte_budget_evicts_largest_association() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 8,
            per_association_byte_limit: 512,
            global_packet_limit: 16,
            global_byte_limit: 160,
        });

        for n in 0..3 {
            assert!(tx.enqueue(frame("tiny", Bytes::from(vec![n]))).accepted);
        }
        assert!(
            tx.enqueue(frame("large", Bytes::from(vec![0; 96])))
                .accepted
        );
        let outcome = tx.enqueue(frame("quiet", Bytes::from_static(b"q")));

        assert!(outcome.accepted);
        assert_eq!(outcome.global_budget_evicted, 1);
        assert_eq!(tx.snapshot().total_packets, 4);

        let mut seen = Vec::new();
        while let Ok(Some(next)) = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            rx.recv_with_quantum(1452),
        )
        .await
        {
            seen.push(next.conn_id);
        }

        assert!(
            !seen.contains(&"large".to_string()),
            "byte pressure must evict the byte-noisiest association first"
        );
        assert_eq!(
            seen.iter()
                .filter(|conn_id| conn_id.as_str() == "tiny")
                .count(),
            3
        );
        assert!(seen.contains(&"quiet".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_datagram_scheduler_global_pressure_keeps_new_fragment_group() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 8,
            global_packet_limit: 4,
            ..DatagramSchedulerConfig::for_test()
        });

        for conn_id in ["a", "b", "c"] {
            assert!(
                tx.enqueue(frame(conn_id, Bytes::from_static(b"x")))
                    .accepted
            );
        }
        let group = vec![
            DatagramFrame {
                fragment_group: Some(7),
                ..frame("new", Bytes::from_static(b"f1"))
            },
            DatagramFrame {
                fragment_group: Some(7),
                ..frame("new", Bytes::from_static(b"f2"))
            },
        ];
        let outcome = tx.enqueue_group(group);

        assert!(outcome.accepted);
        assert_eq!(outcome.accepted_packets, 2);
        assert_eq!(outcome.global_budget_evicted, 1);
        assert_eq!(tx.snapshot().total_packets, 4);

        let mut new_fragments = 0;
        while let Ok(Some(next)) = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            rx.recv_with_quantum(1452),
        )
        .await
        {
            if next.conn_id == "new" {
                assert_eq!(next.fragment_group, Some(7));
                new_fragments += 1;
            }
        }

        assert_eq!(new_fragments, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_datagram_scheduler_fragment_group_drains_atomically_after_multiple_rounds() {
        let (tx, mut rx) = datagram_scheduler_channel(DatagramSchedulerConfig {
            per_association_packet_limit: 16,
            global_packet_limit: 16,
            ..DatagramSchedulerConfig::for_test()
        });
        let frames = (0..6)
            .map(|n| DatagramFrame {
                fragment_group: Some(42),
                ..frame("frag", Bytes::from(vec![n; 32]))
            })
            .collect::<Vec<_>>();

        let outcome = tx.enqueue_group(frames);

        assert!(outcome.accepted);
        assert_eq!(outcome.accepted_packets, 6);
        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.recv_batch_with_quantum(64),
        )
        .await
        .expect("timed out waiting for fragment group")
        .expect("scheduler closed");
        assert_eq!(batch.len(), 6);
        assert!(batch
            .iter()
            .all(|frame| frame.conn_id == "frag" && frame.fragment_group == Some(42)));
    }
}
