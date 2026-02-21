use crate::message::{ExitReason, SystemMessage};
use crate::pid::Pid;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

const DEFAULT_MAILBOX_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub enum MailboxMessage {
    Data(Vec<u8>),
    Tagged { ref_id: u64, payload: Vec<u8> },
    Exit { from: Pid, reason: ExitReason },
    Down { from: Pid, ref_id: u64, reason: ExitReason },
}

#[derive(Debug)]
pub enum SendError {
    NoProcess,
    MailboxFull,
}

struct Inner {
    queue: VecDeque<MailboxMessage>,
    data_count: usize,
    capacity: usize,
    closed: bool,
}

pub struct Mailbox {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailbox").finish_non_exhaustive()
    }
}

impl Mailbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                data_count: 0,
                capacity,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_MAILBOX_CAPACITY)
    }

    /// Push a data message. Bounded by capacity.
    pub async fn push_data(&self, msg: Vec<u8>) -> Result<(), SendError> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(SendError::NoProcess);
        }
        if inner.data_count >= inner.capacity {
            return Err(SendError::MailboxFull);
        }
        inner.data_count += 1;
        inner.queue.push_back(MailboxMessage::Data(msg));
        drop(inner);
        self.notify.notify_one();
        Ok(())
    }

    /// Push a tagged message. Bounded by capacity.
    pub async fn push_tagged(&self, ref_id: u64, payload: Vec<u8>) -> Result<(), SendError> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(SendError::NoProcess);
        }
        if inner.data_count >= inner.capacity {
            return Err(SendError::MailboxFull);
        }
        inner.data_count += 1;
        inner.queue.push_back(MailboxMessage::Tagged { ref_id, payload });
        drop(inner);
        self.notify.notify_one();
        Ok(())
    }

    /// Push a system message (Exit/Down). Unbounded, inserted at front.
    pub async fn push_system(&self, msg: SystemMessage) {
        let mailbox_msg = match msg {
            SystemMessage::Exit { from, reason } => MailboxMessage::Exit { from, reason },
            SystemMessage::Down { from, monitor_ref, reason } => {
                MailboxMessage::Down { from, ref_id: monitor_ref, reason }
            }
        };
        let mut inner = self.inner.lock().await;
        inner.queue.push_front(mailbox_msg);
        drop(inner);
        self.notify.notify_one();
    }

    /// Receive the next message from the front of the queue.
    /// Blocks until a message arrives or timeout expires.
    pub async fn recv(&self, timeout: Option<Duration>) -> Option<MailboxMessage> {
        loop {
            {
                let mut inner = self.inner.lock().await;
                if let Some(msg) = inner.queue.pop_front() {
                    if matches!(msg, MailboxMessage::Data(_) | MailboxMessage::Tagged { .. }) {
                        inner.data_count -= 1;
                    }
                    return Some(msg);
                }
                if inner.closed {
                    return None;
                }
            }

            match timeout {
                Some(dur) => {
                    if tokio::time::timeout(dur, self.notify.notified()).await.is_err() {
                        return None;
                    }
                }
                None => {
                    self.notify.notified().await;
                }
            }
        }
    }

    /// Receive a message matching a specific ref_id (Tagged or Down with matching ref).
    /// Non-matching messages stay in the queue.
    pub async fn recv_ref(&self, ref_id: u64, timeout: Option<Duration>) -> Option<MailboxMessage> {
        loop {
            {
                let mut inner = self.inner.lock().await;
                // Scan for matching message
                if let Some(pos) = inner.queue.iter().position(|msg| match msg {
                    MailboxMessage::Tagged { ref_id: r, .. } => *r == ref_id,
                    MailboxMessage::Down { ref_id: r, .. } => *r == ref_id,
                    _ => false,
                }) {
                    let msg = inner.queue.remove(pos).unwrap();
                    if matches!(msg, MailboxMessage::Tagged { .. }) {
                        inner.data_count -= 1;
                    }
                    return Some(msg);
                }
                if inner.closed {
                    return None;
                }
            }

            match timeout {
                Some(dur) => {
                    if tokio::time::timeout(dur, self.notify.notified()).await.is_err() {
                        return None;
                    }
                }
                None => {
                    self.notify.notified().await;
                }
            }
        }
    }

    /// Close the mailbox, waking any blocked recv.
    pub async fn close(&self) {
        let mut inner = self.inner.lock().await;
        inner.closed = true;
        drop(inner);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use std::sync::Arc;

    fn make_pid() -> Pid {
        let key = SecretKey::generate(&mut rand::rng());
        Pid { node: key.public(), seq: 1 }
    }

    #[tokio::test]
    async fn test_push_data_and_recv() {
        let mailbox = Mailbox::with_default_capacity();
        mailbox.push_data(b"hello".to_vec()).await.unwrap();
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Data(data) => assert_eq!(data, b"hello"),
            other => panic!("expected Data, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_push_tagged_and_recv_ref() {
        let mailbox = Mailbox::with_default_capacity();
        mailbox.push_data(b"unrelated".to_vec()).await.unwrap();
        mailbox.push_tagged(42, b"payload".to_vec()).await.unwrap();
        mailbox.push_data(b"also unrelated".to_vec()).await.unwrap();

        // recv_ref should skip unrelated and find the tagged message
        let msg = mailbox.recv_ref(42, Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Tagged { ref_id, payload } => {
                assert_eq!(ref_id, 42);
                assert_eq!(payload, b"payload");
            }
            other => panic!("expected Tagged, got {:?}", other),
        }

        // The other messages should still be there
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Data(data) => assert_eq!(data, b"unrelated"),
            other => panic!("expected Data, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_system_messages_at_front() {
        let mailbox = Mailbox::with_default_capacity();
        let pid = make_pid();

        mailbox.push_data(b"user msg".to_vec()).await.unwrap();
        mailbox.push_system(SystemMessage::Exit {
            from: pid.clone(),
            reason: ExitReason::Normal,
        }).await;

        // System message should come first (pushed to front)
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Exit { .. } => {}
            other => panic!("expected Exit, got {:?}", other),
        }

        // Then the data message
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Data(data) => assert_eq!(data, b"user msg"),
            other => panic!("expected Data, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_system_messages_bypass_capacity() {
        let mailbox = Mailbox::new(2);
        let pid = make_pid();

        // Fill to capacity
        mailbox.push_data(b"1".to_vec()).await.unwrap();
        mailbox.push_data(b"2".to_vec()).await.unwrap();

        // Data should fail
        assert!(mailbox.push_data(b"3".to_vec()).await.is_err());

        // System message should succeed (unbounded)
        mailbox.push_system(SystemMessage::Exit {
            from: pid,
            reason: ExitReason::Normal,
        }).await;
    }

    #[tokio::test]
    async fn test_timeout_returns_none() {
        let mailbox = Mailbox::with_default_capacity();
        let result = mailbox.recv(Some(Duration::from_millis(10))).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_capacity_enforcement() {
        let mailbox = Mailbox::new(2);
        mailbox.push_data(b"1".to_vec()).await.unwrap();
        mailbox.push_data(b"2".to_vec()).await.unwrap();

        match mailbox.push_data(b"3".to_vec()).await {
            Err(SendError::MailboxFull) => {}
            other => panic!("expected MailboxFull, got {:?}", other.is_ok()),
        }

        // After consuming one, should be able to push again
        mailbox.recv(Some(Duration::from_millis(10))).await.unwrap();
        mailbox.push_data(b"3".to_vec()).await.unwrap();
    }

    #[tokio::test]
    async fn test_close_wakes_blocked_recv() {
        let mailbox = Arc::new(Mailbox::with_default_capacity());
        let mailbox2 = mailbox.clone();

        let handle = tokio::spawn(async move {
            mailbox2.recv(None).await
        });

        // Give the task time to block on recv
        tokio::time::sleep(Duration::from_millis(10)).await;

        mailbox.close().await;
        let result = handle.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recv_ref_skips_non_matching() {
        let mailbox = Mailbox::with_default_capacity();
        let pid = make_pid();

        mailbox.push_tagged(1, b"wrong ref".to_vec()).await.unwrap();
        mailbox.push_data(b"data msg".to_vec()).await.unwrap();
        mailbox.push_system(SystemMessage::Down {
            from: pid.clone(),
            monitor_ref: 42,
            reason: ExitReason::Normal,
        }).await;

        // recv_ref(42) should find the Down message
        let msg = mailbox.recv_ref(42, Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Down { ref_id, .. } => assert_eq!(ref_id, 42),
            other => panic!("expected Down, got {:?}", other),
        }

        // Other messages should still be in queue (system at front, then tagged, then data)
        // Actually after the Down was pushed_system (front), then tagged(1) was first data,
        // then data msg. After removing Down, the order is: tagged(1), data msg
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            MailboxMessage::Tagged { ref_id, .. } => assert_eq!(ref_id, 1),
            other => panic!("expected Tagged(1), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_recv_ref_timeout_no_match() {
        let mailbox = Mailbox::with_default_capacity();
        mailbox.push_data(b"unrelated".to_vec()).await.unwrap();

        let result = mailbox.recv_ref(99, Some(Duration::from_millis(10))).await;
        assert!(result.is_none());
    }
}
