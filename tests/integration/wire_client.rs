//! PS-06 / PS-16 / PS-22 — perchpub wire-client edge cases, exercised
//! end-to-end against the TLS fake perchpub via `PerchpubClient` directly
//! (in process; no `serve` binary). These cover the response-handling paths
//! the unit tests in `perchpub::client` model, but over a real mTLS round
//! trip so a regression in `read_capped`/`classify_response` is caught.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};

use perchstation_core::perchpub::client::{ClientError, PerchpubClient};
use uuid::Uuid;

use support::fakepub::FakePerchpub;
use support::fixtures::{build_station_keypair, sample_mp4_bytes, write_test_credentials};

/// Stand up on-disk credentials minted by the fake CA and build an mTLS
/// client pointed at it.
fn client_against(pub_: &FakePerchpub, data_dir: &Path) -> PerchpubClient {
    let station_id = Uuid::new_v4();
    let station_key = build_station_keypair();
    let station_cert_pem = pub_.mint_station_cert(&station_key, station_id);
    write_test_credentials(
        data_dir,
        station_id,
        pub_.url(),
        &station_key.serialize_pem(),
        &station_cert_pem,
        pub_.ca_pem(),
    )
    .expect("write credentials");
    PerchpubClient::new(data_dir, pub_.url()).expect("build mTLS client")
}

fn write_clip(dir: &Path) -> PathBuf {
    let path = dir.join("clip.mp4");
    std::fs::write(&path, sample_mp4_bytes()).expect("write clip");
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_accepts_2xx_other_status() {
    // PS-22: a 201 (not just 200) is success, not a Terminal → Undeliverable drop.
    let pub_ = FakePerchpub::start().await;
    pub_.respond_upload_status(201);
    let dir = tempfile::tempdir().unwrap();
    let client = client_against(&pub_, dir.path());
    let clip = write_clip(dir.path());

    let task = client.upload_clip(&clip, "clip-1").await.expect("a 201 must be Ok");
    assert_eq!(task.object_name, "clip-1.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_undecodable_2xx_is_undecodable_success() {
    // PS-06: a 200 whose body won't decode → UndecodableSuccess (the clip is
    // already stored; the runner records it delivered, never re-uploads).
    let pub_ = FakePerchpub::start().await;
    pub_.respond_upload_undecodable();
    let dir = tempfile::tempdir().unwrap();
    let client = client_against(&pub_, dir.path());
    let clip = write_clip(dir.path());

    let err = client.upload_clip(&clip, "clip-1").await.expect_err("undecodable 2xx");
    assert!(matches!(err, ClientError::UndecodableSuccess { .. }), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_caps_oversized_response_body() {
    // PS-16: a multi-megabyte response body must error rather than buffer
    // unbounded (OOM on a Pi).
    let pub_ = FakePerchpub::start().await;
    pub_.respond_upload_oversized();
    let dir = tempfile::tempdir().unwrap();
    let client = client_against(&pub_, dir.path());
    let clip = write_clip(dir.path());

    let err = client.upload_clip(&clip, "clip-1").await.expect_err("oversized body must error");
    assert!(matches!(err, ClientError::Decode { .. }), "got {err:?}");
}
