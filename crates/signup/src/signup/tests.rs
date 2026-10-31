use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Sheets,
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

    fn can_press(&self, id: &str) -> bool {
        self.client.contains(id)
            && self.client.attr(id, "visible") == Some(json!(true))
            && self.client.attr(id, "disabled") != Some(json!(true))
    }

    /// Presses a button that only changes the renderer's own signals.
    fn toggle(&mut self, id: &str) {
        assert_eq!(self.client.trigger(id, "onPress"), None);
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

    /// The texts of the history, newest first, without the times.
    fn history(&self) -> Vec<String> {
        self.client
            .children(Some("log"))
            .iter()
            .map(|id| {
                let t = self.text(id);
                t.rsplit_once(" · ").unwrap().0.to_owned()
            })
            .collect()
    }

    fn clean(&mut self) {
        assert_eq!(self.client.take_diagnostics(), Vec::<String>::new());
    }
}

const ORIGIN: &str = "hyperb://signup.example";

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Sheets::new(db.clone(), key, ORIGIN, path),
        out: Outbox::new(),
    };
    let hello = l.client.hello();
    l.receive(&hello);
    l
}

/// Ann makes a picnic sheet: drinks for one, dessert for one, drivers for
/// three. Returns her session and the sheet's id.
fn create(db: &Arc<Db>, key: PublicKey) -> (Loopback, String) {
    let mut ann = connect(db, key, "/");
    assert_eq!(ann.view(), "home");
    ann.press("create_btn");
    assert_eq!(ann.feedback(), json!("Give the sheet a title."));
    ann.write("draft.title", json!("Picnic"));
    ann.press("create_btn");
    assert_eq!(
        ann.feedback(),
        json!("List at least one item, one per line.")
    );
    ann.write("draft.items", json!("Drinks\n\n  dessert \nDRINKS"));
    ann.press("create_btn");
    assert_eq!(ann.feedback(), json!("“DRINKS” is in the list twice."));
    ann.write("draft.description", json!("Saturday, in the park."));
    ann.write("draft.items", json!("Drinks\nDessert\nDrivers x3"));
    ann.press("create_btn");
    assert_eq!(ann.view(), "sheet");
    let link = ann
        .client
        .value("sheet.link")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let id = link
        .strip_prefix(&format!("{ORIGIN}/"))
        .expect("the link is on the origin")
        .to_owned();
    ann.clean();
    (ann, id)
}

#[test]
fn tags_are_short_salted_and_stable() {
    let key = person();
    let t = tag(&key);
    assert_eq!(t.len(), 8);
    assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(t, tag(&key));
    assert_ne!(t, tag(&person()));
    // Salted: not just the start of the key's own hash.
    assert_ne!(t, blake3::hash(&key.to_bytes()).to_hex()[..8]);
    // Nor of the key itself.
    assert!(!key.to_string().starts_with(&t));
}

#[test]
fn items_parse_with_how_many() {
    assert_eq!(read_item("Drinks"), ("Drinks".into(), Some(1)));
    assert_eq!(read_item(" Drivers x3 "), ("Drivers".into(), Some(3)));
    assert_eq!(read_item("Chairs ×10"), ("Chairs".into(), Some(10)));
    assert_eq!(read_item("Malcolm X"), ("Malcolm X".into(), Some(1)));
    assert_eq!(read_item("x3"), ("x3".into(), Some(1)));
    assert_eq!(read_item("Cups x0"), ("Cups x0".into(), Some(1)));
    assert_eq!(read_item("Cups x1000"), ("Cups x1000".into(), Some(1)));
    assert_eq!(read_capacity(""), Ok(None));
    assert_eq!(read_capacity("4"), Ok(Some(4)));
    assert!(read_capacity("0").is_err());
    assert!(read_capacity("lots").is_err());
}

#[test]
fn people_put_names_down_and_everyone_sees_who() {
    let db = db();
    let (ann_key, bob_key) = (person(), person());
    let (mut ann, id) = create(&db, ann_key);
    assert_eq!(ann.text("sheet_title"), "Picnic");
    assert_eq!(ann.text("sheet_description"), "Saturday, in the park.");
    assert_eq!(ann.text("sheet_fill"), "0 of 5 spots taken");
    assert_eq!(ann.text("share_link"), format!("{ORIGIN}/{id}"));
    assert_eq!(ann.history(), [format!("{} made the sheet", tag(&ann_key))]);

    let mut bob = connect(&db, bob_key, &format!("/{id}"));
    assert_eq!(bob.view(), "sheet");
    assert!(bob.text("me_caption").contains(&tag(&bob_key)));
    assert!(!bob.visible("edit_btn"));
    // No name, no button.
    assert!(!bob.can_press("item0_join"));
    bob.write("who", json!("   "));
    bob.press("item0_join");
    assert_eq!(bob.feedback(), json!("Type in a name first."));
    bob.write("who", json!("Bob"));
    bob.press("item0_join");
    assert_eq!(bob.feedback(), json!("“Bob” is down for “Drinks”."));
    assert_eq!(bob.text("name3_name"), "Bob");
    assert_eq!(bob.text("name3_by"), format!("by {}", tag(&bob_key)));
    // Drinks takes one, so it is taken.
    assert!(!bob.client.contains("item0_join"));
    assert_eq!(bob.text("item0_full"), "Taken");
    assert_eq!(bob.text("item0_count"), "1 of 1");

    // Bob signs his friend up for driving, under his own tag.
    bob.press("item2_join");
    assert_eq!(bob.feedback(), json!("“Bob” is down for “Drivers”."));
    bob.press("item2_join");
    assert_eq!(
        bob.feedback(),
        json!("“Bob” is already down for “Drivers”.")
    );
    bob.write("who", json!("Cleo"));
    bob.press("item2_join");
    assert_eq!(bob.text("name5_by"), format!("by {}", tag(&bob_key)));
    assert_eq!(bob.text("sheet_fill"), "3 of 5 spots taken");
    bob.clean();

    // Ann sees it on her next tick.
    ann.tick();
    assert_eq!(ann.text("name3_name"), "Bob");
    assert_eq!(ann.text("name5_name"), "Cleo");
    let b = tag(&bob_key);
    assert_eq!(
        ann.history(),
        [
            format!("{b} put “Cleo” down for “Drivers”"),
            format!("{b} put “Bob” down for “Drivers”"),
            format!("{b} put “Bob” down for “Drinks”"),
            format!("{} made the sheet", tag(&ann_key)),
        ]
    );
    ann.clean();
}

#[test]
fn a_full_item_takes_no_more() {
    let db = db();
    let (_, id) = create(&db, person());
    let mut bob = connect(&db, person(), &format!("/{id}"));
    let mut cat = connect(&db, person(), &format!("/{id}"));
    bob.write("who", json!("Bob"));
    cat.write("who", json!("Cat"));
    // Cat's screen still offers Drinks after Bob took it.
    bob.press("item0_join");
    cat.press("item0_join");
    assert_eq!(cat.feedback(), json!("“Drinks” is already taken."));
    assert!(!cat.client.contains("item0_join"));
}

#[test]
fn names_come_off_only_by_who_wrote_them_or_the_owner() {
    let db = db();
    let ann_key = person();
    let (mut ann, id) = create(&db, ann_key);
    let mut bob = connect(&db, person(), &format!("/{id}"));
    let mut cat = connect(&db, person(), &format!("/{id}"));
    bob.write("who", json!("Bob"));
    bob.press("item1_join");
    cat.tick();
    // Cat gets no button for Bob's name, and is refused if she tries anyway.
    assert!(!cat.client.contains("name3_leave"));
    let line = bob.client.trigger("name3_leave", "onPress").unwrap();
    cat.receive(&line);
    assert_eq!(
        cat.feedback(),
        json!("You can only take off the names you put down.")
    );

    bob.press("name3_leave");
    assert_eq!(bob.feedback(), json!("“Bob” is off “Dessert”."));
    assert!(!bob.client.contains("name3_name"));
    bob.press("item1_join");
    ann.tick();
    ann.press("name4_leave");
    assert_eq!(ann.feedback(), json!("“Bob” is off “Dessert”."));
    assert_eq!(
        ann.history()[0],
        format!("{} took “Bob” off “Dessert”", tag(&ann_key))
    );
    // A stale button finds nothing.
    bob.press("name4_leave");
    assert_eq!(
        bob.feedback(),
        json!("That name is no longer on the sheet.")
    );
    bob.clean();
}

#[test]
fn the_owner_edits_the_items() {
    let db = db();
    let ann_key = person();
    let (mut ann, id) = create(&db, ann_key);
    let a = tag(&ann_key);
    assert!(!ann.visible("add_card"));
    assert!(!ann.visible("item0_edit"));
    ann.toggle("edit_btn");
    assert!(ann.visible("add_card"));
    assert!(ann.visible("item0_edit"));
    assert_eq!(ann.client.value("edit.i2.cap"), Some(&json!("3")));

    // Adding.
    ann.write("add.label", json!("drinks"));
    ann.press("add_btn");
    assert_eq!(ann.feedback(), json!("“drinks” is on the sheet already."));
    ann.write("add.label", json!("Napkins"));
    ann.write("add.cap", json!(""));
    ann.press("add_btn");
    assert_eq!(ann.feedback(), json!("“Napkins” is added."));
    assert_eq!(ann.client.value("add.label"), Some(&json!("")));
    assert_eq!(ann.text("item3_count"), "0, no limit");
    assert_eq!(ann.text("sheet_fill"), "0 names down");

    // Changing.
    let mut bob = connect(&db, person(), &format!("/{id}"));
    bob.write("who", json!("Bob"));
    bob.press("item2_join");
    bob.write("who", json!("Dee"));
    bob.press("item2_join");
    assert!(!bob.client.contains("item2_edit"));
    // Bob cannot edit even by sending what Ann's button sends.
    let line = ann.client.trigger("item2_edit_save", "onPress").unwrap();
    bob.receive(&line);
    assert_eq!(
        bob.feedback(),
        json!("Only whoever made this sheet can change its items.")
    );
    ann.tick();
    ann.write("edit.i2.cap", json!("1"));
    ann.press("item2_edit_save");
    assert_eq!(
        ann.feedback(),
        json!("“Drivers” has 2 names down already; take some off first.")
    );
    ann.write("edit.i2.cap", json!("2"));
    ann.write("edit.i2.label", json!("Cars"));
    ann.press("item2_edit_save");
    assert_eq!(ann.feedback(), json!("“Cars” is saved."));
    assert_eq!(ann.client.attr("item2", "title"), Some(json!("Cars")));
    assert_eq!(ann.text("item2_full"), "Taken");
    ann.press("item2_edit_save");
    assert_eq!(ann.feedback(), json!("Nothing to save."));

    // What Ann is typing in one item survives a change to another.
    ann.write("edit.i0.label", json!("Lemonade"));
    bob.write("who", json!("Bob"));
    bob.press("item1_join");
    ann.tick();
    assert_eq!(ann.client.value("edit.i0.label"), Some(&json!("Lemonade")));

    // Removing.
    ann.press("item2_edit_remove");
    assert_eq!(
        ann.feedback(),
        json!("“Cars” is removed, with the 2 names down for it.")
    );
    assert!(!ann.client.contains("item2"));
    bob.tick();
    assert!(!bob.client.contains("item2"));

    // The title.
    ann.write("details.title", json!("Big picnic"));
    ann.press("details_btn");
    assert_eq!(ann.text("sheet_title"), "Big picnic");
    assert_eq!(
        ann.history()[..5],
        [
            format!("{a} renamed the sheet to “Big picnic”"),
            format!("{a} removed “Cars”"),
            format!("{} put “Bob” down for “Dessert”", tag(&bob.app.viewer)),
            format!("{a} set “Cars” to take 2"),
            format!("{a} renamed “Drivers” to “Cars”"),
        ]
    );
    ann.clean();
    bob.clean();
}

#[test]
fn the_owner_lists_and_deletes_sheets() {
    let db = db();
    let ann_key = person();
    let (mut ann, id) = create(&db, ann_key);
    ann.press("home_btn");
    assert!(ann.visible("mine"));
    assert_eq!(
        ann.client.attr("mine_0", "label"),
        Some(json!("Picnic · 0 of 5 spots taken"))
    );

    let mut bob = connect(&db, person(), &format!("/{id}"));
    assert!(!bob.visible("add_card"));
    bob.receive(r#"action {"name":"delete","signals":{}}"#);
    assert_eq!(
        bob.feedback(),
        json!("Only its creator can delete this sheet.")
    );

    ann.press("mine_0");
    assert_eq!(ann.view(), "sheet");
    ann.toggle("edit_btn");
    ann.press("delete_btn");
    assert_eq!(ann.view(), "home");
    assert!(!ann.visible("mine"));
    bob.tick();
    assert_eq!(bob.view(), "missing");
    assert_eq!(bob.feedback(), json!("This sheet was deleted."));
    let gone = connect(&db, person(), &format!("/{id}"));
    assert_eq!(gone.view(), "missing");
    assert_eq!(connect(&db, person(), "/a/b").view(), "missing");
}

#[test]
fn idle_sheets_are_pruned() {
    let db = db();
    let (_, id) = create(&db, person());
    assert_eq!(prune(&db), 0);
    store::write(&db, |tx| {
        let mut s = tx.sheets().get(SheetId::new(&id))?.unwrap();
        s.touched -= IDLE + 1;
        tx.sheets().update(&s)
    });
    assert_eq!(prune(&db), 1);
    assert!(store::read(&db, |tx| tx.owners().all()).is_empty());
}

#[test]
fn times_read_naturally() {
    assert_eq!(ago(100, 100), "just now");
    assert_eq!(ago(0, 125), "2 min ago");
    assert_eq!(ago(0, 7200), "2 h ago");
    assert_eq!(ago(0, 86400), "yesterday");
    assert_eq!(ago(0, 3 * 86400), "3 days ago");
}
