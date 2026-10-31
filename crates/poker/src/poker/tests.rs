use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Poker,
    out: Outbox,
}

impl Loopback {
    fn receive(&mut self, line: &str) {
        match RendererMessage::parse(line.trim_end()) {
            Ok(RendererMessage::Hello { params }) => self.app.hello(&mut self.out, &params),
            Ok(RendererMessage::Action(a)) => self.app.action(&mut self.out, &a),
            Err(e) => panic!("{e:?}"),
        }
        self.pump();
    }

    fn pump(&mut self) {
        let out = self.out.take();
        self.client.receive(out.as_bytes());
    }

    fn press(&mut self, id: &str) {
        let line = self
            .client
            .trigger(id, "onPress")
            .unwrap_or_else(|| panic!("{id} cannot be pressed"));
        self.receive(&line);
    }

    /// Sends the action `name`, whether or not a button for it is shown.
    fn forge(&mut self, name: &str) {
        let line = self
            .client
            .trigger("home_btn", "onPress")
            .unwrap()
            .replace("\"home\"", &format!("\"{name}\""));
        self.receive(&line);
    }

    fn write(&mut self, path: &str, value: Value) {
        self.client.write(path, value);
    }

    fn tick(&mut self) {
        self.app.tick(&mut self.out, 0.0);
        self.pump();
    }

    fn view(&self) -> &str {
        self.client.value("view").unwrap().as_str().unwrap()
    }

    fn feedback(&self) -> Value {
        self.client.attr("feedback", "text").unwrap()
    }

    fn visible(&self, id: &str) -> bool {
        self.client.attr(id, "visible") == Some(json!(true))
    }

    fn text(&self, id: &str) -> String {
        self.client
            .attr(id, "text")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn value(&self, path: &str) -> Value {
        self.client.value(path).cloned().unwrap_or(Value::Null)
    }

    /// Each player row as “name: badge”, with the tag after the name, which
    /// must be there, left out.
    fn players(&self) -> Vec<String> {
        self.client
            .children(Some("players"))
            .iter()
            .map(|id| {
                format!(
                    "{}: {}",
                    untagged(&self.text(&format!("{id}_name"))),
                    self.text(&format!("{id}_vote"))
                )
            })
            .collect()
    }

    fn join(&mut self, name: &str) {
        self.write("name", json!(name));
        self.press("join_btn");
    }

    fn vote(&mut self, value: Value) {
        self.write("vote", value);
        self.press("vote_btn");
    }
}

/// “Ann · 1a2b3c4d (you)” as “Ann (you)”.
fn untagged(name: &str) -> String {
    let (name, rest) = name.split_once(" · ").expect("a name has its tag");
    let (tag, rest) = rest.split_at(TAG_LEN);
    assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "{tag}");
    format!("{name}{rest}")
}

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Poker::new(db.clone(), key, "hyperb://poker.test", path),
        out: Outbox::new(),
    };
    let hello = l.client.hello();
    l.receive(&hello);
    l
}

/// A manager with a room open, and the room's path.
fn open(db: &Arc<Db>) -> (Loopback, String) {
    let mut m = connect(db, person(), "/");
    m.write("draft.title", json!("Sprint 14"));
    m.press("create_btn");
    assert_eq!(m.view(), "room");
    let link = m.value("room.link");
    let path = link
        .as_str()
        .unwrap()
        .strip_prefix("hyperb://poker.test")
        .unwrap()
        .to_owned();
    (m, path)
}

#[test]
fn a_round_stays_hidden_until_everyone_votes() {
    let db = db();
    let (mut m, path) = open(&db);
    assert!(m.visible("manager"));
    assert_eq!(
        m.client.attr("start_btn", "label"),
        Some(json!("Start the first round"))
    );

    let mut ann = connect(&db, person(), &path);
    let mut bob = connect(&db, person(), &path);
    assert!(!ann.visible("manager"));
    assert!(ann.visible("join_card"));
    ann.join("Ann");
    bob.join("Bob");
    assert!(!ann.visible("join_card"));
    assert!(ann.visible("waiting"));

    m.tick();
    assert_eq!(m.players(), ["Ann: in", "Bob: in"]);
    m.write("next.topic", json!("Login page"));
    m.press("start_btn");
    assert_eq!(m.value("next.topic"), json!(""));
    assert!(m.visible("reveal_row") && !m.visible("start_row"));

    ann.tick();
    bob.tick();
    assert_eq!(ann.value("room.heading"), json!("Round 1 · Login page"));
    assert!(ann.visible("vote_row"));
    ann.vote(json!(5));
    assert_eq!(ann.feedback(), json!("You voted 5."));

    // Bob and the manager see that Ann voted, but not what.
    bob.tick();
    m.tick();
    assert_eq!(bob.players(), ["Ann: voted", "Bob (you): thinking…"]);
    assert_eq!(m.text("progress"), "1 of 2 voted");
    assert_eq!(m.players(), ["Ann: voted", "Bob: thinking…"]);
    assert_eq!(bob.value("room.result"), json!(""));

    // Ann changes her mind; the last vote counts.
    ann.vote(json!("8"));
    bob.vote(json!("3,5"));
    assert_eq!(bob.value("room.phase"), json!("revealed"));
    assert_eq!(bob.players(), ["Ann: 8", "Bob (you): 3.5"]);
    assert_eq!(
        bob.text("result"),
        "Average 5.75 · median 5.75 · from 3.5 to 8 (2 votes)"
    );
    ann.tick();
    m.tick();
    assert_eq!(ann.players(), ["Ann (you): 8", "Bob: 3.5"]);
    assert!(!ann.visible("vote_row"));
    assert!(m.visible("start_row"));
    assert_eq!(
        m.client.attr("start_btn", "label"),
        Some(json!("Start round 2"))
    );

    // No more votes once revealed.
    ann.forge("vote");
    assert_eq!(
        ann.feedback(),
        json!("There is no round to vote in right now.")
    );

    // The next round starts empty, and the last one goes to the history.
    ann.write("vote", json!("8"));
    m.press("start_btn");
    ann.tick();
    assert_eq!(ann.value("vote"), json!(""));
    assert_eq!(ann.value("room.heading"), json!("Round 2"));
    assert_eq!(ann.players(), ["Ann (you): thinking…", "Bob: thinking…"]);
    assert!(ann.visible("history_card"));
    assert_eq!(
        ann.text("past0"),
        "Round 1 · Login page: Average 5.75 · median 5.75 · from 3.5 to 8 (2 votes)"
    );
    for l in [&mut m, &mut ann, &mut bob] {
        assert!(l.client.take_diagnostics().is_empty());
    }
}

#[test]
fn the_manager_runs_the_room() {
    let db = db();
    let (mut m, path) = open(&db);
    let mut ann = connect(&db, person(), &path);
    let mut bob = connect(&db, person(), &path);
    ann.join("Ann");
    bob.join("Bob");

    // The manager may vote too.
    m.join("Max");
    m.press("start_btn");
    m.vote(json!(2));
    ann.tick();
    ann.vote(json!(2));
    assert_eq!(ann.value("room.phase"), json!("voting"));

    // Bob went for coffee: the manager reveals without him.
    m.press("reveal_btn");
    assert_eq!(m.players(), ["Ann: 2", "Bob: no vote", "Max (you): 2"]);
    assert_eq!(m.text("result"), "Everyone said 2 (2 votes)");
    bob.tick();
    assert_eq!(bob.feedback(), json!(""));
    assert!(!bob.visible("vote_row"));

    // Removing the last one who has not voted reveals the round.
    m.press("start_btn");
    m.vote(json!(1));
    ann.tick();
    ann.vote(json!(3));
    m.tick();
    let bob_row = m
        .client
        .children(Some("players"))
        .iter()
        .find(|id| untagged(&m.text(&format!("{id}_name"))) == "Bob")
        .unwrap()
        .to_string();
    m.press(&format!("{bob_row}_remove"));
    assert_eq!(m.value("room.phase"), json!("revealed"));
    assert_eq!(m.text("progress"), "2 of 2 voted");

    // Players cannot run rounds, even by forging the action.
    ann.tick();
    assert!(!ann.client.contains(&format!("{bob_row}_remove")));
    for name in ["start", "reveal", "delete"] {
        ann.forge(name);
        assert_eq!(
            ann.feedback(),
            json!("Only the manager of this room can do that.")
        );
    }

    // A forged early start does not throw away a round in progress.
    m.press("start_btn");
    m.vote(json!(1));
    m.forge("start");
    assert_eq!(
        m.feedback(),
        json!("Reveal this round before starting the next.")
    );
    assert_eq!(m.value("room.heading"), json!("Round 3"));

    // Closing the room sends everyone away.
    m.press("delete_btn");
    assert_eq!(m.view(), "home");
    assert!(!m.visible("mine"));
    ann.tick();
    assert_eq!(ann.view(), "missing");
    assert_eq!(ann.app.tick_interval(), None);
}

#[test]
fn players_come_back_with_their_key() {
    let db = db();
    let (mut m, path) = open(&db);
    let key = person();
    let mut ann = connect(&db, key, &path);
    ann.join("Ann");
    let mut bob = connect(&db, person(), &path);
    bob.join("Bob");
    m.press("start_btn");
    ann.tick();
    ann.vote(json!(13));
    drop(ann);

    let ann = connect(&db, key, &path);
    assert!(!ann.visible("join_card"));
    assert_eq!(
        ann.text("me_caption"),
        format!("You are in as Ann · {}.", tag(&key))
    );
    assert_eq!(ann.value("vote"), json!("13"));
    assert_eq!(ann.players(), ["Ann (you): voted", "Bob: thinking…"]);

    // The manager finds the room on the start page.
    let mut m = connect(&db, m.app.viewer, "/");
    assert!(m.visible("mine"));
    assert_eq!(
        m.client.attr("mine_0", "label"),
        Some(json!("Sprint 14 · 2 players"))
    );
    m.press("mine_0");
    assert!(m.visible("reveal_row"));
}

#[test]
fn the_name_goes_with_the_key() {
    let db = db();
    let (mut m, path) = open(&db);
    let max = m.app.viewer;
    assert_eq!(m.value("name"), json!(""));
    m.join("Max");

    // The same manager on another device, with the same key: still in.
    let other = connect(&db, max, &path);
    assert!(!other.visible("join_card"));
    assert_eq!(other.players(), ["Max (you): in"]);

    // And in their next room, the name is filled in already.
    let mut other = connect(&db, max, "/");
    other.press("create_btn");
    assert!(other.visible("join_card"));
    assert_eq!(other.value("name"), json!("Max"));
    other.press("join_btn");
    assert_eq!(other.players(), ["Max (you): in"]);
    let next = other.value("room.link").as_str().unwrap()["hyperb://poker.test".len()..].to_owned();

    // A player's too, and after a rename it is the new name.
    let mut ann = connect(&db, person(), &path);
    assert_eq!(ann.value("name"), json!(""));
    ann.join("Ann");
    ann.write("name", json!("Annie"));
    ann.forge("join");
    let ann = connect(&db, ann.app.viewer, &next);
    assert!(ann.visible("join_card"));
    assert_eq!(ann.value("name"), json!("Annie"));
}

#[test]
fn same_names_tell_apart_by_tag() {
    let db = db();
    let (mut m, path) = open(&db);
    let (k1, k2) = (person(), person());
    connect(&db, k1, &path).join("Sam");
    connect(&db, k2, &path).join("Sam");
    m.tick();
    let names: Vec<String> = m
        .client
        .children(Some("players"))
        .iter()
        .map(|id| m.text(&format!("{id}_name")))
        .collect();
    assert_eq!(
        names,
        [format!("Sam · {}", tag(&k1)), format!("Sam · {}", tag(&k2))]
    );
    assert!(
        m.text("tag_caption")
            .ends_with(&format!("{}.", tag(&m.app.viewer)))
    );
}

#[test]
fn tags_are_short_salted_and_stable() {
    let key = person();
    let t = tag(&key);
    assert_eq!(t.len(), 8);
    assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(t, tag(&key));
    assert_ne!(t, tag(&person()));
    assert_ne!(t, blake3::hash(&key.to_bytes()).to_hex()[..8]);
    assert!(!key.to_string().starts_with(&t));
}

#[test]
fn votes_must_be_numbers() {
    let db = db();
    let (mut m, path) = open(&db);
    let mut ann = connect(&db, person(), &path);
    ann.write("name", json!("  "));
    ann.press("join_btn");
    assert_eq!(ann.feedback(), json!("Type your name first."));
    ann.join("Ann");
    m.press("start_btn");
    ann.tick();
    for bad in [
        json!(""),
        json!("lots"),
        json!(-1),
        json!("1e9"),
        json!("NaN"),
    ] {
        ann.vote(bad.clone());
        assert_eq!(
            ann.client.attr("feedback", "variant"),
            Some(json!("failure")),
            "{bad}"
        );
    }
    assert_eq!(ann.players(), ["Ann (you): thinking…"]);
    ann.vote(json!("0.5"));
    assert_eq!(ann.players(), ["Ann (you): 0.5"]);
}

#[test]
fn unknown_links_and_old_rooms() {
    let db = db();
    let l = connect(&db, person(), "/nope");
    assert_eq!(l.view(), "missing");
    let l = connect(&db, person(), "/a/b");
    assert_eq!(l.view(), "missing");

    let (_m, path) = open(&db);
    let id = RoomId::new(path.trim_start_matches('/'));
    assert_eq!(prune(&db), 0);
    let mut r = db.rooms().get(&id).unwrap().unwrap();
    r.touched -= IDLE;
    db.rooms().update(&r).unwrap();
    assert_eq!(prune(&db), 1);
    assert!(db.owners().all().unwrap().is_empty());

    // A name nobody used for as long is forgotten too.
    let (mut m, _) = open(&db);
    m.join("Max");
    let mut p = db.people().all().unwrap().remove(0);
    p.touched -= IDLE;
    db.people().update(&p).unwrap();
    prune(&db);
    assert!(db.people().all().unwrap().is_empty());
}

#[test]
fn numbers_read_well() {
    assert_eq!(number(10.0), "10");
    assert_eq!(number(0.0), "0");
    assert_eq!(number(2.5), "2.5");
    assert_eq!(number(1.0 / 3.0), "0.33");
    assert_eq!(stats(&[]), "Nobody voted.");
}
