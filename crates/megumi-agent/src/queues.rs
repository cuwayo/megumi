//! One turn at a time per chat, many chats at once.
//!
//! A turn is a multi-step conversation with the model, so two turns for the
//! same chat must not interleave — they would each see the other's half-stored
//! state. Two turns for *different* chats are independent and should run
//! together. [`ChatQueues`] gives each chat its own async mutex, created on
//! first use, so serialization is per chat and parallelism is across chats.
//!
//! The outer `std::sync::Mutex` guards only the map; it is never held across an
//! `await`, so it does not serialize turns itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::event::ChatId;

/// A per-chat lock, so a chat's turns never overlap.
#[derive(Default)]
pub struct ChatQueues {
    queues: Mutex<HashMap<ChatId, Arc<AsyncMutex<()>>>>,
}

impl ChatQueues {
    /// A queue with no chats registered yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the lock for `chat`, waiting for any turn already running.
    ///
    /// The returned guard releases the lock when dropped. A mutex is fair among
    /// tasks already waiting, but it does not restore *arrival* order across
    /// events: WhatsApp's default concurrent delivery spawns each event on its
    /// own task, so which of two same-chat events reaches this lock first is
    /// scheduler-dependent. That is accepted here — turns are serialized, and
    /// near-simultaneous messages are rare — rather than paying for a global
    /// ordering that the rest of the pipeline does not need.
    pub async fn lock(&self, chat: &ChatId) -> OwnedMutexGuard<()> {
        let queue = {
            let mut queues = self
                .queues
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(queues.entry(chat.clone()).or_default())
        };
        queue.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn the_same_chat_serializes() {
        let queues = Arc::new(ChatQueues::new());
        let chat = ChatId::new("gA");

        let first = queues.lock(&chat).await;
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let queues = Arc::clone(&queues);
            let chat = chat.clone();
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                let _guard = queues.lock(&chat).await;
                started.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
        drop(first);
        waiter.await.unwrap();
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn different_chats_run_together() {
        let queues = ChatQueues::new();
        let _a = queues.lock(&ChatId::new("gA")).await;
        // Would deadlock if the two chats shared a lock.
        let _b = queues.lock(&ChatId::new("gB")).await;
    }
}
