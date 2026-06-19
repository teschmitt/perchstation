//! PS-10 — eviction (capture task) racing `transition_inflight` (delivery
//! task) on the same `pending/` clips, with no lock between them.
//!
//! A pressure-driven eviction sweep (`apply_policy`) reads every `pending/`
//! sidecar and unlinks some of them, while a fleet of `transition_inflight`
//! calls concurrently renames those same `pending/<id>.mp4` files into
//! `inflight/` and removes their sidecars. Without the fix this surfaces raw
//! `QueueError::Io` errors (a sidecar that vanished between `read_dir` and
//! `read`; a rename whose source was unlinked underneath it). The fix makes
//! both sides tolerate the race: the eviction census skips vanished sidecars,
//! and `transition_inflight` reports a graceful `MissingMedia` instead of a
//! raw `Io`. The on-disk state stays consistent — a clip's media is never
//! duplicated across `pending/` and `inflight/`.

use chrono::{TimeZone, Utc};

use perchstation_core::config::EvictionPolicy;
use perchstation_core::queue::policy::{QueuePolicy, apply_policy};
use perchstation_core::queue::store::QueueStore;
use perchstation_core::queue::{ClipQueueEntry, InboxError, QueueError};

const CLIPS_PER_ROUND: usize = 40;
const ROUNDS: usize = 25;

#[tokio::test(flavor = "multi_thread")]
async fn evict_and_transition_inflight_race_stays_consistent() {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    for round in 0..ROUNDS {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = QueueStore::open(dir.path()).expect("open store");

        // Stage a batch of pending clips. Each is both an eviction candidate
        // and a delivery head, so the sweep and the transitions contend.
        let mut clip_ids = Vec::with_capacity(CLIPS_PER_ROUND);
        for c in 0..CLIPS_PER_ROUND {
            let clip_id = format!("20260101T0000{c:02}Z-{round:03}");
            std::fs::write(store.pending_dir().join(format!("{clip_id}.mp4")), [0u8; 16])
                .expect("write mp4");
            let entry = ClipQueueEntry::new(&clip_id, now, now, 16);
            std::fs::write(
                store.pending_dir().join(format!("{clip_id}.json")),
                serde_json::to_vec_pretty(&entry).expect("serialise"),
            )
            .expect("write sidecar");
            clip_ids.push(clip_id);
        }

        // Delivery side: one `transition_inflight` per clip, all concurrent.
        let transitions: Vec<_> = clip_ids
            .iter()
            .map(|clip_id| {
                let store = store.clone();
                let entry = ClipQueueEntry::new(clip_id, now, now, 16);
                tokio::task::spawn_blocking(move || store.transition_inflight(entry, now))
            })
            .collect();

        // Capture side: a pressure-driven eviction sweep. `max_clips = 1`
        // forces the census to read every sidecar and try to unlink most of
        // them, racing the transitions above.
        let evict = {
            let store = store.clone();
            let policy = QueuePolicy {
                max_clips: 1,
                max_bytes: u64::MAX,
                eviction: EvictionPolicy::DropOldestUndelivered,
            };
            tokio::task::spawn_blocking(move || apply_policy(&store, policy, 0))
        };

        let evict_result = evict.await.expect("evict task join");
        // The eviction side must tolerate the race. `QueueFull` is a legitimate
        // outcome (the un-evictable inflight floor can fill the ceiling), but a
        // raw store `Io` error — e.g. a sidecar that vanished between `read_dir`
        // and `read` — must NEVER leak.
        match evict_result.result {
            Ok(()) | Err(InboxError::QueueFull { .. }) => {}
            Err(InboxError::Queue(e)) => {
                panic!("round {round}: apply_policy leaked a raw store error: {e:?}");
            }
        }

        for (clip_id, handle) in clip_ids.iter().zip(transitions) {
            let result = handle.await.expect("transition task join");
            // The delivery side must surface only a graceful MissingMedia when
            // the eviction won — never a raw Io error from the failed rename.
            let pending_mp4 = store.pending_dir().join(format!("{clip_id}.mp4")).exists();
            let pending_json = store.pending_dir().join(format!("{clip_id}.json")).exists();
            let inflight_mp4 = store.inflight_dir().join(format!("{clip_id}.mp4")).exists();
            let inflight_json = store.inflight_dir().join(format!("{clip_id}.json")).exists();

            match result {
                Ok(_) => {
                    // Transition won: the clip is a complete pair in inflight/
                    // and fully gone from pending/.
                    assert!(
                        inflight_mp4 && inflight_json,
                        "round {round}: {clip_id} reported delivered but is not a complete inflight pair",
                    );
                    assert!(
                        !pending_mp4 && !pending_json,
                        "round {round}: {clip_id} left a residue in pending/ after transitioning",
                    );
                }
                Err(QueueError::MissingMedia { .. }) => {
                    // Eviction won: the media is gone, never stranded in inflight/.
                    assert!(
                        !inflight_mp4,
                        "round {round}: {clip_id} reported MissingMedia but its mp4 is in inflight/",
                    );
                }
                Err(other) => {
                    panic!("round {round}: {clip_id} leaked a non-graceful error: {other:?}");
                }
            }

            // A clip's media is never present in BOTH pending/ and inflight/.
            assert!(
                !(pending_mp4 && inflight_mp4),
                "round {round}: {clip_id} mp4 present in BOTH pending/ and inflight/",
            );
        }
    }
}
