//! Library surface of the indexer-service crate.
//!
//! The crate is primarily a binary (head-tracker / worker / refresher), but
//! we also expose its modules as a library so other crates in the workspace
//! (specifically `indexer-web`) can reuse the Redis queue type and the env
//! config without copy-paste.

pub mod config;
pub mod head_tracker;
pub mod queue;
pub mod refresher;
pub mod worker;

pub mod state {
    //! Shared Redis keys + helpers for service-wide state visible to operators.

    /// Key into which the head-tracker writes its last observed chain head.
    /// Set with a short TTL so a stuck tracker disappears from the admin
    /// health view rather than reporting a stale block forever.
    pub const LAST_HEAD_KEY: &str = "indexer:state:last_head";
    /// TTL on `LAST_HEAD_KEY`. Must comfortably exceed the head-tracker's
    /// poll interval; 60s is well over the 4s default.
    pub const LAST_HEAD_TTL_SECS: u64 = 60;
}
