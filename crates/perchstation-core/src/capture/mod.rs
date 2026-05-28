//! Capture subsystem: motion-triggered recorder.
//!
//! Public surface is filled in by US1 (T018). This file currently only
//! declares the submodules so the workspace compiles after the Phase 1
//! skeleton lands; the supervised task entry point lands later.

pub mod cooldown;
pub mod liveness;
pub mod recording;
pub mod runner;
pub mod staging;
pub mod state;
