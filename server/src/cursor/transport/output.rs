//! Buffers, replays, broadcasts, and atomically closes downstream output.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use tokio::sync::mpsc;

const MAX_REPLAY_BYTES: usize = 64 * 1024 * 1024;
const MAX_REPLAY_FRAMES: usize = 512;
const SUBSCRIBER_CAPACITY: usize = 1024;

#[derive(Default)]
pub struct OutputHub {
    state: parking_lot::Mutex<OutputState>,
}

struct OutputSubscriber {
    sender: mpsc::Sender<Bytes>,
    overflowed: Arc<AtomicBool>,
}

pub struct OutputReceiver {
    receiver: mpsc::Receiver<Bytes>,
    overflowed: Arc<AtomicBool>,
}

impl OutputReceiver {
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }
}

impl Deref for OutputReceiver {
    type Target = mpsc::Receiver<Bytes>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl DerefMut for OutputReceiver {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}

struct OutputState {
    history: Vec<Bytes>,
    history_bytes: usize,
    replayable: bool,
    subscribers: Vec<OutputSubscriber>,
    closed: bool,
}

impl Default for OutputState {
    fn default() -> Self {
        Self {
            history: Vec::new(),
            history_bytes: 0,
            replayable: true,
            subscribers: Vec::new(),
            closed: false,
        }
    }
}

pub(super) struct EmitStatus {
    pub accepted: bool,
    pub last_subscriber_evicted: bool,
}

impl OutputHub {
    pub fn emit(&self, frame: Bytes) -> bool {
        self.emit_status(frame).accepted
    }

    pub(super) fn emit_status(&self, frame: Bytes) -> EmitStatus {
        let mut state = self.state.lock();
        if state.closed {
            return EmitStatus {
                accepted: false,
                last_subscriber_evicted: false,
            };
        }
        if state.replayable {
            let next_bytes = state.history_bytes.saturating_add(frame.len());
            if next_bytes <= MAX_REPLAY_BYTES && state.history.len() < MAX_REPLAY_FRAMES {
                state.history_bytes = next_bytes;
                state.history.push(frame.clone());
            } else {
                // Active streams continue, but a stream too large to retain cannot be
                // replayed safely from the middle to a later subscriber.
                state.history.clear();
                state.history_bytes = 0;
                state.replayable = false;
            }
        }
        // Slow subscribers are disconnected when their bounded queue fills. The
        // handle detaches a run immediately when the last subscriber is evicted;
        // the marker lets the HTTP stream report an explicit terminal error.
        let had_subscribers = !state.subscribers.is_empty();
        state.subscribers.retain(|subscriber| {
            if subscriber.sender.try_send(frame.clone()).is_ok() {
                return true;
            }
            subscriber.overflowed.store(true, Ordering::Release);
            false
        });
        EmitStatus {
            accepted: true,
            last_subscriber_evicted: had_subscribers && state.subscribers.is_empty(),
        }
    }

    /// 订阅输出流:历史未超重放容量时返回带历史回放的接收器;
    /// 超出容量(或回放失败)返回 None,调用方必须显式失败,不能把空流当成正常结束。
    pub fn subscribe(&self) -> Option<OutputReceiver> {
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let mut state = self.state.lock();
        if !state.replayable {
            return None;
        }
        for frame in &state.history {
            if sender.try_send(frame.clone()).is_err() {
                return None;
            }
        }
        if !state.closed {
            state.subscribers.push(OutputSubscriber {
                sender,
                overflowed: overflowed.clone(),
            });
        }
        Some(OutputReceiver {
            receiver,
            overflowed,
        })
    }

    pub fn has_subscribers(&self) -> bool {
        self.state
            .lock()
            .subscribers
            .iter()
            .any(|subscriber| !subscriber.sender.is_closed())
    }

    pub fn close(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.subscribers.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnects_a_slow_subscriber_at_bounded_capacity() {
        let hub = OutputHub::default();
        let receiver = hub.subscribe().unwrap();

        for _ in 0..=SUBSCRIBER_CAPACITY {
            assert!(hub.emit(Bytes::from_static(b"frame")));
        }

        assert!(!hub.has_subscribers());
        assert!(receiver.overflowed());
    }

    #[test]
    fn oversized_history_refuses_new_subscribers() {
        let hub = OutputHub::default();
        for _ in 0..=MAX_REPLAY_FRAMES {
            assert!(hub.emit(Bytes::from_static(b"frame")));
        }

        assert!(hub.subscribe().is_none());
        assert!(!hub.has_subscribers());
    }

    #[test]
    fn normal_history_replays_in_order() {
        let hub = OutputHub::default();
        hub.emit(Bytes::from_static(b"one"));
        hub.emit(Bytes::from_static(b"two"));

        let mut receiver = hub.subscribe().unwrap();
        assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"one"));
        assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"two"));
        assert!(hub.has_subscribers());
    }
}
