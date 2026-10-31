//! The brackets, their links and who made what, in an [orangedb] database.
//! Owners are keyed by the hex of the renderer's public key.
//!
//! A storage error is nothing a visitor can fix, so [`read`] and [`write`]
//! panic on one: that ends the session it happened in, which runs in its own
//! task, and nothing else.

use std::time::{SystemTime, UNIX_EPOCH};

pub use schema::*;

#[orangedb::schema]
pub mod schema {
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Bracket {
        /// A cuid2, which is also the path of the link that only shows it.
        #[id(String)]
        pub id: BracketId,
        /// The cuid2 of the link that can enter results.
        pub edit: String,
        /// The creator's key.
        pub owner: String,
        pub name: String,
        pub teams: Vec<String>,
        /// The first round, two spots a match: a team by its index in
        /// `teams`, or `None` for a bye. Its length is a power of two.
        pub slots: Vec<Option<u32>>,
        /// The score of every match, round by round, first spot's then
        /// second's, once it is played. Byes have none.
        pub scores: Vec<Vec<Option<[u32; 2]>>>,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// The link that can enter results, pointing at its bracket.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct EditLink {
        #[id(String)]
        pub id: EditId,
        #[references(Bracket, on_delete = cascade)]
        pub bracket: BracketId,
    }

    /// The brackets a key created, newest first. A deleted bracket leaves
    /// the list by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Bracket, on_delete = set_null)]
        pub brackets: Vec<BracketId>,
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
