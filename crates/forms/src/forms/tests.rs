use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Forms,
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

    /// Triggers a handler, if the client lets it, and delivers the response.
    fn trigger(&mut self, id: &str, event: &str) -> bool {
        let Some(line) = self.client.trigger(id, event) else {
            return false;
        };
        self.receive(&line);
        true
    }

    fn press(&mut self, id: &str) {
        assert!(self.trigger(id, "onPress"), "{id} cannot be pressed");
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

    fn text(&self, id: &str) -> Value {
        self.client.attr(id, "text").unwrap()
    }

    fn clean(&mut self) {
        assert_eq!(self.client.take_diagnostics(), Vec::<String>::new());
    }
}

const ORIGIN: &str = "hyperb://forms.example";

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Forms::new(db.clone(), key, ORIGIN, path),
        out: Outbox::new(),
    };
    let hello = l.client.hello();
    l.receive(&hello);
    l
}

/// Ann makes a private form with three questions, and gets its id.
fn create(db: &Arc<Db>, key: PublicKey) -> (Loopback, String) {
    let mut ann = connect(db, key, "/");
    assert_eq!(ann.view(), "home");
    ann.press("new_btn");
    assert_eq!(ann.view(), "edit");
    // A new form starts with one blank question.
    assert!(ann.client.contains("d0") && !ann.client.contains("d1"));
    ann.press("save_btn");
    assert_eq!(ann.feedback(), json!("Give the form a title."));
    ann.write("draft.title", json!("Offsite"));
    ann.press("save_btn");
    assert_eq!(ann.feedback(), json!("Question 1 needs a text."));
    ann.write("draft.fields.f0.label", json!("Your name"));
    ann.write("draft.fields.f0.required", json!(true));
    ann.press("add_btn");
    ann.write("draft.fields.f1.label", json!("Where?"));
    ann.write("draft.fields.f1.kind", json!("choice"));
    assert!(ann.visible("d1_options"));
    ann.press("save_btn");
    assert_eq!(
        ann.feedback(),
        json!("Give question 2 some options, one per line.")
    );
    ann.write("draft.fields.f1.options", json!("Lake\n\nMountains\nLake"));
    ann.press("add_btn");
    ann.write("draft.fields.f2.label", json!("Activities"));
    ann.write("draft.fields.f2.kind", json!("checkboxes"));
    ann.write(
        "draft.fields.f2.options",
        json!("Hiking\nSwimming\nNapping"),
    );
    ann.write("draft.lifetime", json!("3d"));
    ann.press("save_btn");
    assert_eq!(ann.view(), "fill");
    assert_eq!(
        ann.feedback(),
        json!("Your form is ready. Share the link below.")
    );
    let link = ann.text("share_link").as_str().unwrap().to_owned();
    let id = link
        .strip_prefix(&format!("{ORIGIN}/"))
        .expect("the link is on the origin")
        .to_owned();
    assert!(cuid2::is_cuid2(&id));
    assert_eq!(
        ann.text("results_caption"),
        json!(format!("Results: {link}/results"))
    );
    ann.clean();
    (ann, id)
}

#[test]
fn routes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse(""), Route::Home);
    assert_eq!(Route::parse("/abc"), Route::Fill("abc".into()));
    assert_eq!(Route::parse("/abc/"), Route::Fill("abc".into()));
    assert_eq!(Route::parse("/abc/results"), Route::Results("abc".into()));
    assert_eq!(Route::parse("/abc/edit"), Route::Edit("abc".into()));
    assert_eq!(Route::parse("/abc/what"), Route::Unknown);
}

#[test]
fn create_share_fill_and_read_results() {
    let db = db();
    let (ann_key, bob_key, cy_key) = (person(), person(), person());
    let (mut ann, id) = create(&db, ann_key);
    let form = db.forms().get(FormId::new(id.clone())).unwrap().unwrap();
    assert_eq!(form.fields[1].options, ["Lake", "Mountains"]);
    assert!(form.expires.abs_diff(store::now() + 3 * DAY) < 5);
    assert!(!form.public);

    // Bob opens the link.
    let mut bob = connect(&db, bob_key, &format!("/{id}"));
    assert_eq!(bob.view(), "fill");
    assert_eq!(bob.text("form_title"), json!("Offsite"));
    assert_eq!(bob.client.attr("q0", "title"), Some(json!("Your name *")));
    assert_eq!(
        bob.text("privacy"),
        json!("Private: only the creator sees the answers")
    );
    assert!(!bob.visible("owner"));
    assert!(!bob.visible("results_btn"));
    // The name is required.
    bob.press("submit_btn");
    assert_eq!(bob.feedback(), json!("“Your name” needs an answer."));
    bob.write("answer.f0", json!("Bob"));
    bob.write("answer.f1", json!("Lake"));
    bob.write("answer.f2.o0", json!(true));
    bob.write("answer.f2.o2", json!(true));
    bob.press("submit_btn");
    assert_eq!(
        bob.client.attr("feedback", "variant"),
        Some(json!("success"))
    );
    assert_eq!(
        bob.client.attr("submit_btn", "label"),
        Some(json!("Update my answers"))
    );
    bob.clean();

    // The results are private: Bob cannot see them, even by address.
    let mut bob2 = connect(&db, bob_key, &format!("/{id}/results"));
    assert_eq!(bob2.view(), "results");
    assert!(bob2.visible("private_notice"));
    assert!(!bob2.client.contains("r0"));
    // His answers come back when he returns.
    bob2.press("fill_btn");
    assert_eq!(bob2.client.value("answer.f0"), Some(&json!("Bob")));
    assert_eq!(bob2.client.value("answer.f2.o2"), Some(&json!(true)));
    assert_eq!(bob2.client.value("answer.f2.o1"), Some(&json!(false)));
    // Changing them replaces the response instead of adding one.
    bob2.write("answer.f1", json!("Mountains"));
    bob2.press("submit_btn");

    let mut cy = connect(&db, cy_key, &format!("/{id}"));
    cy.write("answer.f0", json!("Cy"));
    cy.write("answer.f1", json!("Mountains"));
    cy.write("answer.f2.o0", json!(true));
    cy.press("submit_btn");

    // Ann sees two responses.
    ann.tick();
    assert_eq!(
        ann.client.attr("results_btn", "label"),
        Some(json!("See results (2 responses)"))
    );
    ann.press("results_btn");
    assert_eq!(ann.view(), "results");
    assert!(!ann.visible("private_notice"));
    assert_eq!(ann.text("r0_count"), json!("2 of 2 answered"));
    assert_eq!(ann.client.attr("r1_o0", "label"), Some(json!("Lake — 0")));
    assert_eq!(
        ann.client.attr("r1_o1", "label"),
        Some(json!("Mountains — 2"))
    );
    assert_eq!(ann.client.attr("r1_o1", "value"), Some(json!(1.0)));
    assert_eq!(ann.client.attr("r2_o0", "value"), Some(json!(1.0)));
    assert_eq!(ann.client.attr("r2_o2", "value"), Some(json!(0.5)));
    let names = [ann.text("r0_0"), ann.text("r0_1")];
    assert!(names.contains(&json!("Bob")) && names.contains(&json!("Cy")));
    ann.clean();

    // She comes back later and finds the form on her start page.
    let mut ann = connect(&db, ann_key, "/");
    assert!(ann.visible("mine"));
    let label = ann.client.attr("mine_0", "label").unwrap();
    assert!(
        label
            .as_str()
            .unwrap()
            .starts_with("Offsite · 2 responses · "),
        "{label}"
    );
    ann.press("mine_0");
    assert_eq!(ann.view(), "results");
    assert_eq!(ann.text("count"), json!("2 responses"));
    ann.clean();
}

#[test]
fn the_owner_edits_and_publishes_while_people_watch() {
    let db = db();
    let (ann_key, bob_key) = (person(), person());
    let (mut ann, id) = create(&db, ann_key);
    let mut bob = connect(&db, bob_key, &format!("/{id}"));
    bob.write("answer.f0", json!("Bob"));
    bob.write("answer.f2.o1", json!(true));
    bob.press("submit_btn");
    let mut watcher = connect(&db, person(), &format!("/{id}/results"));
    assert!(watcher.visible("private_notice"));
    assert_eq!(watcher.app.tick_interval(), Some(WATCH_MS));

    // Ann drops the choice, moves the checkboxes first, asks something new
    // and makes the results public.
    ann.press("edit_btn");
    assert_eq!(ann.view(), "edit");
    assert_eq!(ann.client.value("draft.editing"), Some(&json!(true)));
    assert!(!ann.visible("lifetime_in"));
    assert_eq!(
        ann.client.value("draft.fields.f1.options"),
        Some(&json!("Lake\nMountains"))
    );
    ann.press("d1_remove");
    ann.press("d2_up");
    assert_eq!(ann.client.attr("d2", "title"), Some(json!("Question 1")));
    assert_eq!(ann.client.attr("d2_up", "disabled"), Some(json!(true)));
    ann.press("add_btn");
    // The removed question's id is not reused.
    assert!(ann.client.contains("d3"));
    ann.write("draft.fields.f3.label", json!("How many nights?"));
    ann.write("draft.fields.f3.kind", json!("number"));
    ann.write("draft.public", json!(true));
    ann.press("save_btn");
    assert_eq!(ann.feedback(), json!("Saved."));
    let form = db.forms().get(FormId::new(id.clone())).unwrap().unwrap();
    let ids: Vec<u32> = form.fields.iter().map(|f| f.id).collect();
    assert_eq!(ids, [2, 0, 3]);
    assert!(form.public);

    // Bob, who was typing, keeps what did not change and sees the new
    // question; the results open up for the watcher.
    bob.write("answer.f0", json!("Robert"));
    bob.tick();
    assert!(!bob.client.contains("q1") && bob.client.contains("q3"));
    assert_eq!(bob.client.value("answer.f0"), Some(&json!("Robert")));
    assert_eq!(bob.client.value("answer.f2.o1"), Some(&json!(true)));
    assert!(bob.visible("results_btn"));
    assert_eq!(
        bob.text("privacy"),
        json!("Public results: anyone can see the answers")
    );
    bob.write("answer.f3", json!("two"));
    bob.press("submit_btn");
    assert_eq!(bob.feedback(), json!("“How many nights?” takes a number."));
    bob.write("answer.f3", json!(3));
    bob.press("submit_btn");
    let mut cy = connect(&db, person(), &format!("/{id}"));
    cy.write("answer.f0", json!("Cy"));
    cy.write("answer.f3", json!("2"));
    cy.press("submit_btn");

    watcher.tick();
    assert!(!watcher.visible("private_notice"));
    assert_eq!(
        watcher.text("r3_stats"),
        json!("Average 2.5 · lowest 2 · highest 3")
    );
    assert_eq!(watcher.text("r2_count"), json!("1 of 2 answered"));
    assert!(!watcher.client.contains("r1"));
    for l in [&mut ann, &mut bob, &mut cy, &mut watcher] {
        l.clean();
    }
}

#[test]
fn only_the_owner_edits_or_deletes() {
    let db = db();
    let (ann_key, bob_key) = (person(), person());
    let (mut ann, id) = create(&db, ann_key);

    // The edit address does not open the editor for Bob.
    let mut bob = connect(&db, bob_key, &format!("/{id}/edit"));
    assert_eq!(bob.view(), "fill");
    assert_eq!(
        bob.feedback(),
        json!("Only its creator can edit this form.")
    );
    assert!(!bob.trigger("edit_btn", "onPress"));
    // Nor does forging the action.
    let forged = bob
        .client
        .trigger("submit_btn", "onPress")
        .unwrap()
        .replace("\"submit\"", "\"edit\"");
    bob.receive(&forged);
    assert_eq!(bob.view(), "fill");
    for name in ["save", "delete"] {
        let forged = bob
            .client
            .trigger("submit_btn", "onPress")
            .unwrap()
            .replace("\"submit\"", &format!("\"{name}\""));
        bob.receive(&forged);
    }
    assert!(db.forms().contains(FormId::new(id.clone())).unwrap());

    // Ann deletes it, and Bob is told.
    ann.press("edit_btn");
    ann.press("delete_btn");
    assert_eq!(ann.view(), "home");
    assert!(!ann.visible("mine"));
    bob.tick();
    assert_eq!(bob.view(), "missing");
    assert_eq!(
        bob.feedback(),
        json!("This form was deleted, or it expired.")
    );
    assert_eq!(bob.app.tick_interval(), None);
    assert!(db.responses().all().unwrap().is_empty());
    bob.clean();
}

#[test]
fn expired_forms_are_gone_and_pruned() {
    let db = db();
    let (_ann, id) = create(&db, person());
    let fid = FormId::new(id.clone());
    let mut bob = connect(&db, person(), &format!("/{id}"));
    bob.write("answer.f0", json!("Bob"));
    bob.press("submit_btn");
    assert_eq!(prune(&db), 0);

    let mut f = db.forms().get(&fid).unwrap().unwrap();
    f.expires = store::now();
    db.forms().update(&f).unwrap();
    // Before the prune comes, the form is already out of reach.
    let late = connect(&db, person(), &format!("/{id}"));
    assert_eq!(late.view(), "missing");
    bob.press("submit_btn");
    assert_eq!(bob.feedback(), json!("This form no longer exists."));

    assert_eq!(prune(&db), 1);
    assert!(!db.forms().contains(&fid).unwrap());
    assert!(db.responses().all().unwrap().is_empty());
    assert!(db.owners().all().unwrap().is_empty());
    assert_eq!(connect(&db, person(), "/nope/really").view(), "missing");
}

#[test]
fn forms_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forms.redb");
    let ann_key = person();
    let db = Arc::new(Db::open(&path).unwrap());
    let (ann, id) = create(&db, ann_key);
    drop((ann, db));

    let db = Arc::new(Db::open(&path).unwrap());
    let mut ann = connect(&db, ann_key, "/");
    ann.press("mine_0");
    assert_eq!(ann.text("form_title"), json!("Offsite"));
    assert_eq!(ann.text("share_link"), json!(format!("{ORIGIN}/{id}")));
}
