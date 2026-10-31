//! The tournaments, their links and who made what, in an [orangedb]
//! database. Owners are keyed by the hex of the renderer's public key.
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
    pub struct Tournament {
        /// A cuid2, which is also the path of the link that only shows it.
        #[id(String)]
        pub id: TournamentId,
        /// The cuid2 of the link that can enter results.
        pub edit: String,
        /// The creator's key.
        pub owner: String,
        pub name: String,
        pub teams: Vec<String>,
        pub points: crate::store::Points,
        /// Every game, round by round.
        pub games: Vec<crate::store::Game>,
        /// Bumped on every change, so that watchers know to refresh.
        pub version: u64,
        /// When it last changed, in seconds since the epoch.
        pub touched: u64,
    }

    /// The link that can enter results, pointing at its tournament.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct EditLink {
        #[id(String)]
        pub id: EditId,
        #[references(Tournament, on_delete = cascade)]
        pub tournament: TournamentId,
    }

    /// The tournaments a key created, newest first. A deleted tournament
    /// leaves the list by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Tournament, on_delete = set_null)]
        pub tournaments: Vec<TournamentId>,
    }
}

/// What a win, a draw and a loss are worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Points {
    pub win: u32,
    pub draw: u32,
    pub loss: u32,
}

impl Default for Points {
    fn default() -> Self {
        Points {
            win: 3,
            draw: 1,
            loss: 0,
        }
    }
}

/// A game between two teams, by their index in the tournament's list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    /// Counting from 0.
    pub round: u32,
    pub home: u32,
    pub away: u32,
    /// Home's score and away's, once it is played.
    pub score: Option<[u32; 2]>,
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
