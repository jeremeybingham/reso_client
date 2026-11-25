//! A shutdown request that a run checks between pages
//!
//! The command line triggers it on SIGTERM or Ctrl-C; tests and library users trigger it directly.
//! A run that sees it stops asking for pages, keeps what is already written and checkpointed, and
//! reports itself as interrupted.

use std::sync::Arc;

use tokio::sync::watch;

/// A shutdown request; clones share one state
#[derive(Debug, Clone)]
pub struct Shutdown {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    /// A request that has not been made yet
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    /// Ask every run holding a clone to stop (calling it again does nothing)
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    /// True once [`trigger`](Self::trigger) was called
    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Wait until [`trigger`](Self::trigger) is called; returns at once when it already was
    pub async fn triggered(&self) {
        let mut rx = self.tx.subscribe();
        // The sender lives in `self`, so the channel cannot close while waiting
        let _ = rx.wait_for(|v| *v).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_wakes_waiters_and_sticks() {
        let s = Shutdown::new();
        assert!(!s.is_triggered());
        let waiter = tokio::spawn({
            let s = s.clone();
            async move { s.triggered().await }
        });
        s.trigger();
        waiter.await.unwrap();
        assert!(s.is_triggered());
        s.triggered().await; // already triggered: returns at once
    }
}
