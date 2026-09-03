use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

struct Inner<T> {
    queue: Mutex<VecDeque<T>>,
    cap: usize,
    notify: Notify,
    sender_count: AtomicUsize,
    receiver_closed: AtomicBool,
}

pub struct DropOldestSender<T> {
    inner: Arc<Inner<T>>,
}

pub struct DropOldestReceiver<T> {
    inner: Arc<Inner<T>>,
}

pub fn drop_oldest_channel<T>(cap: usize) -> (DropOldestSender<T>, DropOldestReceiver<T>) {
    assert!(cap > 0, "drop-oldest channel capacity must be non-zero");
    let inner = Arc::new(Inner {
        queue: Mutex::new(VecDeque::with_capacity(cap)),
        cap,
        notify: Notify::new(),
        sender_count: AtomicUsize::new(1),
        receiver_closed: AtomicBool::new(false),
    });
    (
        DropOldestSender {
            inner: inner.clone(),
        },
        DropOldestReceiver { inner },
    )
}

impl<T> Clone for DropOldestSender<T> {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for DropOldestSender<T> {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.notify.notify_waiters();
        }
    }
}

impl<T> DropOldestSender<T> {
    pub fn is_closed(&self) -> bool {
        self.inner.receiver_closed.load(Ordering::Acquire)
    }

    /// Push a value, evicting the oldest queued value if the bounded ring is
    /// full. Returns `Ok(true)` when an older value was dropped.
    pub fn send_drop_oldest(&self, value: T) -> Result<bool, T> {
        if self.is_closed() {
            return Err(value);
        }

        let mut queue = self.inner.queue.lock();
        if self.inner.receiver_closed.load(Ordering::Acquire) {
            return Err(value);
        }
        let dropped = if queue.len() == self.inner.cap {
            queue.pop_front();
            true
        } else {
            false
        };
        queue.push_back(value);
        drop(queue);
        self.inner.notify.notify_one();
        Ok(dropped)
    }
}

impl<T> Drop for DropOldestReceiver<T> {
    fn drop(&mut self) {
        self.inner.receiver_closed.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }
}

impl<T> DropOldestReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(value) = self.inner.queue.lock().pop_front() {
                return Some(value);
            }
            if self.inner.sender_count.load(Ordering::Acquire) == 0 {
                return None;
            }
            notified.await;
        }
    }

    pub fn try_recv(&mut self) -> Result<T, tokio::sync::mpsc::error::TryRecvError> {
        if let Some(value) = self.inner.queue.lock().pop_front() {
            return Ok(value);
        }
        if self.inner.sender_count.load(Ordering::Acquire) == 0 {
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        } else {
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        }
    }
}
