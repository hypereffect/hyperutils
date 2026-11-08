//! The server's persistent state: members, rooms, their messages and the
//! invites, in an [orangedb] database (see [`Store`]).
//!
//! Members are keyed by the hex of their hyperb key, which the transport has
//! already proven, so there are no passwords. Members are never deleted, so
//! every message keeps its author.

use std::path::Path;

pub use schema::*;

#[orangedb::schema]
pub mod schema {
    #[entity]
    #[derive(Clone, Debug, PartialEq)]
    pub struct Member {
        #[id(String)]
        pub key: MemberKey,
        pub name: String,
        /// A `data:` URI or a link to an image; empty for the default square.
        pub picture: String,
        pub joined: i64,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Room {
        #[id(auto)]
        pub id: RoomId,
        /// The name in lower case, so that two rooms cannot differ by case
        /// alone.
        #[unique]
        pub slug: String,
        pub name: String,
        pub created: i64,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Message {
        #[id(auto)]
        pub id: MessageId,
        #[references(Room, on_delete = cascade)]
        pub room: RoomId,
        #[references(Member)]
        pub author: MemberKey,
        pub at: i64,
        pub text: String,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Invite {
        #[id(String)]
        pub code: InviteCode,
        /// Who made it; nobody for the one the server prints at start.
        #[references(Member, on_delete = set_null)]
        pub by: Option<MemberKey>,
        pub created: i64,
        /// How many more people it lets in; `None` for no limit.
        pub uses_left: Option<u32>,
    }
}

/// Why someone could not join.
#[derive(Debug, PartialEq)]
pub enum JoinError {
    /// The server is invite-only and the code is unknown or used up.
    BadInvite,
}

/// Why a room could not be made.
#[derive(Debug, PartialEq)]
pub enum RoomError {
    Taken,
}

/// The database. Every method is one transaction.
///
/// A storage error is nothing a member can fix, so it panics: that ends the
/// session it happened in, which runs in its own task, and nothing else.
pub struct Store {
    db: Db,
}

impl Store {
    /// Opens the database at `path`, or creates a new one with a first room.
    pub fn open(path: &Path) -> Result<Self, String> {
        let fresh = !path.exists();
        let db = Db::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Store::new(db, fresh))
    }

    /// A fresh database in memory, for tests.
    #[cfg(test)]
    pub fn memory() -> Self {
        Store::new(Db::in_memory().expect("in-memory database"), true)
    }

    fn new(db: Db, fresh: bool) -> Self {
        let store = Store { db };
        if fresh {
            let _ = store.add_room("general", 0);
        }
        store
    }

    fn read<R>(&self, f: impl FnOnce(&ReadTx) -> orangedb::Result<R>) -> R {
        self.db.read(f).unwrap_or_else(|e| panic!("database: {e}"))
    }

    fn write<R>(&self, f: impl FnOnce(&WriteTx) -> orangedb::Result<R>) -> R {
        self.db.write(f).unwrap_or_else(|e| panic!("database: {e}"))
    }

    // Members.

    pub fn member(&self, key: &MemberKey) -> Option<Member> {
        self.read(|tx| tx.members().get(key))
    }

    pub fn members(&self) -> Vec<Member> {
        self.read(|tx| tx.members().all())
    }

    /// Adds a member, taking one use of `invite` if one is needed. Someone
    /// who is a member already just stays one.
    pub fn join(
        &self,
        key: &MemberKey,
        name: &str,
        picture: &str,
        invite: Option<&str>,
        now: i64,
    ) -> Result<(), JoinError> {
        self.write(|tx| {
            if tx.members().contains(key)? {
                return Ok(Ok(()));
            }
            if let Some(code) = invite {
                let usable = tx
                    .invites()
                    .get(InviteCode::new(code))?
                    .filter(|i| i.uses_left != Some(0));
                let Some(mut i) = usable else {
                    return Ok(Err(JoinError::BadInvite));
                };
                if let Some(n) = i.uses_left.as_mut() {
                    *n -= 1;
                }
                tx.invites().update(&i)?;
            }
            tx.members().insert(&Member {
                key: key.clone(),
                name: name.into(),
                picture: picture.into(),
                joined: now,
            })?;
            Ok(Ok(()))
        })
    }

    pub fn save_profile(&self, key: &MemberKey, name: &str, picture: &str) {
        self.write(|tx| {
            if let Some(mut m) = tx.members().get(key)? {
                m.name = name.into();
                m.picture = picture.into();
                tx.members().update(&m)?;
            }
            Ok(())
        });
    }

    // Invites.

    /// A new invite good for `uses` people, or any number for `None`.
    pub fn add_invite(&self, by: Option<&MemberKey>, uses: Option<u32>, now: i64) -> String {
        let code = invite_code();
        self.write(|tx| {
            tx.invites().insert(&Invite {
                code: InviteCode::new(code.clone()),
                by: by.cloned(),
                created: now,
                uses_left: uses,
            })
        });
        code
    }

    /// An invite that still lets someone in, if there is one.
    pub fn usable_invite(&self) -> Option<String> {
        self.read(|tx| {
            Ok(tx
                .invites()
                .all()?
                .into_iter()
                .find(|i| i.uses_left != Some(0))
                .map(|i| i.code.0))
        })
    }

    // Rooms.

    /// The rooms, oldest first.
    pub fn rooms(&self) -> Vec<Room> {
        self.read(|tx| tx.rooms().all())
    }

    pub fn room(&self, id: RoomId) -> Option<Room> {
        self.read(|tx| tx.rooms().get(id))
    }

    pub fn add_room(&self, name: &str, now: i64) -> Result<RoomId, RoomError> {
        let slug = name.to_lowercase();
        self.write(|tx| {
            if tx.rooms().by_slug(slug.clone())?.is_some() {
                return Ok(Err(RoomError::Taken));
            }
            let room = tx.rooms().create(|id| Room {
                id,
                slug,
                name: name.into(),
                created: now,
            })?;
            Ok(Ok(room.id))
        })
    }

    // Messages.

    /// Where a window of the latest `n` messages before `before` (or of the
    /// latest `n` of all) starts: the id of its first message, or 0 if it
    /// reaches the first message of the room.
    pub fn start(&self, room: RoomId, before: Option<MessageId>, n: usize) -> MessageId {
        let mut ids = self.read(|tx| tx.messages().ids_by_room(room));
        ids.sort();
        let end = before.map_or(ids.len(), |b| ids.partition_point(|id| *id < b));
        match end.checked_sub(n) {
            Some(i) if i > 0 => ids[i],
            _ => MessageId(0),
        }
    }

    /// The messages of `room` from `from` on, oldest first, and whether
    /// there are older ones.
    pub fn messages_from(&self, room: RoomId, from: MessageId) -> (Vec<Message>, bool) {
        self.read(|tx| {
            let mut ids = tx.messages().ids_by_room(room)?;
            ids.sort();
            let first = ids.partition_point(|id| *id < from);
            let mut messages = Vec::with_capacity(ids.len() - first);
            for id in &ids[first..] {
                messages.extend(tx.messages().get(*id)?);
            }
            Ok((messages, first > 0))
        })
    }

    /// Posts a message, unless the room is gone.
    pub fn post(&self, room: RoomId, author: &MemberKey, text: &str, now: i64) -> Option<MessageId> {
        self.write(|tx| {
            if !tx.rooms().contains(room)? {
                return Ok(None);
            }
            let m = tx.messages().create(|id| Message {
                id,
                room,
                author: author.clone(),
                at: now,
                text: text.into(),
            })?;
            Ok(Some(m.id))
        })
    }
}

/// Ten letters and digits that cannot be mistaken for one another.
fn invite_code() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 10];
    getrandom::getrandom(&mut bytes).expect("system random number generator");
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}
