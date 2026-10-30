//! The BBS's persistent state: users, message boards, mail, one-liners,
//! bulletins and the callers log, in an [orangedb] database (see [`Store`]).
//!
//! Users are keyed by the hex of their iroh endpoint key, which the
//! connection has already proven, so there are no passwords. Users are never
//! deleted, so everything that names one references it.

use std::collections::BTreeMap;
use std::path::Path;

pub use schema::*;

/// How many callers the log keeps.
const CALLERS_KEPT: u64 = 50;
/// How many one-liners the wall keeps.
const ONELINERS_KEPT: u64 = 50;

#[orangedb::schema]
pub mod schema {
    use std::collections::BTreeMap;

    #[entity]
    #[derive(Clone, Debug)]
    pub struct User {
        #[id(String)]
        pub key: UserKey,
        pub handle: String,
        pub location: String,
        /// Shown under the user's posts, like a signature.
        pub tagline: String,
        pub sysop: bool,
        pub banned: bool,
        pub joined: i64,
        pub last_on: i64,
        pub calls: u64,
        pub posts: u64,
        /// The newest post the user has seen in each topic.
        pub last_read: BTreeMap<TopicId, PostId>,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Board {
        #[id(auto)]
        pub id: BoardId,
        pub name: String,
        pub description: String,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Topic {
        #[id(auto)]
        pub id: TopicId,
        #[references(Board, on_delete = cascade)]
        pub board: BoardId,
        pub subject: String,
        #[references(User)]
        pub author: UserKey,
        pub created: i64,
        /// The id of the newest post in it.
        pub last_post: PostId,
        pub last_at: i64,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Post {
        #[id(auto)]
        pub id: PostId,
        #[references(Topic, on_delete = cascade)]
        pub topic: TopicId,
        #[references(User)]
        pub author: UserKey,
        pub at: i64,
        pub body: String,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Letter {
        #[id(auto)]
        pub id: LetterId,
        #[references(User)]
        pub from: UserKey,
        #[references(User)]
        pub to: UserKey,
        pub subject: String,
        pub body: String,
        pub at: i64,
        pub read: bool,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Oneliner {
        #[id(auto)]
        pub id: OnelinerId,
        #[references(User)]
        pub author: UserKey,
        pub text: String,
        pub at: i64,
    }

    #[entity]
    #[derive(Clone, Debug)]
    pub struct Bulletin {
        #[id(auto)]
        pub id: BulletinId,
        pub title: String,
        pub body: String,
        pub at: i64,
    }

    /// A login. Its id is the caller number, which keeps counting when old
    /// entries leave the log.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Caller {
        #[id(auto)]
        pub id: CallerId,
        #[references(User)]
        pub key: UserKey,
        pub node: u32,
        pub at: i64,
    }
}

/// Whether `user` has posts in this topic they have not read.
pub fn is_unread(user: Option<&User>, topic: &Topic) -> bool {
    let seen = user.and_then(|u| u.last_read.get(&topic.id));
    seen.is_none_or(|seen| topic.last_post > *seen)
}

/// The handle of `user`, or a placeholder for someone who is gone.
fn handle_of(user: Option<User>) -> String {
    user.map_or_else(|| "<unknown>".into(), |u| u.handle)
}

/// The database. Every method is one transaction.
///
/// A storage error is nothing a caller can fix, so it panics: that ends the
/// session it happened in, which runs in its own task, and nothing else.
pub struct Store {
    db: Db,
}

/// What the sysop console counts.
pub struct Stats {
    pub users: u64,
    pub topics: u64,
    pub posts: u64,
    pub letters: u64,
}

impl Store {
    /// Opens the database at `path`, or creates and seeds a new one.
    pub fn open(path: &Path) -> Result<Self, String> {
        let fresh = !path.exists();
        let db = Db::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Store::new(db, fresh))
    }

    /// A database in memory, for tests.
    #[cfg(test)]
    pub fn memory() -> Self {
        Store::new(Db::in_memory().expect("in-memory database"), true)
    }

    fn new(db: Db, fresh: bool) -> Self {
        let store = Store { db };
        if fresh {
            // A fresh BBS, with a few boards to start from.
            for (name, description) in [
                ("General", "Anything goes. Be excellent to each other."),
                ("Tech Talk", "Hardware, software, protocols and hacks."),
                ("Trading Post", "Buy, sell, swap."),
            ] {
                store.add_board(name, description);
            }
        }
        store
    }

    fn read<R>(&self, f: impl FnOnce(&ReadTx) -> orangedb::Result<R>) -> R {
        self.db.read(f).unwrap_or_else(|e| panic!("database: {e}"))
    }

    fn write<R>(&self, f: impl FnOnce(&WriteTx) -> orangedb::Result<R>) -> R {
        self.db.write(f).unwrap_or_else(|e| panic!("database: {e}"))
    }

    // Users.

    pub fn user(&self, key: &UserKey) -> Option<User> {
        self.read(|tx| tx.users().get(key))
    }

    /// The handle of `key`, or a placeholder for someone who is gone.
    pub fn handle(&self, key: &UserKey) -> String {
        handle_of(self.user(key))
    }

    /// The key of the user with this handle, ignoring case.
    pub fn find_handle(&self, handle: &str) -> Option<UserKey> {
        self.read(|tx| Ok(find_handle(&tx.users().all()?, handle)))
    }

    pub fn users(&self) -> Vec<User> {
        self.read(|tx| tx.users().all())
    }

    /// Adds a user, unless the handle is taken; the first one runs the place.
    /// Returns whether it did.
    pub fn register(&self, key: &UserKey, handle: &str, location: &str, tagline: &str, now: i64) -> bool {
        self.write(|tx| {
            let users = tx.users().all()?;
            if find_handle(&users, handle).is_some() {
                return Ok(false);
            }
            tx.users().insert(&User {
                key: key.clone(),
                handle: handle.into(),
                location: location.into(),
                tagline: tagline.into(),
                sysop: users.is_empty(),
                banned: false,
                joined: now,
                last_on: 0,
                calls: 0,
                posts: 0,
                last_read: BTreeMap::new(),
            })?;
            Ok(true)
        })
    }

    pub fn save_profile(&self, key: &UserKey, location: &str, tagline: &str) {
        self.edit_user(key, |u| {
            u.location = location.into();
            u.tagline = tagline.into();
        });
    }

    /// Bans or unbans `key`; returns their handle if they exist.
    pub fn set_banned(&self, key: &UserKey, banned: bool) -> Option<String> {
        self.edit_user(key, |u| {
            u.banned = banned;
            u.handle.clone()
        })
    }

    /// Records that `key` has seen the topic up to `post`.
    pub fn mark_read(&self, key: &UserKey, topic: TopicId, post: PostId) {
        self.edit_user(key, |u| {
            u.last_read.insert(topic, post);
        });
    }

    fn edit_user<R>(&self, key: &UserKey, f: impl FnOnce(&mut User) -> R) -> Option<R> {
        self.write(|tx| {
            let Some(mut u) = tx.users().get(key)? else {
                return Ok(None);
            };
            let r = f(&mut u);
            tx.users().update(&u)?;
            Ok(Some(r))
        })
    }

    /// Logs a call; returns its caller number.
    pub fn log_call(&self, key: &UserKey, node: u32, now: i64) -> u64 {
        self.write(|tx| {
            let caller = tx.callers().create(|id| Caller {
                id,
                key: key.clone(),
                node,
                at: now,
            })?;
            for old in tx.callers().all()?.iter().rev().skip(CALLERS_KEPT as usize) {
                tx.callers().delete(old.id)?;
            }
            if let Some(mut u) = tx.users().get(key)? {
                u.calls += 1;
                u.last_on = now;
                tx.users().update(&u)?;
            }
            Ok(caller.id.0)
        })
    }

    /// The most recent logins, oldest first.
    pub fn callers(&self) -> Vec<Caller> {
        self.read(|tx| tx.callers().all())
    }

    /// Every login ever: the number of the latest one.
    pub fn calls(&self) -> u64 {
        self.read(|tx| Ok(tx.callers().all()?.last().map_or(0, |c| c.id.0)))
    }

    // Boards, topics and posts.

    pub fn boards(&self) -> Vec<Board> {
        self.read(|tx| tx.boards().all())
    }

    pub fn board(&self, id: BoardId) -> Option<Board> {
        self.read(|tx| tx.boards().get(id))
    }

    pub fn add_board(&self, name: &str, description: &str) -> BoardId {
        self.write(|tx| {
            let board = tx.boards().create(|id| Board {
                id,
                name: name.into(),
                description: description.into(),
            })?;
            Ok(board.id)
        })
    }

    /// Deletes a board with its topics and their posts.
    pub fn delete_board(&self, id: BoardId) {
        self.write(|tx| tx.boards().delete(id));
    }

    pub fn topic(&self, id: TopicId) -> Option<Topic> {
        self.read(|tx| tx.topics().get(id))
    }

    /// The topics of a board, most recently active first.
    pub fn topics_of(&self, board: BoardId) -> Vec<Topic> {
        let mut topics = self.read(|tx| tx.topics().by_board(board));
        topics.sort_by_key(|t| std::cmp::Reverse(t.last_post));
        topics
    }

    pub fn topic_count(&self, board: BoardId) -> usize {
        self.read(|tx| Ok(tx.topics().ids_by_board(board)?.len()))
    }

    /// The posts of a topic, oldest first.
    pub fn posts_of(&self, topic: TopicId) -> Vec<Post> {
        self.read(|tx| tx.posts().by_topic(topic))
    }

    pub fn post_count(&self, topic: TopicId) -> usize {
        self.read(|tx| Ok(tx.posts().ids_by_topic(topic)?.len()))
    }

    /// How many topics in each board have posts `key` has not read.
    pub fn unread(&self, key: &UserKey) -> BTreeMap<BoardId, usize> {
        self.read(|tx| {
            let user = tx.users().get(key)?;
            let mut unread = BTreeMap::new();
            for b in tx.boards().all()? {
                let topics = tx.topics().by_board(b.id)?;
                let n = topics.iter().filter(|t| is_unread(user.as_ref(), t)).count();
                unread.insert(b.id, n);
            }
            Ok(unread)
        })
    }

    /// Starts a topic with its first post, unless the board is gone.
    pub fn add_topic(&self, board: BoardId, author: &UserKey, subject: &str, body: &str, now: i64) -> Option<TopicId> {
        self.write(|tx| {
            if !tx.boards().contains(board)? {
                return Ok(None);
            }
            let topic = tx.topics().create(|id| Topic {
                id,
                board,
                subject: subject.into(),
                author: author.clone(),
                created: now,
                last_post: PostId(0),
                last_at: now,
            })?;
            let id = topic.id;
            add_post(tx, topic, author, body, now)?;
            Ok(Some(id))
        })
    }

    /// Adds a post, unless the topic is gone.
    pub fn add_post(&self, topic: TopicId, author: &UserKey, body: &str, now: i64) -> Option<PostId> {
        self.write(|tx| match tx.topics().get(topic)? {
            Some(t) => add_post(tx, t, author, body, now).map(Some),
            None => Ok(None),
        })
    }

    /// Deletes a post if `allowed` says so, and its topic if it was the last
    /// one in it. Returns the topic's board and whether the topic is gone too.
    pub fn delete_post(&self, id: PostId, allowed: impl FnOnce(&Post) -> bool) -> Option<(BoardId, bool)> {
        self.write(|tx| {
            let Some(post) = tx.posts().get(id)?.filter(|p| allowed(p)) else {
                return Ok(None);
            };
            let Some(mut topic) = tx.topics().get(post.topic)? else {
                return Ok(None);
            };
            tx.posts().delete(id)?;
            match tx.posts().by_topic(topic.id)?.last() {
                Some(last) => {
                    topic.last_post = last.id;
                    topic.last_at = last.at;
                    tx.topics().update(&topic)?;
                    Ok(Some((topic.board, false)))
                }
                None => {
                    tx.topics().delete(topic.id)?;
                    Ok(Some((topic.board, true)))
                }
            }
        })
    }

    // Mail.

    /// The letters to `key`, newest first.
    pub fn inbox(&self, key: &UserKey) -> Vec<Letter> {
        let mut mail = self.read(|tx| tx.letters().by_to(key));
        mail.reverse();
        mail
    }

    pub fn unread_mail(&self, key: &UserKey) -> usize {
        self.inbox(key).iter().filter(|l| !l.read).count()
    }

    pub fn letter(&self, id: LetterId) -> Option<Letter> {
        self.read(|tx| tx.letters().get(id))
    }

    /// Sends a letter; returns the recipient's handle.
    pub fn send_letter(&self, from: &UserKey, to: &UserKey, subject: &str, body: &str, now: i64) -> String {
        self.write(|tx| {
            tx.letters().create(|id| Letter {
                id,
                from: from.clone(),
                to: to.clone(),
                subject: subject.into(),
                body: body.into(),
                at: now,
                read: false,
            })?;
            Ok(handle_of(tx.users().get(to)?))
        })
    }

    pub fn mark_letter_read(&self, id: LetterId) {
        self.write(|tx| {
            if let Some(mut l) = tx.letters().get(id)? {
                l.read = true;
                tx.letters().update(&l)?;
            }
            Ok(())
        });
    }

    /// Deletes a letter, if it is addressed to `owner`.
    pub fn delete_letter(&self, id: LetterId, owner: &UserKey) {
        self.write(|tx| {
            if tx.letters().get(id)?.is_some_and(|l| l.to == *owner) {
                tx.letters().delete(id)?;
            }
            Ok(())
        });
    }

    // The rest.

    /// The one-liners, oldest first.
    pub fn oneliners(&self) -> Vec<Oneliner> {
        self.read(|tx| tx.oneliners().all())
    }

    pub fn add_oneliner(&self, author: &UserKey, text: &str, now: i64) {
        self.write(|tx| {
            tx.oneliners().create(|id| Oneliner {
                id,
                author: author.clone(),
                text: text.into(),
                at: now,
            })?;
            for old in tx.oneliners().all()?.iter().rev().skip(ONELINERS_KEPT as usize) {
                tx.oneliners().delete(old.id)?;
            }
            Ok(())
        });
    }

    /// The bulletins, oldest first.
    pub fn bulletins(&self) -> Vec<Bulletin> {
        self.read(|tx| tx.bulletins().all())
    }

    pub fn add_bulletin(&self, title: &str, body: &str, now: i64) {
        self.write(|tx| {
            tx.bulletins().create(|id| Bulletin {
                id,
                title: title.into(),
                body: body.into(),
                at: now,
            })
        });
    }

    pub fn delete_bulletin(&self, id: BulletinId) {
        self.write(|tx| tx.bulletins().delete(id));
    }

    pub fn stats(&self) -> Stats {
        self.read(|tx| {
            Ok(Stats {
                users: tx.users().len()?,
                topics: tx.topics().len()?,
                posts: tx.posts().len()?,
                letters: tx.letters().len()?,
            })
        })
    }
}

fn find_handle(users: &[User], handle: &str) -> Option<UserKey> {
    let handle = handle.trim();
    users
        .iter()
        .find(|u| u.handle.eq_ignore_ascii_case(handle))
        .map(|u| u.key.clone())
}

fn add_post(tx: &WriteTx, mut topic: Topic, author: &UserKey, body: &str, now: i64) -> orangedb::Result<PostId> {
    let post = tx.posts().create(|id| Post {
        id,
        topic: topic.id,
        author: author.clone(),
        at: now,
        body: body.into(),
    })?;
    topic.last_post = post.id;
    topic.last_at = now;
    tx.topics().update(&topic)?;
    if let Some(mut u) = tx.users().get(author)? {
        u.posts += 1;
        // Your own post is not news to you.
        u.last_read.insert(topic.id, post.id);
        tx.users().update(&u)?;
    }
    Ok(post.id)
}
