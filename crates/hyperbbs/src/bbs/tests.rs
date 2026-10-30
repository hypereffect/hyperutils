//! Sessions driven the way the transport drives them, against hypereffect's
//! renderer core, which also checks every element and expression.

use super::*;
use crate::db::Store;
use crate::hub::Config;
use hypereffect::client::Client;
use hypereffect::RendererMessage;
use tokio::sync::broadcast;

fn hub(nodes: u32) -> Arc<Hub> {
    Hub::new(
        Config {
            name: "Test BBS".into(),
            nodes,
            sysops: vec![],
            banner: default_banner("Test BBS"),
        },
        Store::memory(),
    )
}

struct Caller {
    bbs: Bbs,
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
            bbs: Bbs::new(hub.clone(), key.into()),
            client: Client::new(),
            out: Outbox::new(),
            last: Vec::new(),
        };
        let hello = c.client.hello();
        c.send(&hello);
        c
    }

    /// A new caller who registers as `handle`.
    fn join(hub: &Arc<Hub>, key: &str, handle: &str) -> Self {
        let mut c = Caller::dial(hub, key);
        c.client.write("f.handle", json!(handle));
        c.client.write("f.location", json!("Testville"));
        assert!(c.press("register"));
        assert!(c.press("continue"), "{}", c.screen());
        c
    }

    fn send(&mut self, line: &str) {
        match RendererMessage::parse(line.trim_end()).unwrap() {
            RendererMessage::Hello { params } => self.bbs.hello(&mut self.out, &params),
            RendererMessage::Action(a) => self.bbs.action(&mut self.out, &a),
        }
        self.pump();
    }

    /// Delivers the other sessions' events and the output.
    fn pump(&mut self) {
        while let Ok(e) = self.events.try_recv() {
            self.bbs.event(&mut self.out, &e);
        }
        let batch = self.out.take();
        self.last = batch.lines().map(str::to_owned).collect();
        self.client.receive(batch.as_bytes());
        // Resolving the tree reports bad attributes.
        let _ = self.client.tree();
        let diagnostics = self.client.take_diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    fn trigger(&mut self, id: &str, event: &str) -> bool {
        let Some(line) = self.client.trigger(id, event) else {
            return false;
        };
        self.send(&line);
        true
    }

    fn tick(&mut self) {
        self.bbs.tick(&mut self.out);
        self.pump();
    }

    fn press(&mut self, id: &str) -> bool {
        self.trigger(id, "onPress")
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
                if let Some(Value::String(s)) = self.client.attr(child, attr) {
                    if !s.is_empty() {
                        words.push(s);
                    }
                }
            }
            self.collect(Some(child), words);
        }
    }

    fn shows(&self, text: &str) -> bool {
        self.screen().contains(text)
    }

    /// The ids of the elements the last batch sent, and of those it removed.
    fn touched(&self) -> (Vec<String>, Vec<String>) {
        let id = |json: &str| serde_json::from_str::<Value>(json).unwrap()["id"].as_str().unwrap().to_owned();
        let mut sent = Vec::new();
        let mut removed = Vec::new();
        for line in &self.last {
            if let Some(json) = line.strip_prefix("element ") {
                sent.push(id(json));
            } else if let Some(json) = line.strip_prefix("remove ") {
                removed.push(id(json));
            }
        }
        (sent, removed)
    }

    fn flash(&self) -> String {
        self.client
            .value("_flash.text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

const ALICE: &str = "aa";
const BOB: &str = "bb";

#[test]
fn first_caller_registers_and_becomes_sysop() {
    let hub = hub(4);
    let mut a = Caller::dial(&hub, ALICE);
    assert!(a.shows("New user application"), "{}", a.screen());
    // A bad handle is refused.
    a.client.write("f.handle", json!("x"));
    assert!(a.press("register"));
    assert!(a.flash().contains("2 to 20"), "{}", a.flash());
    a.client.write("f.handle", json!("Alice"));
    assert!(a.press("register"));
    assert!(a.shows("Welcome, Alice!"), "{}", a.screen());
    assert!(a.shows("You are caller #1, on node 1."));
    assert!(a.shows("You are a sysop here."));
    assert!(a.press("continue"));
    assert!(a.shows("Sysop console"), "{}", a.screen());
    assert_eq!(a.client.value("_me"), Some(&json!("Alice")));

    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(!b.shows("Sysop console"));
    // Asking for the sysop console anyway does nothing.
    let line = format!(
        "action {}\n",
        json!({"name": "go", "payload": {"screen": "sysop"}})
    );
    b.send(&line);
    assert!(b.shows("Main Menu"));
    // Handles are unique, whatever the case.
    let mut c = Caller::dial(&hub, "cc");
    c.client.write("f.handle", json!("ALICE"));
    assert!(c.press("register"));
    assert!(c.flash().contains("taken"));
}

#[test]
fn returning_caller_is_welcomed_back() {
    let hub = hub(4);
    drop(Caller::join(&hub, ALICE, "Alice"));
    let a = Caller::dial(&hub, ALICE);
    assert!(a.shows("Welcome back, Alice!"), "{}", a.screen());
    assert!(a.shows("You are caller #2"));
    assert!(a.shows("This is your call #2."));
    assert!(a.shows("Last caller: Alice from Testville."));
}

#[test]
fn every_screen_draws() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    for (item, title) in [
        ("m_boards", "Message Boards"),
        ("m_mail", "E-mail"),
        ("m_bull", "Bulletins"),
        ("m_chat", "Teleconference"),
        ("m_who", "Who's Online"),
        ("m_one", "One-liner Wall"),
        ("m_users", "User List (1)"),
        ("m_last", "Last Callers"),
        ("m_prof", "Your Profile"),
        ("m_sysop", "Sysop Console"),
    ] {
        assert!(a.press(item), "{item}");
        assert!(a.shows(title), "{item}: {}", a.screen());
        // Drawn once: bringing it up to date again sends nothing.
        a.bbs.refresh(&mut a.out);
        assert_eq!(a.out.take(), "", "{item}");
        assert!(a.press("back") || a.press("prof_back"), "{item}");
        assert!(a.shows("Main Menu"), "{item}");
    }
    assert!(a.press("m_bye"));
    assert!(a.shows("NO CARRIER"));
    assert!(a.bbs.hung_up());
}

#[test]
fn topics_and_replies_are_live() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    let board = hub.store.get().boards[0].id;
    assert!(a.press("m_boards"));
    assert!(a.press(&format!("b{board}_go")));
    assert!(a.press("new_topic"));
    a.client.write("f.subject", json!("First!"));
    a.client.write("f.body", json!("Hello, board."));
    assert!(a.press("nt_post"));
    assert!(a.shows("Hello, board."), "{}", a.screen());

    // Bob sees it is new, reads it and replies.
    assert!(b.press("m_boards"));
    assert!(b.shows("1 new"), "{}", b.screen());
    assert!(b.press(&format!("b{board}_go")));
    let topic = hub.store.get().topics[0].id;
    assert!(b.press(&format!("t{topic}_go")));
    b.client.write("f.body", json!("Welcome, Alice."));
    assert!(b.press("reply"));
    assert_eq!(b.client.value("f.body"), Some(&json!("")));
    // Alice was looking at the topic, and the reply showed up there.
    a.pump();
    assert!(a.shows("Welcome, Alice."), "{}", a.screen());
    {
        let db = hub.store.get();
        assert!(!db.is_unread(BOB, &db.topics[0]));
    }
    // Bob cannot delete Alice's post, the sysop can delete his.
    let first = hub.store.get().posts[0].id;
    let second = hub.store.get().posts[1].id;
    assert!(!b.client.contains(&format!("p{first}_del")));
    assert!(a.press(&format!("p{second}_del")));
    assert_eq!(hub.store.get().posts.len(), 1);
    assert_eq!(hub.store.get().user(BOB).unwrap().posts, 1);
}

#[test]
fn teleconference() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(a.press("m_chat"));
    assert!(b.press("m_chat"));
    a.pump();
    assert!(a.shows("*** Bob has entered the teleconference"), "{}", a.screen());
    assert!(a.shows("Here: Alice, Bob"));
    b.client.write("f.chat", json!("hi there"));
    assert!(b.trigger("chat_input", "onSubmit"));
    b.client.write("f.chat", json!("/me waves"));
    assert!(b.press("chat_send"));
    a.pump();
    assert!(a.shows("<Bob> hi there"));
    assert!(a.shows("* Bob waves"));
    // Newest first.
    let log = a.client.children(Some("chatlog")).to_vec();
    assert!(a.client.attr(&log[0], "text").unwrap().as_str().unwrap().ends_with("* Bob waves"));
    drop(b);
    a.pump();
    assert!(a.shows("*** Bob dropped carrier"));
    assert!(a.shows("Here: Alice"));
}

#[test]
fn paging_brings_a_caller_to_chat() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(a.press("m_who"));
    assert!(a.shows("Bob from Testville"), "{}", a.screen());
    assert_eq!(a.client.attr("n1_page", "visible"), Some(json!(false)), "no paging yourself");
    assert!(!a.press("n1_page"));
    assert!(a.press("n2_page"));
    b.pump();
    assert!(b.shows("Alice is paging you"));
    assert!(b.press("page_yes"));
    // The dialog closed itself, with a local patch.
    assert_eq!(b.client.attr("pagedlg", "open"), Some(json!(false)));
    assert!(b.shows("Teleconference"));
}

#[test]
fn mail() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(b.press("m_mail"));
    assert!(b.press("mail_new"));
    b.client.write("f.to", json!("nobody"));
    assert!(b.press("compose_send"));
    assert!(b.flash().contains("Nobody here"));
    b.client.write("f.to", json!("alice"));
    b.client.write("f.subject", json!("Lunch"));
    b.client.write("f.body", json!("Pizza?"));
    assert!(b.press("compose_send"));
    assert!(b.flash().contains("on its way to Alice"));
    a.pump();
    assert_eq!(a.client.value("_mail"), Some(&json!(1)));
    assert!(a.shows("1 new letter"));
    assert!(a.press("m_mail"));
    let id = hub.store.get().mail[0].id;
    assert!(a.press(&format!("l{id}_go")));
    assert!(a.shows("Pizza?"));
    assert_eq!(a.client.value("_mail"), Some(&json!(0)));
    // Replying fills in the recipient and subject.
    assert!(a.press("letter_reply"));
    assert_eq!(a.client.value("f.to"), Some(&json!("Bob")));
    assert_eq!(a.client.value("f.subject"), Some(&json!("Re: Lunch")));
    // Bob cannot read or delete Alice's letter.
    b.send(&format!("action {}\n", json!({"name": "go", "payload": {"screen": "letter", "id": id}})));
    assert!(b.shows("This letter is gone."));
    b.send(&format!("action {}\n", json!({"name": "delete_mail", "payload": id})));
    assert_eq!(hub.store.get().mail.len(), 1);
}

#[test]
fn oneliners() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(a.press("m_one"));
    a.client.write("f.oneliner", json!("kilroy was here"));
    assert!(a.press("one_send"));
    assert!(a.shows("kilroy was here"));
    // Bob is at the main menu, which shows the latest ones.
    b.pump();
    assert!(b.shows("kilroy was here"), "{}", b.screen());
}

#[test]
fn bans_kick_and_keep_out() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    assert!(a.press("m_users"));
    assert!(a.press("ubb_ban"));
    // The reply confirms it: the unban button is the one shown now.
    assert_eq!(a.client.attr("ubb_ban", "visible"), Some(json!(false)));
    assert!(a.shows("BANNED"));
    b.pump();
    assert!(b.bbs.hung_up());
    assert!(b.shows("The sysop has banned this key."));
    drop(b);
    let b = Caller::dial(&hub, BOB);
    assert!(b.bbs.hung_up());
    assert!(b.shows("Access denied"));
    // Only sysops ban.
    let mut c = Caller::join(&hub, "cc", "Carol");
    c.send(&format!("action {}\n", json!({"name": "unban", "payload": BOB})));
    assert!(hub.store.get().user(BOB).unwrap().banned);
    drop(c);
    assert!(a.press("ubb_unban"));
    assert!(!hub.store.get().user(BOB).unwrap().banned);
}

#[test]
fn lines_run_out() {
    let hub = hub(1);
    let _a = Caller::join(&hub, ALICE, "Alice");
    let b = Caller::dial(&hub, BOB);
    assert!(b.shows("All lines are busy"));
    assert!(b.bbs.hung_up());
}

#[test]
fn database_round_trips() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    assert!(a.press("m_one"));
    a.client.write("f.oneliner", json!("persist me"));
    assert!(a.press("one_send"));
    let json = serde_json::to_string(&*hub.store.get()).unwrap();
    let db: Db = serde_json::from_str(&json).unwrap();
    assert_eq!(db.user(ALICE).unwrap().handle, "Alice");
    assert_eq!(db.oneliners[0].text, "persist me");
    assert_eq!(db.boards.len(), 3);
}

// The server guide's rules: build once, then change as little as possible.

/// Starts a topic on the first board as `c`, who must be at the main menu.
fn start_topic(hub: &Arc<Hub>, c: &mut Caller, subject: &str) -> u64 {
    let board = hub.store.get().boards[0].id;
    assert!(c.press("m_boards"));
    assert!(c.press(&format!("b{board}_go")));
    assert!(c.press("new_topic"));
    c.client.write("f.subject", json!(subject));
    c.client.write("f.body", json!("…"));
    assert!(c.press("nt_post"));
    hub.store.get().topics.last().unwrap().id
}

#[test]
fn a_reply_adds_one_card_and_keeps_the_draft() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    let topic = start_topic(&hub, &mut a, "Drafts");
    let board = hub.store.get().boards[0].id;
    assert!(b.press("m_boards"));
    assert!(b.press(&format!("b{board}_go")));
    assert!(b.press(&format!("t{topic}_go")));
    b.client.write("f.body", json!("half a thought"));

    a.client.write("f.body", json!("A reply."));
    assert!(a.press("reply"));
    let post = hub.store.get().posts.last().unwrap().id;
    // Alice's own reply already shows her card, in the reply itself.
    assert!(a.shows("A reply."));
    b.pump();
    let (sent, removed) = b.touched();
    assert!(removed.is_empty(), "{:?}", b.last);
    assert!(!sent.is_empty() && sent.iter().all(|id| id.starts_with(&format!("p{post}"))), "{sent:?}");
    assert!(b.shows("A reply."));
    assert_eq!(b.client.value("f.body"), Some(&json!("half a thought")));
}

#[test]
fn a_reply_moves_its_topic_to_the_top_with_one_element() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    let mut b = Caller::join(&hub, BOB, "Bob");
    let old = start_topic(&hub, &mut a, "Old");
    assert!(a.press("back"));
    assert!(a.press("new_topic"));
    a.client.write("f.subject", json!("New"));
    a.client.write("f.body", json!("…"));
    assert!(a.press("nt_post"));
    let new = hub.store.get().topics.last().unwrap().id;
    let board = hub.store.get().boards[0].id;
    assert!(b.press("m_boards"));
    assert!(b.press(&format!("b{board}_go")));
    assert_eq!(b.client.children(Some("topics")), [format!("t{new}"), format!("t{old}")]);

    hub.store.edit(|db| db.add_post(old, ALICE, "bump", now()));
    hub.emit(Event::Changed);
    b.pump();
    // The row moves (its children with it); the caption is a signal.
    assert_eq!(b.touched(), (vec![format!("t{old}")], vec![]), "{:?}", b.last);
    assert_eq!(b.client.children(Some("topics")), [format!("t{old}"), format!("t{new}")]);
    assert!(b.shows("by Alice · 1 reply"), "{}", b.screen());
}

#[test]
fn who_is_online_changes_by_signals() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    assert!(a.press("m_who"));
    let mut b = Caller::join(&hub, BOB, "Bob");
    a.pump();
    assert_eq!(a.client.children(Some("who")), ["n1", "n2"]);
    assert!(a.shows("Bob from Testville"));
    // Bob wanders about: only Alice's signals change.
    assert!(b.press("m_boards"));
    a.pump();
    assert_eq!(a.touched(), (vec![], vec![]), "{:?}", a.last);
    assert!(a.shows("Reading messages"));
    drop(b);
    a.pump();
    assert_eq!(a.touched(), (vec![], vec!["n2".to_owned()]));
}

#[test]
fn requests_are_busy_until_the_reply() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    start_topic(&hub, &mut a, "Busy");
    // Even a refused request clears the indicator.
    let line = a.client.trigger("reply", "onPress").unwrap();
    assert_eq!(a.client.value("_busy"), Some(&json!(true)));
    assert_eq!(a.client.attr("reply", "disabled"), Some(json!(true)));
    assert!(a.client.trigger("reply", "onPress").is_none(), "no second request");
    a.send(&line);
    assert_eq!(a.client.value("_busy"), Some(&json!(false)));
    assert!(a.flash().contains("Type a reply first"));
}

#[test]
fn actions_carry_only_the_forms() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    a.tick();
    assert!(a.press("m_one"));
    a.client.write("f.oneliner", json!("hi"));
    let line = a.client.trigger("one_send", "onPress").unwrap();
    let action: Action = serde_json::from_str(line.strip_prefix("action ").unwrap()).unwrap();
    assert_eq!(action.signals.keys().collect::<Vec<_>>(), ["f"]);
}

#[test]
fn the_clock_sends_only_what_changed() {
    let hub = hub(4);
    let mut a = Caller::join(&hub, ALICE, "Alice");
    a.tick();
    a.tick();
    // Within one second, nothing at all; otherwise only the clock.
    assert!(a.last.iter().all(|l| l.starts_with("signal ") && !l.contains("_unread")), "{:?}", a.last);
}
