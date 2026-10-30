//! The BBS's persistent state: users, message boards, mail, one-liners,
//! bulletins and the callers log. It lives in memory and is written to one
//! JSON file a moment after every change (see [`Store`]).
//!
//! Users are keyed by the hex of their iroh endpoint key, which the
//! connection has already proven, so there are no passwords.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::Notify;

/// How many callers the log keeps.
const CALLERS_KEPT: usize = 50;
/// How many one-liners the wall keeps.
const ONELINERS_KEPT: usize = 50;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Db {
    /// Every login ever, the "you are caller #N" counter.
    pub calls: u64,
    /// The last id handed out, shared by every kind of record.
    pub last_id: u64,
    pub users: BTreeMap<String, User>,
    pub boards: Vec<Board>,
    pub topics: Vec<Topic>,
    pub posts: Vec<Post>,
    pub mail: Vec<Letter>,
    pub oneliners: Vec<Oneliner>,
    pub bulletins: Vec<Bulletin>,
    /// The most recent logins, oldest first.
    pub callers: Vec<Caller>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
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
    pub last_read: BTreeMap<u64, u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: u64,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: u64,
    pub board: u64,
    pub subject: String,
    pub author: String,
    pub created: i64,
    /// The id of the newest post in it.
    pub last_post: u64,
    pub last_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: u64,
    pub topic: u64,
    pub author: String,
    pub at: i64,
    pub body: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Letter {
    pub id: u64,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub at: i64,
    pub read: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Oneliner {
    #[serde(default)]
    pub id: u64,
    pub author: String,
    pub text: String,
    pub at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bulletin {
    pub id: u64,
    pub title: String,
    pub body: String,
    pub at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Caller {
    pub key: String,
    pub node: u32,
    pub at: i64,
}

impl Db {
    /// A fresh BBS, with a few boards to start from.
    pub fn seeded() -> Self {
        let mut db = Db::default();
        for (name, description) in [
            ("General", "Anything goes. Be excellent to each other."),
            ("Tech Talk", "Hardware, software, protocols and hacks."),
            ("Trading Post", "Buy, sell, swap."),
        ] {
            db.add_board(name, description);
        }
        db
    }

    pub fn next_id(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }

    pub fn user(&self, key: &str) -> Option<&User> {
        self.users.get(key)
    }

    /// The handle of `key`, or a placeholder for someone who is gone.
    pub fn handle(&self, key: &str) -> String {
        self.users
            .get(key)
            .map_or_else(|| "<unknown>".into(), |u| u.handle.clone())
    }

    /// The key of the user with this handle, ignoring case.
    pub fn find_handle(&self, handle: &str) -> Option<&str> {
        let handle = handle.trim();
        self.users
            .iter()
            .find(|(_, u)| u.handle.eq_ignore_ascii_case(handle))
            .map(|(k, _)| k.as_str())
    }

    pub fn add_board(&mut self, name: &str, description: &str) -> u64 {
        let id = self.next_id();
        self.boards.push(Board {
            id,
            name: name.into(),
            description: description.into(),
        });
        id
    }

    pub fn board(&self, id: u64) -> Option<&Board> {
        self.boards.iter().find(|b| b.id == id)
    }

    pub fn topic(&self, id: u64) -> Option<&Topic> {
        self.topics.iter().find(|t| t.id == id)
    }

    /// The topics of a board, most recently active first.
    pub fn topics_of(&self, board: u64) -> Vec<&Topic> {
        let mut topics: Vec<_> = self.topics.iter().filter(|t| t.board == board).collect();
        topics.sort_by_key(|t| std::cmp::Reverse(t.last_post));
        topics
    }

    pub fn posts_of(&self, topic: u64) -> impl Iterator<Item = &Post> {
        self.posts.iter().filter(move |p| p.topic == topic)
    }

    /// Whether `key` has posts in this topic it has not read.
    pub fn is_unread(&self, key: &str, topic: &Topic) -> bool {
        let seen = self
            .user(key)
            .and_then(|u| u.last_read.get(&topic.id))
            .copied()
            .unwrap_or(0);
        topic.last_post > seen
    }

    pub fn unread_in(&self, key: &str, board: u64) -> usize {
        self.topics
            .iter()
            .filter(|t| t.board == board && self.is_unread(key, t))
            .count()
    }

    /// Starts a topic with its first post; returns the topic's id.
    pub fn add_topic(&mut self, board: u64, author: &str, subject: &str, body: &str, now: i64) -> u64 {
        let id = self.next_id();
        self.topics.push(Topic {
            id,
            board,
            subject: subject.into(),
            author: author.into(),
            created: now,
            last_post: 0,
            last_at: now,
        });
        self.add_post(id, author, body, now);
        id
    }

    pub fn add_post(&mut self, topic: u64, author: &str, body: &str, now: i64) -> u64 {
        let id = self.next_id();
        self.posts.push(Post {
            id,
            topic,
            author: author.into(),
            at: now,
            body: body.into(),
        });
        if let Some(t) = self.topics.iter_mut().find(|t| t.id == topic) {
            t.last_post = id;
            t.last_at = now;
        }
        if let Some(u) = self.users.get_mut(author) {
            u.posts += 1;
            // Your own post is not news to you.
            u.last_read.insert(topic, id);
        }
        id
    }

    /// Deletes a post, and its topic if it was the last one in it. Returns
    /// the topic's id and whether the topic is gone too.
    pub fn delete_post(&mut self, id: u64) -> Option<(u64, bool)> {
        let at = self.posts.iter().position(|p| p.id == id)?;
        let topic = self.posts.remove(at).topic;
        match self.posts_of(topic).map(|p| (p.id, p.at)).last() {
            Some((last, last_at)) => {
                if let Some(t) = self.topics.iter_mut().find(|t| t.id == topic) {
                    t.last_post = last;
                    t.last_at = last_at;
                }
                Some((topic, false))
            }
            None => {
                self.topics.retain(|t| t.id != topic);
                Some((topic, true))
            }
        }
    }

    pub fn delete_board(&mut self, id: u64) {
        let gone: Vec<u64> = self
            .topics
            .iter()
            .filter(|t| t.board == id)
            .map(|t| t.id)
            .collect();
        self.posts.retain(|p| !gone.contains(&p.topic));
        self.topics.retain(|t| t.board != id);
        self.boards.retain(|b| b.id != id);
    }

    pub fn inbox(&self, key: &str) -> Vec<&Letter> {
        let mut mail: Vec<_> = self.mail.iter().filter(|l| l.to == key).collect();
        mail.reverse();
        mail
    }

    pub fn unread_mail(&self, key: &str) -> usize {
        self.mail.iter().filter(|l| l.to == key && !l.read).count()
    }

    pub fn add_oneliner(&mut self, author: &str, text: &str, now: i64) {
        let id = self.next_id();
        self.oneliners.push(Oneliner {
            id,
            author: author.into(),
            text: text.into(),
            at: now,
        });
        let extra = self.oneliners.len().saturating_sub(ONELINERS_KEPT);
        self.oneliners.drain(..extra);
    }

    /// Logs a call; returns its caller number.
    pub fn log_call(&mut self, key: &str, node: u32, now: i64) -> u64 {
        self.calls += 1;
        self.callers.push(Caller {
            key: key.into(),
            node,
            at: now,
        });
        let extra = self.callers.len().saturating_sub(CALLERS_KEPT);
        self.callers.drain(..extra);
        if let Some(u) = self.users.get_mut(key) {
            u.calls += 1;
            u.last_on = now;
        }
        self.calls
    }
}

/// The database and the file it is saved to.
pub struct Store {
    path: PathBuf,
    db: Mutex<Db>,
    dirty: Notify,
}

impl Store {
    /// Loads the database at `path`, or seeds a new one if there is no file.
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Db::seeded(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Store {
            path: path.into(),
            db: Mutex::new(db),
            dirty: Notify::new(),
        })
    }

    /// An unsaved database, for tests.
    #[cfg(test)]
    pub fn memory() -> Self {
        Store {
            path: PathBuf::new(),
            db: Mutex::new(Db::seeded()),
            dirty: Notify::new(),
        }
    }

    /// Reads the database.
    pub fn get(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap()
    }

    /// Changes the database; it is saved shortly after.
    pub fn edit<R>(&self, f: impl FnOnce(&mut Db) -> R) -> R {
        let r = f(&mut self.db.lock().unwrap());
        self.dirty.notify_one();
        r
    }

    /// Writes the database whenever it changes, at most once a second.
    pub async fn keep_saved(&self) {
        loop {
            self.dirty.notified().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Err(e) = self.save() {
                eprintln!("saving {}: {e}", self.path.display());
            }
        }
    }

    /// Writes the database now, through a temporary file so that a crash
    /// never leaves half of it.
    pub fn save(&self) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&*self.get())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}
