//! Capture-subsystem-facing entry point into the queue.
//!
//! The capture pipeline holds a `Box<dyn Inbox>` and calls
//! [`Inbox::submit`] once per captured clip; the default
//! [`StoreInbox`] delegates straight to [`super::store::QueueStore::enqueue`].
//!
//! Eviction-policy interception (T047) wraps this trait via
//! [`super::policy::PolicyInbox`]; zero-length / unreadable pre-flight
//! (T049) lives in the delivery loop on the receiving end.

use std::path::Path;

use async_trait::async_trait;

use super::store::{ClipMeta, QueueStore};
use super::{ClipQueueEntry, InboxError};

/// Capture → delivery handoff. Implementations are `Send + Sync` so the
/// capture loop can hold them across `await` points and share them with
/// supervisory tasks.
#[async_trait]
pub trait Inbox: Send + Sync {
    /// Move `clip_path` (and its associated [`ClipMeta`]) into the
    /// `pending/` queue. Returns the resulting [`ClipQueueEntry`] so the
    /// caller can correlate logs with the assigned clip-id.
    async fn submit(&self, clip_path: &Path, meta: ClipMeta) -> Result<ClipQueueEntry, InboxError>;
}

/// Default [`Inbox`] backed by a [`QueueStore`]. Filesystem ops are sync,
/// so [`submit`](Self::submit) hops to a blocking-friendly task to avoid
/// stalling the runtime under unexpectedly slow disks.
pub struct StoreInbox {
    store: QueueStore,
}

impl StoreInbox {
    #[must_use]
    pub fn new(store: QueueStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Inbox for StoreInbox {
    async fn submit(&self, clip_path: &Path, meta: ClipMeta) -> Result<ClipQueueEntry, InboxError> {
        let store = self.store.clone();
        let path = clip_path.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || store.enqueue(&path, meta))
            .await
            .expect("queue enqueue task panicked")?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn store_inbox_submits_clip_into_pending() {
        let dir = TempDir::new().unwrap();
        let store = QueueStore::open(dir.path()).unwrap();
        let inbox = StoreInbox::new(store.clone());

        let src = dir.path().join("clip.mp4");
        std::fs::write(&src, b"bytes").unwrap();
        let entry = inbox.submit(&src, ClipMeta { captured_at: Utc::now() }).await.expect("submit");

        assert!(store.pending_dir().join(format!("{}.mp4", entry.clip_id)).is_file());
        assert!(store.pending_dir().join(format!("{}.json", entry.clip_id)).is_file());
    }
}
