//! The contests, their links and who made what, in an [orangedb] database.
//! Owners are keyed by the hex of the renderer's public key.
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
    pub struct Contest {
        /// A cuid2, which is also the path of the link that only shows the
        /// leaderboard.
        #[id(String)]
        pub id: ContestId,
        /// The cuid2 of the organizer's link.
        pub admin: String,
        /// The creator's key.
        pub owner: String,
        pub name: String,
        pub entries: Vec<String>,
        pub judges: Vec<crate::store::Judge>,
        pub scale: crate::store::Scale,
        /// Whether the leaderboard is out. Scores are locked while it is.
        pub revealed: bool,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// The organizer's link, pointing at its contest.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct AdminLink {
        #[id(String)]
        pub id: AdminId,
        #[references(Contest, on_delete = cascade)]
        pub contest: ContestId,
    }

    /// A judge's link, pointing at its contest and the judge in it.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct JudgeLink {
        #[id(String)]
        pub id: JudgeId,
        #[references(Contest, on_delete = cascade)]
        pub contest: ContestId,
        /// The judge's index in the contest's list.
        pub judge: u32,
    }

    /// The contests a key created, newest first. A deleted contest leaves
    /// the list by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Contest, on_delete = set_null)]
        pub contests: Vec<ContestId>,
    }
}

/// A judge and the scores they gave.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judge {
    pub name: String,
    /// The cuid2 of their link.
    pub link: String,
    /// One per entry, in the contest's order; `None` until scored.
    pub scores: Vec<Option<u32>>,
}

/// The lowest and the highest score a judge can give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scale {
    pub min: u32,
    pub max: u32,
}

impl Default for Scale {
    fn default() -> Self {
        Scale { min: 1, max: 10 }
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
