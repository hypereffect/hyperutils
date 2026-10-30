//! What the sessions share: the database, the nodes (the "phone lines" in
//! use), the teleconference and the events that keep every screen live.

use crate::db::{Store, UserKey};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// How many teleconference lines a newcomer sees.
const CHAT_KEPT: usize = 100;

pub struct Config {
    /// The board's name, shown in the banner and the renderer's title.
    pub name: String,
    /// How many callers can be on at once.
    pub nodes: u32,
    /// Keys that are always sysops, besides the first user to register.
    pub sysops: Vec<String>,
    /// The login screen's art.
    pub banner: String,
}

/// Something happened that other sessions may want to show.
#[derive(Clone, Debug)]
pub enum Event {
    /// The teleconference has a new line (see [`Hub::chat_history`]).
    Chat,
    /// Someone came, left, or moved to another screen.
    Nodes,
    /// Something in the database changed. Sessions work out what it means
    /// for their screen from the database itself.
    Changed,
    /// A letter arrived for this key.
    Mail { to: UserKey },
    /// A user on `node` asks the user on `to` to join the teleconference.
    Page { to: u32, from: String },
    /// The sysop banned this key.
    Kick { key: UserKey },
}

#[derive(Clone, Debug)]
pub struct ChatLine {
    pub n: u64,
    pub at: i64,
    pub kind: ChatKind,
    pub who: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChatKind {
    Say,
    Emote,
    /// Someone entered or left.
    System,
}

/// A caller on a line.
#[derive(Clone)]
pub struct Node {
    pub key: UserKey,
    /// Empty until a new user has registered.
    pub handle: String,
    pub since: i64,
    pub doing: &'static str,
}

pub struct Hub {
    pub config: Config,
    pub store: Store,
    pub nodes: Mutex<BTreeMap<u32, Node>>,
    chat: Mutex<(u64, VecDeque<ChatLine>)>,
    events: broadcast::Sender<Event>,
}

impl Hub {
    pub fn new(config: Config, store: Store) -> Arc<Self> {
        Arc::new(Hub {
            config,
            store,
            nodes: Mutex::default(),
            chat: Mutex::default(),
            events: broadcast::channel(256).0,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn emit(&self, e: Event) {
        // No receivers is fine: nobody else is on.
        let _ = self.events.send(e);
    }

    /// Takes the lowest free line, if any.
    pub fn take_node(self: &Arc<Self>, key: &UserKey, now: i64) -> Option<NodeGuard> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = (1..=self.config.nodes).find(|n| !nodes.contains_key(n))?;
        nodes.insert(
            n,
            Node {
                key: key.clone(),
                handle: String::new(),
                since: now,
                doing: "Logging in",
            },
        );
        drop(nodes);
        self.emit(Event::Nodes);
        Some(NodeGuard {
            hub: self.clone(),
            n,
        })
    }

    pub fn update_node(&self, n: u32, f: impl FnOnce(&mut Node)) {
        if let Some(node) = self.nodes.lock().unwrap().get_mut(&n) {
            f(node);
        }
        self.emit(Event::Nodes);
    }

    pub fn online(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    pub fn is_sysop(&self, key: &UserKey) -> bool {
        self.config.sysops.contains(&key.0) || self.store.user(key).is_some_and(|u| u.sysop)
    }

    /// Says something in the teleconference.
    pub fn chat(&self, kind: ChatKind, who: &str, text: &str, now: i64) {
        {
            let mut chat = self.chat.lock().unwrap();
            chat.0 += 1;
            let line = ChatLine {
                n: chat.0,
                at: now,
                kind,
                who: who.into(),
                text: text.into(),
            };
            chat.1.push_back(line);
            if chat.1.len() > CHAT_KEPT {
                chat.1.pop_front();
            }
        }
        self.emit(Event::Chat);
    }

    pub fn chat_history(&self) -> Vec<ChatLine> {
        self.chat.lock().unwrap().1.iter().cloned().collect()
    }
}

/// A line in use; hangs up when dropped.
pub struct NodeGuard {
    hub: Arc<Hub>,
    pub n: u32,
}

impl Drop for NodeGuard {
    fn drop(&mut self) {
        self.hub.nodes.lock().unwrap().remove(&self.n);
        self.hub.emit(Event::Nodes);
    }
}
