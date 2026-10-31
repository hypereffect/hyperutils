//! The rooms, who is in them and what they voted, in an [orangedb] database.
//! Managers and players are keyed by the hex of the renderer's public key,
//! which is never shown.
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
    pub struct Room {
        /// A cuid2, which is also the path of the room's link.
        #[id(String)]
        pub id: RoomId,
        /// The manager's key.
        pub owner: String,
        pub title: String,
        /// Who joined, in the order they came.
        pub players: Vec<crate::store::Player>,
        /// The round being played, or the last one played.
        pub round: Option<crate::store::Round>,
        /// The rounds before it, newest first.
        pub history: Vec<crate::store::Summary>,
        /// The id the next player gets, so that ids are never reused and a
        /// stale button never hits the wrong one.
        pub next_id: u32,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// The rooms a key opened, newest first. A deleted room leaves the list
    /// by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Room, on_delete = set_null)]
        pub rooms: Vec<RoomId>,
    }
}

/// Someone in a room.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Player {
    /// Stable for as long as they are in the room; buttons refer to it.
    pub id: u32,
    pub key: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Round {
    /// 1 for the first round of the room.
    pub number: u32,
    /// What is being estimated; may be blank.
    pub topic: String,
    /// The votes in, in the order they came.
    pub votes: Vec<Vote>,
    /// Whether everyone may see the votes now. No more votes are taken.
    pub revealed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    pub key: String,
    /// The voter's name when they voted, for when they leave the room.
    pub name: String,
    pub value: f64,
}

/// How a past round ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub number: u32,
    pub topic: String,
    pub result: String,
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
