//! The sheets and who made what, in an [orangedb] database. Owners and the
//! people on a sheet are keyed by the hex of the renderer's public key, which
//! is never shown; what is shown is its [`tag`](crate::signup::tag).
//!
//! A storage error is nothing a visitor can fix, so [`read`] and [`write`]
//! panic on one: that ends the session it happened in, which runs in its own
//! task, and nothing else.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub use schema::*;

#[orangedb::schema]
pub mod schema {
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Sheet {
        /// A cuid2, which is also the path of the sheet's link.
        #[id(String)]
        pub id: SheetId,
        /// The creator's key.
        pub owner: String,
        pub title: String,
        pub description: String,
        pub items: Vec<crate::store::Item>,
        /// What happened to the sheet, newest first.
        pub log: Vec<crate::store::Change>,
        /// The id the next new item or name gets, so that ids are never
        /// reused and a stale button never hits the wrong one.
        pub next_id: u32,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// The sheets a key created, newest first. A deleted sheet leaves the
    /// list by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Sheet, on_delete = set_null)]
        pub sheets: Vec<SheetId>,
    }
}

/// A role to fill or a thing to bring.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Stable across edits; buttons refer to it.
    pub id: u32,
    pub label: String,
    /// How many names it takes; `None` for as many as come.
    pub capacity: Option<u32>,
    /// The names down for it, in the order they came.
    pub names: Vec<Name>,
}

impl Item {
    pub fn is_full(&self, max: u32) -> bool {
        self.names.len() as u32 >= self.capacity.unwrap_or(max)
    }
}

/// A name put down for an item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Name {
    pub id: u32,
    pub name: String,
    /// The key that put it down, so that it can take it off again.
    pub key: String,
    /// That key's tag, shown with it.
    pub by: String,
}

/// A line of a sheet's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The tag of the key that made it.
    pub by: String,
    /// In seconds since the epoch.
    pub at: u64,
    pub what: String,
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
