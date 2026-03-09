//! Deterministic MPSC channel for simulation tasks.
//!
//! [`SimChannel`] is a bounded-less, FIFO channel that integrates with the
//! cooperative scheduler by returning the [`TaskId`] of a blocked receiver
//! when a message is sent, rather than directly calling into the scheduler.
//! This keeps the channel fully decoupled from scheduling mechanics.

use std::collections::VecDeque;

use crate::task::TaskId;

/// A deterministic, unbounded MPSC channel.
///
/// The channel itself does **not** hold a reference to the scheduler.  When
/// [`send`](SimChannel::send) delivers a message while a task is waiting,
/// it returns that task's [`TaskId`] so the caller (or harness) can call
/// [`DeterministicScheduler::unpark`](crate::DeterministicScheduler::unpark).
#[derive(Debug)]
pub struct SimChannel<T> {
    buffer: VecDeque<T>,
    waiting_receivers: Vec<TaskId>,
}

impl<T> SimChannel<T> {
    /// Create an empty channel.
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            waiting_receivers: Vec::new(),
        }
    }

    /// Send a message into the channel.
    ///
    /// If one or more receivers are blocked waiting for a message, the first
    /// one is dequeued and its [`TaskId`] is returned so the caller can
    /// unpark it via the scheduler.
    pub fn send(&mut self, value: T) -> Option<TaskId> {
        self.buffer.push_back(value);
        // Wake the longest-waiting receiver, if any.
        if !self.waiting_receivers.is_empty() {
            Some(self.waiting_receivers.remove(0))
        } else {
            None
        }
    }

    /// Try to receive a message without blocking.
    ///
    /// Returns `Some(value)` if the buffer is non-empty, `None` otherwise.
    pub fn try_recv(&mut self) -> Option<T> {
        self.buffer.pop_front()
    }

    /// Register `task_id` as waiting for the next message.
    ///
    /// The task should then return [`TaskAction::Park(BlockReason::Channel)`]
    /// to tell the scheduler it is blocked.
    pub fn register_receiver(&mut self, task_id: TaskId) {
        self.waiting_receivers.push(task_id);
    }

    /// `true` if no messages are buffered.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Number of buffered messages.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Number of tasks currently waiting for messages.
    pub fn waiting_count(&self) -> usize {
        self.waiting_receivers.len()
    }
}

impl<T> Default for SimChannel<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_recv_fifo() {
        let mut ch: SimChannel<u64> = SimChannel::new();
        assert!(ch.is_empty());

        ch.send(1);
        ch.send(2);
        ch.send(3);
        assert_eq!(ch.len(), 3);

        assert_eq!(ch.try_recv(), Some(1));
        assert_eq!(ch.try_recv(), Some(2));
        assert_eq!(ch.try_recv(), Some(3));
        assert_eq!(ch.try_recv(), None);
    }

    #[test]
    fn send_unparks_waiting_receiver() {
        let mut ch: SimChannel<&str> = SimChannel::new();
        ch.register_receiver(TaskId(10));
        ch.register_receiver(TaskId(20));

        // First send wakes the longest-waiting receiver.
        let woken = ch.send("hello");
        assert_eq!(woken, Some(TaskId(10)));

        // Second send wakes the next.
        let woken = ch.send("world");
        assert_eq!(woken, Some(TaskId(20)));

        // No more waiters.
        let woken = ch.send("!");
        assert_eq!(woken, None);
    }

    #[test]
    fn try_recv_empty() {
        let mut ch: SimChannel<i32> = SimChannel::new();
        assert_eq!(ch.try_recv(), None);
    }
}
