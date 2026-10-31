use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Contests,
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

    /// Presses a button that only changes the renderer's own signals.
    fn toggle(&mut self, id: &str) {
        assert_eq!(self.client.trigger(id, "onPress"), None);
    }

    /// Sends an action as a renderer would, without a button for it.
    fn forge(&mut self, name: &str, signals: Value) {
        let body = json!({"name": name, "payload": null, "signals": signals});
        match RendererMessage::parse(&format!("action {body}")) {
            Ok(RendererMessage::Action(a)) => self.app.action(&mut self.out, &a),
            other => panic!("{other:?}"),
        }
        self.pump();
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

    fn attr(&self, id: &str, name: &str) -> String {
        self.client
            .attr(id, name)
            .unwrap_or_else(|| panic!("{id} has no {name}"))
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn text(&self, id: &str) -> String {
        self.attr(id, "text")
    }

    fn label(&self, id: &str) -> String {
        self.attr(id, "label")
    }

    fn clean(&mut self) {
        assert_eq!(self.client.take_diagnostics(), Vec::<String>::new());
    }
}

const ORIGIN: &str = "hyperb://judging.example";

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Contests::new(db.clone(), key, ORIGIN, path),
        out: Outbox::new(),
    };
    let hello = l.client.hello();
    l.receive(&hello);
    l
}

/// The id in a link of `ORIGIN`.
fn id_of(link: &str) -> String {
    link.strip_prefix(&format!("{ORIGIN}/"))
        .expect("the link is on the origin")
        .to_owned()
}

/// Olga sets up a bake-off of three cakes with four judges, and gets the
/// public id and the judges' ids, in order.
fn create(db: &Arc<Db>, key: PublicKey) -> (Loopback, String, Vec<String>) {
    let mut olga = connect(db, key, "/");
    assert_eq!(olga.view(), "home");
    olga.press("create_btn");
    assert_eq!(olga.feedback(), json!("Give the contest a name."));
    olga.write("draft.name", json!("Bake-off"));
    olga.write("draft.entries", json!("Lemon tart"));
    olga.press("create_btn");
    assert_eq!(
        olga.feedback(),
        json!("Type in at least 2 entries, one per line.")
    );
    olga.write(
        "draft.entries",
        json!("Lemon tart\n\n  Carrot cake \nScones"),
    );
    olga.press("create_btn");
    assert_eq!(olga.feedback(), json!("Type in the judges, one per line."));
    olga.write("draft.judges", json!("Ann\nBo\nann"));
    olga.press("create_btn");
    assert_eq!(olga.feedback(), json!("“ann” is in the judges twice."));
    olga.write("draft.judges", json!("Ann\nBo\nCy\nDee"));
    olga.write("draft.scale.max", json!("1"));
    olga.press("create_btn");
    assert_eq!(
        olga.feedback(),
        json!("Scores go from one whole number up to a larger one, at most 1000.")
    );
    olga.write("draft.scale.max", json!(10));
    olga.press("create_btn");
    assert_eq!(olga.view(), "contest");
    assert_eq!(
        olga.client.attr("feedback", "variant"),
        Some(json!("success"))
    );
    assert!(olga.visible("judges") && olga.visible("results"));
    assert!(!olga.visible("sheet"));
    assert_eq!(olga.text("c_scale"), "Scores from 1 to 10");
    assert_eq!(olga.text("c_progress"), "0 of 12 scores in");
    let names: Vec<String> = (0..4).map(|k| olga.text(&format!("j{k}_name"))).collect();
    assert_eq!(names, ["Ann", "Bo", "Cy", "Dee"]);
    assert_eq!(olga.text("j0_done"), "0 of 3 scored");
    let judges: Vec<String> = (0..4)
        .map(|k| id_of(&olga.text(&format!("j{k}_link"))))
        .collect();
    let view = id_of(&olga.text("view_link"));
    for id in judges.iter().chain([&view]) {
        assert!(cuid2::is_cuid2(id));
    }
    olga.clean();
    (olga, view, judges)
}

#[test]
fn routes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse(""), Route::Home);
    assert_eq!(Route::parse("/abc"), Route::Open("abc".into()));
    assert_eq!(Route::parse("/abc/"), Route::Open("abc".into()));
    assert_eq!(Route::parse("/abc/what"), Route::Unknown);
}

fn contest(entries: &[&str], scores: &[&[Option<u32>]]) -> Contest {
    Contest {
        id: ContestId::new("c"),
        admin: "a".into(),
        owner: String::new(),
        name: "Show".into(),
        entries: entries.iter().map(|e| e.to_string()).collect(),
        judges: scores
            .iter()
            .enumerate()
            .map(|(k, s)| Judge {
                name: format!("J{k}"),
                link: format!("l{k}"),
                scores: s.to_vec(),
            })
            .collect(),
        scale: Scale::default(),
        revealed: false,
        version: 1,
        touched: 0,
    }
}

#[test]
fn high_and_low_are_dropped_from_three_scores_on() {
    // By judge, scores for A, B, C, D.
    let c = contest(
        &["A", "B", "C", "D"],
        &[
            &[Some(10), Some(7), Some(5), None],
            &[Some(1), Some(7), Some(9), None],
            &[Some(6), Some(7), None, None],
            &[Some(7), Some(9), None, None],
        ],
    );
    let lines = leaderboard(&c);
    let order: Vec<&str> = lines
        .iter()
        .map(|l| c.entries[l.entry as usize].as_str())
        .collect();
    // A: 1 6 7 10, the 10 and 1 dropped, averages 6.5. B: 7 7 7 9 averages
    // 7. C has two scores, both kept: 7. D has none and comes last.
    assert_eq!(order, ["B", "C", "A", "D"]);
    let positions: Vec<Option<usize>> = lines.iter().map(|l| l.position).collect();
    assert_eq!(positions, [Some(1), Some(1), Some(3), None]);
    assert_eq!(lines[2].counted(), [6, 7]);
    assert_eq!(lines[2].average().as_deref(), Some("6.5"));
    assert_eq!(lines[0].average().as_deref(), Some("7"));
    assert_eq!(
        lines[2].detail(),
        "Scores 1 · 6 · 7 · 10. Dropping 1 and 10, the other 2 average 6.5."
    );
    assert!(
        lines[1]
            .detail()
            .starts_with("Scores 5 · 9, averaged as they are")
    );
    assert_eq!(lines[3].average(), None);

    // Two entries with the same scores, given by different judges, tie.
    let c = contest(
        &["X", "Y"],
        &[
            &[Some(3), Some(1)],
            &[Some(3), Some(3)],
            &[Some(4), Some(4)],
            &[Some(9), Some(3)],
            &[Some(1), Some(9)],
        ],
    );
    let lines = leaderboard(&c);
    assert_eq!(lines[0].average().as_deref(), Some("3.33"));
    assert_eq!(lines[0].position, Some(1));
    assert_eq!(lines[1].position, Some(1));
}

#[test]
fn judges_score_privately_and_the_leaderboard_appears_when_revealed() {
    let db = db();
    let (mut olga, view, judges) = create(&db, person());

    // Ann's link knows who she is.
    let mut ann = connect(&db, person(), &format!("/{}", judges[0]));
    assert_eq!(ann.view(), "contest");
    assert!(ann.visible("sheet") && !ann.visible("judges") && !ann.visible("results"));
    assert_eq!(ann.attr("sheet", "title"), "Scorecard of Ann");
    assert!(!ann.visible("delete_btn") && !ann.visible("reveal_btn"));
    let labels: Vec<String> = (0..3).map(|k| ann.label(&format!("e{k}_in"))).collect();
    assert_eq!(labels, ["Lemon tart", "Carrot cake", "Scones"]);
    ann.write("score.e0", json!("11"));
    ann.press("save_btn");
    assert_eq!(
        ann.feedback(),
        json!("The score for “Lemon tart” should be a whole number from 1 to 10.")
    );
    ann.write("score.e0", json!(9));
    ann.write("score.e1", json!("6"));
    ann.press("save_btn");
    assert_eq!(
        ann.feedback(),
        json!("Saved. You have scored 2 of 3 entries.")
    );
    assert_eq!(ann.text("sheet_mine"), "You have scored 2 of 3 entries.");
    // What she types meanwhile survives others' saves.
    ann.write("score.e2", json!("4"));

    // Bo sees his own empty card, not Ann's scores.
    let mut bo = connect(&db, person(), &format!("/{}", judges[1]));
    assert_eq!(bo.attr("sheet", "title"), "Scorecard of Bo");
    for k in 0..3 {
        assert_eq!(bo.client.value(&format!("score.e{k}")), Some(&json!("")));
    }
    bo.write("score.e0", json!("7"));
    bo.write("score.e1", json!("8"));
    bo.write("score.e2", json!("2"));
    bo.press("save_btn");
    ann.tick();
    assert_eq!(ann.client.value("score.e2"), Some(&json!("4")));
    assert!(!ann.client.contains("b0"), "no leaderboard for judges yet");

    // Everyone else waits, and cannot score or reveal.
    let mut fan = connect(&db, person(), &format!("/{view}"));
    assert!(fan.visible("waiting") && !fan.visible("results") && !fan.visible("sheet"));
    assert!(!fan.client.contains("b0") && !fan.client.contains("j0_link"));
    fan.forge("save", json!({"score": {"e0": "1"}}));
    fan.forge("reveal", json!({}));
    // Neither can a judge reveal, or add judges.
    bo.forge("reveal", json!({}));
    bo.forge("add_judge", json!({"new_judge": "Mallory"}));
    let c = db.contests().get(ContestId::new(&view)).unwrap().unwrap();
    assert!(!c.revealed);
    assert_eq!(c.judges.len(), 4);
    assert_eq!(c.judges[0].scores, [Some(9), Some(6), None]);

    // Cy and Dee score; the organizer watches it fill in.
    for (k, s) in [(2, ["8", "5", "3"]), (3, ["10", "6", ""])] {
        let mut j = connect(&db, person(), &format!("/{}", judges[k]));
        for (e, s) in s.iter().enumerate() {
            j.write(&format!("score.e{e}"), json!(s));
        }
        j.press("save_btn");
        j.clean();
    }
    olga.tick();
    assert_eq!(olga.text("c_progress"), "10 of 12 scores in");
    assert_eq!(olga.text("j3_done"), "2 of 3 scored");
    assert_eq!(olga.text("j1_done"), "3 of 3 scored");
    // Lemon tart 7 8 9 10 → 8.5; Carrot cake 5 6 6 8 → 6; Scones 2 3 → 2.5.
    assert_eq!(olga.label("b0_entry"), "1. Lemon tart");
    assert_eq!(olga.text("b0_avg"), "8.5");
    assert_eq!(olga.label("b1_entry"), "2. Carrot cake");
    assert_eq!(olga.text("b1_avg"), "6");
    assert_eq!(olga.label("b2_entry"), "3. Scones");
    assert!(!olga.visible("b0_scores"));
    olga.toggle("b0_entry");
    assert!(olga.visible("b0_scores"));
    assert_eq!(
        olga.text("b0_scores"),
        "Scores 7 · 8 · 9 · 10. Dropping 7 and 10, the other 2 average 8.5."
    );

    // The reveal.
    olga.press("reveal_btn");
    assert_eq!(
        olga.feedback(),
        json!("The leaderboard is out, and the scores are locked.")
    );
    assert_eq!(olga.attr("results", "title"), "Leaderboard");
    fan.tick();
    assert!(fan.visible("results") && !fan.visible("waiting"));
    assert_eq!(fan.label("b0_entry"), "1. Lemon tart");
    ann.tick();
    assert!(ann.visible("results") && ann.visible("locked") && !ann.visible("save_btn"));
    assert_eq!(ann.text("c_status"), "Results are out");
    ann.forge("save", json!({"score": {"e0": "1"}}));
    assert_eq!(
        ann.feedback(),
        json!("The results are out, so the scores are locked.")
    );

    // Hiding reopens scoring, and Ann's late score moves the board.
    olga.press("reveal_btn");
    ann.tick();
    fan.tick();
    assert!(!fan.client.contains("b0") && fan.visible("waiting"));
    ann.write("score.e2", json!("10"));
    ann.press("save_btn");
    olga.tick();
    // Scones 2 3 10 → 3.
    assert_eq!(olga.text("b2_avg"), "3");
    for l in [&mut olga, &mut ann, &mut bo, &mut fan] {
        l.clean();
    }
}

#[test]
fn the_organizer_adds_a_judge() {
    let db = db();
    let (mut olga, _, _) = create(&db, person());
    olga.press("add_btn");
    assert_eq!(olga.feedback(), json!("Type the judge's name."));
    olga.write("new_judge", json!("bo"));
    olga.press("add_btn");
    assert_eq!(olga.feedback(), json!("“bo” is judging already."));
    olga.write("new_judge", json!("Eve"));
    olga.press("add_btn");
    assert_eq!(
        olga.feedback(),
        json!("Eve is judging too. Send them their link below.")
    );
    assert_eq!(olga.client.value("new_judge"), Some(&json!("")));
    assert_eq!(olga.text("j4_name"), "Eve");
    let mut eve = connect(&db, person(), &format!("/{}", id_of(&olga.text("j4_link"))));
    assert_eq!(eve.attr("sheet", "title"), "Scorecard of Eve");
    eve.write("score.e1", json!("5"));
    eve.press("save_btn");
    assert_eq!(
        eve.feedback(),
        json!("Saved. You have scored 1 of 3 entries.")
    );
    olga.tick();
    assert_eq!(olga.text("j4_done"), "1 of 3 scored");
    olga.clean();
    eve.clean();
}

#[test]
fn the_creator_finds_and_deletes_it() {
    let db = db();
    let olga_key = person();
    let (olga, view, judges) = create(&db, olga_key);
    drop(olga);
    let mut olga = connect(&db, olga_key, "/");
    assert!(olga.visible("mine"));
    assert_eq!(
        olga.label("mine_0"),
        "Bake-off · 3 entries · 4 judges · scoring"
    );
    olga.press("mine_0");
    assert!(olga.visible("judges") && olga.visible("delete_btn"));
    let mut ann = connect(&db, person(), &format!("/{}", judges[0]));
    // Only the creator deletes, even with the organizer's link.
    let admin = db.admin_links().all().unwrap()[0].id.0.clone();
    let mut helper = connect(&db, person(), &format!("/{admin}"));
    assert!(helper.visible("judges") && !helper.visible("delete_btn"));
    helper.forge("delete", json!({}));
    assert_eq!(
        helper.feedback(),
        json!("Only its creator can delete this contest.")
    );

    olga.press("delete_btn");
    assert_eq!(olga.view(), "home");
    assert!(!olga.visible("mine"));
    assert!(db.admin_links().all().unwrap().is_empty());
    assert!(db.judge_links().all().unwrap().is_empty());
    ann.tick();
    assert_eq!(ann.view(), "missing");
    assert_eq!(ann.feedback(), json!("This contest was deleted."));
    assert_eq!(ann.app.tick_interval(), None);
    assert_eq!(
        connect(&db, person(), &format!("/{view}")).view(),
        "missing"
    );
    assert_eq!(connect(&db, person(), "/nope/really").view(), "missing");
    for l in [&mut olga, &mut ann, &mut helper] {
        l.clean();
    }
}

#[test]
fn idle_contests_are_pruned() {
    let db = db();
    let (_olga, view, _) = create(&db, person());
    assert_eq!(prune(&db), 0);
    let id = ContestId::new(view);
    let mut c = db.contests().get(&id).unwrap().unwrap();
    c.touched = store::now() - IDLE - 1;
    db.contests().update(&c).unwrap();
    assert_eq!(prune(&db), 1);
    assert!(db.judge_links().all().unwrap().is_empty());
    assert!(db.owners().all().unwrap().is_empty());
}
