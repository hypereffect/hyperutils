//! Sessions driven the way the transport drives them, against hypereffect's
//! renderer core, which also checks every element and expression.

use super::*;
use crate::db::Store;
use crate::hub::Config;
use hypereffect::RendererMessage;
use hypereffect::client::Client;
use tokio::sync::broadcast;

fn hub(invite_only: bool) -> Arc<Hub> {
    Hub::new(
        Config {
            name: "Test chat".into(),
            invite_only,
        },
        Store::memory(),
    )
}

/// A key as the transport writes one: 64 hex digits.
fn key(n: u32) -> String {
    format!("{n:064x}")
}

struct Caller {
    chat: Chat,
    client: Client,
    events: broadcast::Receiver<Event>,
    out: Outbox,
    /// The lines of the last batch the renderer got.
    last: Vec<String>,
}

impl Caller {
    fn dial(hub: &Arc<Hub>, key: &str) -> Self {
        let mut c = Caller {
            events: hub.subscribe(),
            chat: Chat::new(hub.clone(), MemberKey::new(key)),
            client: Client::new(),
            out: Outbox::new(),
            last: Vec::new(),
        };
        let hello = c.client.hello();
        c.send(&hello);
        c
    }

    /// A new caller who joins as `name`.
    fn join(hub: &Arc<Hub>, key: &str, name: &str) -> Self {
        let mut c = Caller::dial(hub, key);
        c.client.write("f.name", json!(name));
        assert!(c.press("join"));
        assert!(c.shows("Rooms"), "{}", c.screen());
        c
    }

    fn send(&mut self, line: &str) {
        match RendererMessage::parse(line.trim_end()).unwrap() {
            RendererMessage::Hello { params } => self.chat.hello(&mut self.out, &params),
            RendererMessage::Action(a) => self.chat.action(&mut self.out, &a),
        }
        self.pump();
    }

    /// Delivers the other sessions' events and the output.
    fn pump(&mut self) {
        while let Ok(e) = self.events.try_recv() {
            self.chat.event(&mut self.out, &e);
        }
        let batch = self.out.take();
        self.last = batch.lines().map(str::to_owned).collect();
        self.client.receive(batch.as_bytes());
        // Resolving the tree reports bad attributes.
        let _ = self.client.tree();
        let diagnostics = self.client.take_diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    fn press(&mut self, id: &str) -> bool {
        let Some(line) = self.client.trigger(id, "onPress") else {
            return false;
        };
        self.send(&line);
        true
    }

    /// Sends the action `name` with `payload`, whether or not a button for it
    /// is shown.
    fn forge(&mut self, name: &str, payload: Value) {
        let line = format!(
            "action {}\n",
            json!({"name": name, "payload": payload, "signals": {"f": self.value("f")}})
        );
        self.send(&line);
    }

    fn value(&self, path: &str) -> Option<Value> {
        self.client.value(path).cloned()
    }

    fn say(&mut self, text: &str) {
        self.client.write("f.msg", json!(text));
        assert!(self.press("send"));
    }

    /// The visible words on screen.
    fn screen(&self) -> String {
        let mut words = Vec::new();
        self.collect(None, &mut words);
        words.join("\n")
    }

    fn collect(&self, id: Option<&str>, words: &mut Vec<String>) {
        for child in self.client.children(id) {
            if self.client.attr(child, "visible") == Some(json!(false))
                || self.client.attr(child, "open") == Some(json!(false))
            {
                continue;
            }
            for attr in ["title", "text", "label"] {
                if let Some(Value::String(s)) = self.client.attr(child, attr)
                    && !s.is_empty()
                {
                    words.push(s);
                }
            }
            self.collect(Some(child), words);
        }
    }

    fn shows(&self, text: &str) -> bool {
        self.screen().contains(text)
    }

    fn visible(&self, id: &str) -> bool {
        self.client.get(id).is_some() && self.client.attr(id, "visible") != Some(json!(false))
    }

    fn flash(&self) -> String {
        match self.client.attr("flash", "text") {
            Some(Value::String(s)) => s,
            _ => String::new(),
        }
    }

    /// The ids of the elements the last batch sent.
    fn sent(&self) -> Vec<String> {
        self.last
            .iter()
            .filter_map(|l| l.strip_prefix("element "))
            .map(|json| serde_json::from_str::<Value>(json).unwrap()["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn messages(&self) -> Vec<String> {
        self.client
            .children(Some("msgs"))
            .iter()
            .map(|m| match self.client.attr(&format!("{m}_text"), "text") {
                Some(Value::String(s)) => s,
                other => panic!("{other:?}"),
            })
            .collect()
    }
}

#[test]
fn people_join_and_talk() {
    let hub = hub(false);
    let mut ann = Caller::join(&hub, &key(1), "Ann");
    let mut bob = Caller::join(&hub, &key(2), "Bob");
    assert!(ann.shows("# general"));
    assert!(ann.messages().is_empty());

    ann.say("hi Bob");
    assert_eq!(ann.value("f.msg"), Some(json!("")));
    bob.pump();
    assert_eq!(bob.messages(), ["hi Bob"]);
    assert!(bob.shows("Ann"));
    // Ann's own echo from the hub has nothing left to send.
    ann.pump();
    assert!(ann.last.is_empty(), "{:?}", ann.last);

    bob.say("hello");
    ann.pump();
    assert_eq!(ann.messages(), ["hi Bob", "hello"]);
}

#[test]
fn a_known_key_goes_straight_in() {
    let hub = hub(false);
    drop(Caller::join(&hub, &key(1), "Ann"));
    let again = Caller::dial(&hub, &key(1));
    assert!(again.shows("Rooms"));
    assert!(!again.shows("Join Test chat"));
}

#[test]
fn names_and_pictures() {
    let hub = hub(false);
    let mut c = Caller::dial(&hub, &key(1));
    c.client.write("f.name", json!("Ann"));
    c.client.write("f.picture", json!("javascript:alert(1)"));
    c.press("join");
    assert!(c.flash().contains("A picture is"), "{}", c.flash());
    c.client.write("f.picture", json!(""));
    c.press("join");
    let me = format!("k{}", key(1));
    let pic = c.value(&format!("_who.{me}.pic")).unwrap();
    assert!(pic.as_str().unwrap().starts_with("data:image/png;base64,"));

    let mut bob = Caller::join(&hub, &key(2), "Bob");
    c.say("one");
    c.say("two");
    bob.pump();

    // A new name is one signal, not every message again.
    assert!(c.press("profile"));
    assert_eq!(c.value("f.name"), Some(json!("Ann")));
    c.client.write("f.name", json!("Annie"));
    c.client.write("f.picture", json!("data:image/png;base64,AAAA"));
    assert!(c.press("save_profile"));
    assert_eq!(c.value("_dlg"), Some(json!("")));
    bob.pump();
    assert!(bob.sent().is_empty(), "{:?}", bob.sent());
    assert_eq!(
        bob.value(&format!("_who.{me}")),
        Some(json!({"name": "Annie", "pic": "data:image/png;base64,AAAA"}))
    );
    assert!(bob.shows("Annie"));
}

#[test]
fn invites() {
    let hub = hub(true);
    let first = hub.store.add_invite(None, Some(1), 0);

    let mut ann = Caller::dial(&hub, &key(1));
    assert!(ann.visible("join_invite"));
    ann.client.write("f.name", json!("Ann"));
    ann.press("join");
    assert_eq!(ann.flash(), "Type your invite first.");
    ann.client.write("f.invite", json!("nope"));
    ann.press("join");
    assert_eq!(ann.flash(), "That invite is unknown or used up.");
    // Nothing but joining works before.
    ann.forge("create_room", json!(null));
    assert_eq!(hub.store.rooms().len(), 1);
    ann.client.write("f.invite", json!(first));
    ann.press("join");
    assert!(ann.shows("Rooms"), "{}", ann.screen());

    // Used up.
    let mut eve = Caller::dial(&hub, &key(9));
    eve.client.write("f.name", json!("Eve"));
    eve.client.write("f.invite", json!(first));
    eve.press("join");
    assert_eq!(eve.flash(), "That invite is unknown or used up.");

    // Ann makes one for anyone.
    // The dialog opens without a round trip.
    assert!(!ann.press("invite"));
    assert_eq!(ann.value("_dlg"), Some(json!("invite")));
    ann.client.write("f.uses", json!("0"));
    assert!(ann.press("make_invite"));
    let code = ann.value("_invite").unwrap().as_str().unwrap().to_owned();
    assert_eq!(code.len(), 10);
    assert!(ann.shows(&code));
    for n in 2..5 {
        let mut c = Caller::dial(&hub, &key(n));
        c.client.write("f.name", json!(format!("Guest {n}")));
        c.client.write("f.invite", json!(code));
        c.press("join");
        assert!(c.shows("Rooms"));
    }
}

#[test]
fn open_servers_have_no_invites() {
    let hub = hub(false);
    let mut ann = Caller::join(&hub, &key(1), "Ann");
    assert!(!ann.visible("join_invite"));
    assert!(!ann.visible("invite"));
    ann.forge("make_invite", json!(null));
    assert!(hub.store.usable_invite().is_none());
}

#[test]
fn rooms() {
    let hub = hub(false);
    let mut ann = Caller::join(&hub, &key(1), "Ann");
    let mut bob = Caller::join(&hub, &key(2), "Bob");
    ann.client.write("f.room", json!("#Random "));
    assert!(ann.press("create_room"));
    assert_eq!(ann.value("f.room"), Some(json!("")));
    assert!(ann.shows("# Random"));
    // The new room is open, and highlighted.
    assert_eq!(ann.value("_room_name"), Some(json!("Random")));
    let random = ann.client.children(Some("rooms"))[1].clone();
    assert_eq!(ann.client.attr(&random, "variant"), Some(json!("primary")));
    assert_eq!(ann.client.attr("r1", "variant"), Some(json!("plain")));

    bob.pump();
    assert_eq!(bob.client.children(Some("rooms")).len(), 2);
    assert_eq!(bob.value("_room_name"), Some(json!("general")));

    ann.client.write("f.room", json!("random"));
    ann.press("create_room");
    assert_eq!(ann.flash(), "There is a room by that name already.");

    // Messages stay in their room.
    ann.say("in random");
    bob.pump();
    assert!(bob.messages().is_empty());
    assert!(bob.press(&random));
    assert_eq!(bob.messages(), ["in random"]);
    assert!(bob.press("r1"));
    assert!(bob.messages().is_empty());
    bob.forge("open", json!(99));
    assert_eq!(bob.flash(), "That room is gone.");
}

#[test]
fn older_messages() {
    let hub = hub(false);
    let ann = Caller::join(&hub, &key(1), "Ann");
    let general = hub.store.rooms()[0].id;
    for n in 0..250 {
        hub.store.post(general, &ann.chat.key, &format!("{n}"), 0);
    }
    let mut bob = Caller::join(&hub, &key(2), "Bob");
    let want = |from: usize| (from..250).map(|n| n.to_string()).collect::<Vec<_>>();
    assert_eq!(bob.messages(), want(150));
    assert!(bob.visible("older"));

    assert!(bob.press("older"));
    assert_eq!(bob.messages(), want(50));
    // Only the older ones were sent.
    assert_eq!(bob.sent().iter().filter(|id| id.ends_with("_text")).count(), 100);
    assert!(bob.press("older"));
    assert_eq!(bob.messages(), want(0));
    assert!(!bob.visible("older"));

    // Going to another room and back starts from the latest again.
    bob.client.write("f.room", json!("other"));
    bob.press("create_room");
    bob.press("r1");
    assert_eq!(bob.messages().len(), PAGE);
}

#[test]
fn limits() {
    let hub = hub(false);
    let mut ann = Caller::join(&hub, &key(1), "Ann");
    ann.say(&"x".repeat(MAX_MESSAGE + 1));
    assert!(ann.flash().contains("At most"));
    assert!(ann.messages().is_empty());
    ann.say("   ");
    assert!(ann.messages().is_empty());
    assert!(ann.press("profile"));
    ann.client.write("f.name", json!(""));
    ann.press("save_profile");
    assert_eq!(ann.flash(), "Pick a name first.");
    assert_eq!(ann.value("_dlg"), Some(json!("profile")));
}
