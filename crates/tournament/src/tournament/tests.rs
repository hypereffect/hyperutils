use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;
use std::collections::BTreeSet;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Tournaments,
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

    fn label(&self, id: &str) -> String {
        self.client
            .attr(id, "label")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn clean(&mut self) {
        assert_eq!(self.client.take_diagnostics(), Vec::<String>::new());
    }
}

const ORIGIN: &str = "hyperb://tournament.example";

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Tournaments::new(db.clone(), key, ORIGIN, path),
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

/// Ann makes a league of four, and gets the edit and view ids.
fn create(db: &Arc<Db>, key: PublicKey) -> (Loopback, String, String) {
    let mut ann = connect(db, key, "/");
    assert_eq!(ann.view(), "home");
    ann.press("create_btn");
    assert_eq!(ann.feedback(), json!("Give the tournament a name."));
    ann.write("draft.name", json!("Park league"));
    ann.write("draft.teams", json!("Lions\n\n  Tigers  \nlions"));
    ann.press("create_btn");
    assert_eq!(ann.feedback(), json!("“lions” is in the list twice."));
    ann.write("draft.teams", json!("Lions"));
    ann.press("create_btn");
    assert_eq!(
        ann.feedback(),
        json!("Type in at least 2 teams, one per line.")
    );
    ann.write("draft.teams", json!("Lions\nTigers\nBears\nOwls"));
    ann.write("draft.points.win", json!("three"));
    ann.press("create_btn");
    assert_eq!(
        ann.feedback(),
        json!("Points for a win are a whole number from 0 to 99.")
    );
    ann.write("draft.points.win", json!(3));
    ann.press("create_btn");
    assert_eq!(ann.view(), "board");
    assert_eq!(
        ann.client.attr("feedback", "variant"),
        Some(json!("success"))
    );
    assert!(ann.visible("links"));
    let edit = id_of(&ann.text("edit_link"));
    let view = id_of(&ann.text("view_link"));
    assert!(cuid2::is_cuid2(&edit) && cuid2::is_cuid2(&view) && edit != view);
    ann.clean();
    (ann, edit, view)
}

#[test]
fn routes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse(""), Route::Home);
    assert_eq!(Route::parse("/abc"), Route::Open("abc".into()));
    assert_eq!(Route::parse("/abc/"), Route::Open("abc".into()));
    assert_eq!(Route::parse("/abc/what"), Route::Unknown);
}

#[test]
fn everyone_meets_everyone_once_a_round() {
    for teams in 2..=MAX_TEAMS {
        let games = schedule(teams, false);
        let rounds = if teams % 2 == 0 { teams - 1 } else { teams };
        assert_eq!(games.len(), teams * (teams - 1) / 2, "{teams} teams");
        assert_eq!(rounds_of(&games), rounds);
        let pairs: BTreeSet<(u32, u32)> = games
            .iter()
            .map(|g| (g.home.min(g.away), g.home.max(g.away)))
            .collect();
        assert_eq!(pairs.len(), games.len(), "a pair meets twice");
        for round in 0..rounds as u32 {
            let mut playing = BTreeSet::new();
            for g in games.iter().filter(|g| g.round == round) {
                assert!(g.home != g.away);
                assert!(playing.insert(g.home) && playing.insert(g.away));
            }
            // Everyone plays but the one with the bye.
            assert_eq!(playing.len(), teams - teams % 2);
        }
        // Home and away come out even, or one apart.
        for team in 0..teams as u32 {
            let home = games.iter().filter(|g| g.home == team).count() as i64;
            let away = games.iter().filter(|g| g.away == team).count() as i64;
            assert!(
                (home - away).abs() <= 1,
                "{teams} teams: {team} {home}/{away}"
            );
        }
    }
}

fn rounds_of(games: &[Game]) -> usize {
    games
        .iter()
        .map(|g| g.round as usize + 1)
        .max()
        .unwrap_or(0)
}

#[test]
fn twice_means_home_and_away() {
    let games = schedule(5, true);
    assert_eq!(games.len(), 20);
    assert_eq!(rounds_of(&games), 10);
    let legs: BTreeSet<(u32, u32)> = games.iter().map(|g| (g.home, g.away)).collect();
    assert_eq!(legs.len(), 20, "every pair plays once at each end");
}

#[test]
fn standings_count_points_and_break_ties() {
    let mut t = Tournament {
        id: TournamentId::new("t"),
        edit: "e".into(),
        owner: String::new(),
        name: "Cup".into(),
        teams: vec!["Cats".into(), "Bats".into(), "Rats".into(), "Ants".into()],
        points: Points::default(),
        games: Vec::new(),
        version: 1,
        touched: 0,
    };
    let game = |home, away, score| Game {
        round: 0,
        home,
        away,
        score,
    };
    t.games = vec![
        game(0, 1, Some([2, 0])),
        game(2, 3, Some([1, 1])),
        game(1, 2, Some([3, 1])),
        game(3, 0, None),
    ];
    let lines = standings(&t);
    let order: Vec<&str> = lines
        .iter()
        .map(|l| t.teams[l.team as usize].as_str())
        .collect();
    // Cats and Bats both have 3 points: Cats on goal difference (+2 vs +0).
    // Ants and Rats have 1: Ants on goal difference (0 vs -2).
    assert_eq!(order, ["Cats", "Bats", "Ants", "Rats"]);
    let positions: Vec<usize> = lines.iter().map(|l| l.position).collect();
    assert_eq!(positions, [1, 2, 3, 4]);
    let rats = &lines[3];
    assert_eq!(
        (
            rats.played,
            rats.won,
            rats.drawn,
            rats.lost,
            rats.scored,
            rats.conceded,
            rats.points
        ),
        (2, 0, 1, 1, 2, 4, 1)
    );

    // Two points a win and one a loss, and the other games go 0-0, 2-0 and
    // 2-0: everyone but Rats has 3 points, Ants lead on goal difference, and
    // Bats and Cats are level on everything and share second place.
    t.points = Points {
        win: 2,
        draw: 1,
        loss: 1,
    };
    t.games[1].score = Some([0, 0]);
    t.games[2].score = Some([2, 0]);
    t.games[3].score = Some([2, 0]);
    let lines = standings(&t);
    let order: Vec<&str> = lines
        .iter()
        .map(|l| t.teams[l.team as usize].as_str())
        .collect();
    assert_eq!(order, ["Ants", "Bats", "Cats", "Rats"]);
    let positions: Vec<usize> = lines.iter().map(|l| l.position).collect();
    assert_eq!(positions, [1, 2, 2, 4]);
    assert_eq!(lines[0].points, 3);
}

#[test]
fn results_entered_on_one_link_show_on_the_other() {
    let db = db();
    let (mut ann, edit, view) = create(&db, person());
    let t = db
        .tournaments()
        .get(TournamentId::new(view.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(t.teams, ["Lions", "Tigers", "Bears", "Owls"]);
    assert_eq!(t.games.len(), 6);
    assert!(ann.client.contains("round2") && !ann.client.contains("round3"));
    assert_eq!(ann.text("t_played"), "0 of 6 games played");

    // Cy follows on the view link: no inputs, no links, no buttons.
    let mut cy = connect(&db, person(), &format!("/{view}"));
    assert_eq!(cy.view(), "board");
    assert!(!cy.visible("links") && !cy.visible("points_card"));
    assert!(!cy.client.contains("g0_h") && !cy.client.contains("g0_save"));
    assert_eq!(cy.text("t_mode"), "View only");
    let (home, away) = (
        &t.teams[t.games[0].home as usize],
        &t.teams[t.games[0].away as usize],
    );
    assert_eq!(cy.text("g0"), format!("{home}  vs  {away}"));
    // Forging a result is ignored.
    cy.app.action(
        &mut cy.out,
        &forged_action(
            "score",
            json!(0),
            json!({"score": {"g0": {"h": "9", "a": "0"}}}),
        ),
    );
    cy.pump();
    assert_eq!(
        db.tournaments().get(&t.id).unwrap().unwrap().games[0].score,
        None
    );

    // Bob keeps score on the edit link.
    let mut bob = connect(&db, person(), &format!("/{edit}"));
    assert!(bob.visible("links"));
    assert!(!bob.visible("delete_btn"), "only the creator deletes");
    bob.write("score.g0.h", json!("2"));
    bob.press("g0_save");
    assert_eq!(
        bob.feedback(),
        json!("Enter both scores, or clear both to undo a result.")
    );
    bob.write("score.g0.a", json!(1));
    bob.write("score.g1.h", json!("1"));
    bob.press("g0_save");
    assert_eq!(bob.feedback(), json!("Result saved."));
    assert_eq!(bob.text("t_played"), "1 of 6 games played");
    assert_eq!(bob.client.attr("g0_save", "label"), Some(json!("Update")));
    // What he is typing in another game is kept.
    assert_eq!(bob.client.value("score.g1.h"), Some(&json!("1")));
    bob.write("score.g1.a", json!("x"));
    bob.press("g1_save");
    assert_eq!(bob.feedback(), json!("Scores are whole numbers."));

    // Cy sees it on the next tick.
    cy.tick();
    assert_eq!(cy.text("g0"), format!("{home}  2 – 1  {away}"));
    assert_eq!(cy.label("s0_team"), format!("1. {home}"));
    assert_eq!(cy.text("s0_pts"), "3 pts");
    assert_eq!(
        cy.text("s0_stats"),
        "Played 1 · W 1 · D 0 · L 0 · Goals 2–1 (+1)"
    );
    assert_eq!(cy.label("s3_team"), format!("4. {away}"));
    assert!(!cy.client.contains("s4"));
    // The records are hidden until a team is pressed, and pressing it
    // again hides it.
    assert!(!cy.visible("s0_stats") && !cy.visible("s3_stats"));
    cy.toggle("s0_team");
    assert!(cy.visible("s0_stats") && !cy.visible("s1_stats"));
    cy.toggle("s0_team");
    assert!(!cy.visible("s0_stats"));
    // An open record stays with its team when the order changes.
    cy.toggle("s3_team");
    assert!(cy.visible("s3_stats"));

    // Ann corrects it to a draw and makes a draw worth two.
    ann.tick();
    assert_eq!(ann.client.value("score.g0.h"), Some(&json!("2")));
    ann.write("score.g0.h", json!("1"));
    ann.press("g0_save");
    ann.write("points.draw", json!("2"));
    ann.press("points_btn");
    assert_eq!(
        ann.feedback(),
        json!("Points saved; the standings are counted again.")
    );
    cy.tick();
    assert_eq!(cy.text("t_points"), "Win 3 · Draw 2 · Loss 0");
    // Both drew 1–1 and share first; the two who have not played share
    // third.
    for k in 0..2 {
        assert!(cy.label(&format!("s{k}_team")).starts_with("1. "));
        assert_eq!(cy.text(&format!("s{k}_pts")), "2 pts");
    }
    assert!(cy.label("s2_team").starts_with("3. "));
    let at = (0..4)
        .find(|k| cy.label(&format!("s{k}_team")).ends_with(away.as_str()))
        .unwrap();
    assert!(cy.visible(&format!("s{at}_stats")));
    assert_eq!(
        (0..4)
            .filter(|k| cy.visible(&format!("s{k}_stats")))
            .count(),
        1
    );
    assert_eq!(cy.text("s2_pts"), "0 pts");
    assert!(cy.label("s3_team").starts_with("3. "));
    // Bob's inputs follow, since he was not changing that game.
    bob.tick();
    assert_eq!(bob.client.value("score.g0.h"), Some(&json!("1")));
    assert_eq!(bob.client.value("points.draw"), Some(&json!("2")));

    // Clearing both undoes the result.
    bob.write("score.g0.h", json!(""));
    bob.write("score.g0.a", json!(""));
    bob.press("g0_save");
    assert_eq!(bob.feedback(), json!("Result cleared."));
    assert_eq!(bob.text("t_played"), "0 of 6 games played");
    for l in [&mut ann, &mut bob, &mut cy] {
        l.clean();
    }
}

/// An action as a renderer would send it, without a button for it.
fn forged_action(name: &str, payload: Value, signals: Value) -> Action {
    let body = json!({"name": name, "payload": payload, "signals": signals});
    match RendererMessage::parse(&format!("action {body}")) {
        Ok(RendererMessage::Action(a)) => a,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_odd_number_of_teams_gives_byes() {
    let db = db();
    let mut ann = connect(&db, person(), "/");
    ann.write("draft.name", json!("Trio"));
    ann.write("draft.teams", json!("A\nB\nC"));
    ann.write("draft.twice", json!(true));
    ann.press("create_btn");
    assert_eq!(ann.text("t_played"), "0 of 6 games played");
    for round in 0..6 {
        assert!(ann.text(&format!("round{round}_bye")).starts_with("Bye: "));
    }
    ann.clean();
}

#[test]
fn the_creator_finds_and_deletes_it() {
    let db = db();
    let ann_key = person();
    let (ann, edit, view) = create(&db, ann_key);
    drop(ann);
    let mut ann = connect(&db, ann_key, "/");
    assert!(ann.visible("mine"));
    assert_eq!(
        ann.client.attr("mine_0", "label"),
        Some(json!("Park league · 4 teams · 0 of 6 games played"))
    );
    ann.press("mine_0");
    assert_eq!(id_of(&ann.text("edit_link")), edit);
    let mut cy = connect(&db, person(), &format!("/{view}"));
    // The view link does not offer deleting, even to the creator.
    let mut ann_viewing = connect(&db, ann_key, &format!("/{view}"));
    assert!(!ann_viewing.visible("delete_btn"));

    ann.press("delete_btn");
    assert_eq!(ann.view(), "home");
    assert!(!ann.visible("mine"));
    assert!(db.edit_links().all().unwrap().is_empty());
    cy.tick();
    assert_eq!(cy.view(), "missing");
    assert_eq!(cy.feedback(), json!("This tournament was deleted."));
    assert_eq!(cy.app.tick_interval(), None);
    assert_eq!(
        connect(&db, person(), &format!("/{edit}")).view(),
        "missing"
    );
    assert_eq!(connect(&db, person(), "/nope/really").view(), "missing");
    for l in [&mut ann, &mut cy, &mut ann_viewing] {
        l.clean();
    }
}

#[test]
fn idle_tournaments_are_pruned() {
    let db = db();
    let (_ann, _, view) = create(&db, person());
    assert_eq!(prune(&db), 0);
    let id = TournamentId::new(view);
    let mut t = db.tournaments().get(&id).unwrap().unwrap();
    t.touched = store::now() - IDLE - 1;
    db.tournaments().update(&t).unwrap();
    assert_eq!(prune(&db), 1);
    assert!(db.edit_links().all().unwrap().is_empty());
    assert!(db.owners().all().unwrap().is_empty());
}
