//! What the sessions share: the configuration, the database and the events
//! that keep every screen live.

use crate::db::{RoomId, Store};
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct Config {
    /// The server's name, shown at the top and in the renderer's title.
    pub name: String,
    /// Whether joining takes an invite.
    pub invite_only: bool,
}

/// Something changed that other sessions may want to show. Sessions work out
/// what it means for their screen from the database itself.
#[derive(Clone, Debug)]
pub enum Event {
    /// A message was posted in this room.
    Message { room: RoomId },
    /// A room was made.
    Rooms,
    /// Someone joined or changed their name or picture.
    Members,
}

pub struct Hub {
    pub config: Config,
    pub store: Store,
    events: broadcast::Sender<Event>,
}

impl Hub {
    pub fn new(config: Config, store: Store) -> Arc<Self> {
        Arc::new(Hub {
            config,
            store,
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
}
