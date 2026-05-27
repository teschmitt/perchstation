//! Delivery subsystem: the long-running upload loop and the classify-task
//! poller.
//!
//! Layout follows `specs/001-clip-delivery/plan.md` §Project Structure:
//!
//! - [`runner`] — picks the oldest `pending/` clip, uploads it via the
//!   mTLS client, and transitions the entry into `delivered/`.
//! - [`classify`] — scans `delivered/` and polls perchpub for the
//!   post-upload classify-task status.
//!
//! Retry policy (US2 T045) and full error classification (US2 T046, T052)
//! layer on top of the happy-path loops here.

pub mod classify;
pub mod runner;
