//! The meetings, their answers and what is remembered about each person, in
//! an [orangedb] database. People are keyed by the hex of the renderer's
//! public key. A person stays for [`IDLE_TTL`] since their last visit, so a
//! returning visitor with the same key is recognized again.
//!
//! A storage error is nothing a visitor can fix, so [`read`] and [`write`]
//! panic on one: that ends the session it happened in, which runs in its own
//! task, and nothing else.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use schema::*;

/// How long a person is remembered without a visit.
pub const IDLE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[orangedb::schema]
pub mod schema {
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Meeting {
        #[id(String)]
        pub code: Code,
        pub name: String,
        pub description: String,
        /// The organizer's key.
        pub owner: String,
        pub owner_name: String,
        pub options: Vec<String>,
        pub closed: bool,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// One person's answer to a meeting; a meeting lists them in the order
    /// they first came.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Answer {
        #[id(auto)]
        pub id: AnswerId,
        #[references(Meeting, on_delete = cascade)]
        pub meeting: Code,
        pub key: String,
        pub name: String,
        pub picks: Vec<bool>,
        pub other: String,
    }

    #[entity(accessor = people)]
    #[derive(Clone, Debug)]
    pub struct Person {
        #[id(String)]
        pub key: PersonKey,
        /// The name last used.
        pub name: String,
        /// Meetings they created or answered, newest first. A deleted
        /// meeting leaves the list by itself.
        #[references(Meeting, on_delete = set_null)]
        pub meetings: Vec<Code>,
        /// When they were last here, in seconds since the epoch.
        pub seen: u64,
    }
}

pub fn read<R>(db: &Db, f: impl FnOnce(&ReadTx) -> orangedb::Result<R>) -> R {
    db.read(f).unwrap_or_else(|e| panic!("database: {e}"))
}

pub fn write<R>(db: &Db, f: impl FnOnce(&WriteTx) -> orangedb::Result<R>) -> R {
    db.write(f).unwrap_or_else(|e| panic!("database: {e}"))
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Whether something last touched at `at` has been idle for `ttl`.
pub fn expired(at: u64, ttl: Duration) -> bool {
    now().saturating_sub(at) >= ttl.as_secs()
}
