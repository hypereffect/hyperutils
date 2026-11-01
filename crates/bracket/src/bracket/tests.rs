use super::*;
use hyperb_core::identity::Identity;
use hypereffect::RendererMessage;
use hypereffect::client::Client;

/// A renderer and a session, wired together the way `run_app` does.
struct Loopback {
    client: Client,
    app: Brackets,
    out: Outbox,
}

impl Loopback {
    fn receive(&mut self, line: &str) -> String {
        match RendererMessage::parse(line.trim_end()) {
            Ok(RendererMessage::Hello { params }) => self.app.hello(&mut self.out, &params),
            Ok(RendererMessage::Action(a)) => self.app.action(&mut self.out, &a),
            Err(e) => panic!("{e:?}"),
        }
        self.pump()
    }

    /// Sends what the app wrote and returns it, the way the wire would.
    fn pump(&mut self) -> String {
        let out = self.out.take();
        self.client.receive(out.as_bytes());
        out
    }

    /// Presses a button and returns what was sent in answer.
    fn press(&mut self, id: &str) -> String {
        let line = self
            .client
            .trigger(id, "onPress")
            .unwrap_or_else(|| panic!("{id} cannot be pressed"));
        self.receive(&line)
    }

    fn write(&mut self, path: &str, value: Value) {
        self.client.write(path, value);
    }

    fn tick(&mut self) -> String {
        self.app.tick(&mut self.out, 0.0);
        self.pump()
    }

    /// Brings the screen up to date and returns what had to be sent.
    fn refresh(&mut self) -> String {
        self.app.refresh(&mut self.out);
        self.pump()
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

    fn clean(&mut self) {
        assert_eq!(self.client.take_diagnostics(), Vec::<String>::new());
    }
}

const ORIGIN: &str = "hyperb://bracket.example";

fn db() -> Arc<Db> {
    Arc::new(Db::in_memory().unwrap())
}

fn person() -> PublicKey {
    Identity::generate().public()
}

fn connect(db: &Arc<Db>, key: PublicKey, path: &str) -> Loopback {
    let mut l = Loopback {
        client: Client::new(),
        app: Brackets::new(db.clone(), key, ORIGIN, path),
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

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// A draw that always takes the first choice.
fn first(_: usize) -> usize {
    0
}

/// A small generator, so that the draws differ from run to run of a loop.
fn lcg(seed: u64) -> impl FnMut(usize) -> usize {
    let mut x = seed;
    move |n| {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((x >> 33) % n as u64) as usize
    }
}

/// The ids of the elements that a batch sends, in order.
fn sent_ids(batch: &str) -> Vec<&str> {
    batch
        .lines()
        .filter_map(|l| {
            l.strip_prefix("element {\"id\":\"")
                .map(|rest| &rest[..rest.find('"').unwrap()])
        })
        .collect()
}

/// The ids of the elements that a batch takes away.
fn removed_ids(batch: &str) -> Vec<&str> {
    batch
        .lines()
        .filter_map(|l| {
            l.strip_prefix("remove {\"id\":\"")
                .map(|rest| &rest[..rest.find('"').unwrap()])
        })
        .collect()
}

#[test]
fn routes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse("/abc/"), Route::Open("abc".into()));
    assert_eq!(Route::parse("/abc/what"), Route::Unknown);
}

#[test]
fn rounds_are_named_from_the_final() {
    let named: Vec<String> = (0..5).map(|r| round_name(r, 5)).collect();
    assert_eq!(
        named,
        [
            "Round of 32",
            "Round of 16",
            "Quarterfinals",
            "Semifinals",
            "Final"
        ]
    );
    assert_eq!(round_name(0, 1), "Final");
}

#[test]
fn the_draw_keeps_the_picks_and_gives_every_match_a_team() {
    for teams in MIN_TEAMS..=MAX_TEAMS {
        let list: Vec<String> = (0..teams).map(|t| format!("T{t}")).collect();
        let size = teams.next_power_of_two();
        for seed in 0..20 {
            let mut picks = vec![Pick::Draw; size];
            // The last team goes in the first spot, and the second match
            // gets a bye when there is one.
            picks[0] = Pick::Team(teams as u32 - 1);
            if size > teams && size >= 4 {
                picks[3] = Pick::Bye;
            }
            let slots = draw(&list, &picks, lcg(seed)).unwrap();
            assert_eq!(slots.len(), size);
            assert_eq!(slots[0], Some(teams as u32 - 1));
            if picks.get(3) == Some(&Pick::Bye) {
                assert_eq!(slots[3], None);
            }
            let mut placed: Vec<u32> = slots.iter().flatten().copied().collect();
            placed.sort();
            assert_eq!(placed, (0..teams as u32).collect::<Vec<_>>());
            for m in slots.chunks(2) {
                assert!(m.iter().any(Option::is_some), "{teams} teams: {slots:?}");
            }
        }
    }
}

#[test]
fn the_draw_refuses_what_cannot_be() {
    let five = names(&["A", "B", "C", "D", "E"]);
    let mut picks = vec![Pick::Draw; 8];
    picks[0] = Pick::Team(1);
    picks[5] = Pick::Team(1);
    assert_eq!(
        draw(&five, &picks, first),
        Err("“B” is in two spots.".into())
    );

    let mut picks = vec![Pick::Draw; 8];
    for k in [0, 2, 4, 6] {
        picks[k] = Pick::Bye;
    }
    assert_eq!(
        draw(&five, &picks, first),
        Err("There are only 3 byes.".into())
    );

    let mut picks = vec![Pick::Draw; 8];
    picks[2] = Pick::Bye;
    picks[3] = Pick::Bye;
    assert_eq!(
        draw(&five, &picks, first),
        Err("Match 2 has a bye in both spots.".into())
    );

    // Four teams paired off in two matches leave one for the other two.
    let picks: Vec<Pick> = (0..4).map(Pick::Team).chain([Pick::Draw; 4]).collect();
    assert_eq!(
        draw(&five, &picks, first),
        Err("Every first-round match needs a team: spread out the teams you placed.".into())
    );

    let four = names(&["A", "B", "C", "D"]);
    let mut picks = vec![Pick::Draw; 4];
    picks[1] = Pick::Bye;
    assert_eq!(
        draw(&four, &picks, first),
        Err("With 4 teams there are no byes.".into())
    );
}

fn bracket(teams: &[&str], slots: Vec<Option<u32>>) -> Bracket {
    Bracket {
        id: BracketId::new("b"),
        edit: "e".into(),
        owner: String::new(),
        name: "Cup".into(),
        teams: names(teams),
        scores: no_scores(slots.len()),
        slots,
        version: 1,
        touched: 0,
    }
}

#[test]
fn winners_go_through_and_corrections_clear_what_follows() {
    // Five teams: A plays B, and C, D and E have byes.
    let mut b = bracket(
        &["A", "B", "C", "D", "E"],
        vec![
            Some(0),
            Some(1),
            Some(2),
            None,
            None,
            Some(3),
            Some(4),
            None,
        ],
    );
    let m = matches(&b);
    assert_eq!(m.iter().map(Vec::len).collect::<Vec<_>>(), [4, 2, 1]);
    assert_eq!(m[1][0].sides, [Side::Waiting, Side::Team(2)]);
    assert_eq!(m[1][1].sides, [Side::Team(3), Side::Team(4)]);
    assert_eq!(
        set_score(&mut b, 1, 0, Some([1, 0])),
        Err("This match is still waiting for its teams.".into())
    );
    assert_eq!(
        set_score(&mut b, 0, 1, Some([1, 0])),
        Err("A bye needs no result.".into())
    );
    assert!(
        set_score(&mut b, 0, 0, Some([2, 2]))
            .unwrap_err()
            .starts_with("A knockout")
    );

    assert_eq!(set_score(&mut b, 0, 0, Some([2, 1])), Ok("Result saved."));
    set_score(&mut b, 1, 0, Some([3, 0])).unwrap();
    set_score(&mut b, 1, 1, Some([0, 1])).unwrap();
    assert_eq!(champion(&b), None);
    set_score(&mut b, 2, 0, Some([1, 0])).unwrap();
    assert_eq!(champion(&b), Some(0));
    assert_eq!(progress(&b), (4, 4));

    // A different score with the same winner keeps the rest.
    assert_eq!(set_score(&mut b, 0, 0, Some([5, 1])), Ok("Result saved."));
    assert_eq!(champion(&b), Some(0));
    // B going through instead clears A's later matches, not the other
    // semifinal.
    assert_eq!(
        set_score(&mut b, 0, 0, Some([0, 1])),
        Ok("Result saved. The later results it changed were cleared.")
    );
    assert_eq!(b.scores[1], [None, Some([0, 1])]);
    assert_eq!(b.scores[2], [None]);
    assert_eq!(matches(&b)[1][0].sides, [Side::Team(1), Side::Team(2)]);
    assert_eq!(champion(&b), None);
}

#[test]
fn a_bracket_is_placed_drawn_and_played() {
    let db = db();
    let ann_key = person();
    let mut ann = connect(&db, ann_key, "/");
    assert_eq!(ann.view(), "home");
    ann.press("next_btn");
    assert_eq!(ann.feedback(), json!("Give the bracket a name."));
    ann.write("draft.name", json!("Spring cup"));
    ann.write("draft.teams", json!("Lions\nTigers\nlions"));
    ann.press("next_btn");
    assert_eq!(ann.feedback(), json!("“lions” is in the list twice."));

    // Five teams make quarterfinals with three byes.
    ann.write("draft.teams", json!("Lions\nTigers\nBears\nOwls\nFoxes"));
    ann.press("next_btn");
    assert_eq!(ann.view(), "seed");
    assert_eq!(
        ann.text("seed_info"),
        "5 teams make a bracket starting with the quarterfinals. 3 teams get a bye \
         and go straight to the next round. Choose a team for any spot you want to \
         fill; the spots left on Draw are filled at random."
    );
    assert!(ann.client.contains("pick7") && !ann.client.contains("pick8"));
    let options = ann.client.attr("pick0", "options").unwrap();
    assert_eq!(options[0], json!({"value": "", "label": "Draw"}));
    assert_eq!(options[1], json!({"value": "bye", "label": "Bye"}));
    assert_eq!(options[6], json!({"value": "4", "label": "Foxes"}));

    // Back and forth keeps what was typed.
    ann.press("back_btn");
    assert_eq!(ann.view(), "home");
    assert_eq!(
        ann.client.value("draft.teams"),
        Some(&json!("Lions\nTigers\nBears\nOwls\nFoxes"))
    );
    ann.press("next_btn");

    // Lions against Foxes, Owls with a bye, the rest to the draw.
    ann.write("seed.s0", json!("0"));
    ann.write("seed.s1", json!("4"));
    ann.write("seed.s6", json!("3"));
    ann.write("seed.s7", json!("0"));
    ann.press("create_btn");
    assert_eq!(ann.feedback(), json!("“Lions” is in two spots."));
    ann.write("seed.s7", json!("bye"));
    ann.press("create_btn");
    assert_eq!(ann.view(), "board");
    assert!(ann.visible("links"));
    let edit = id_of(&ann.text("edit_link"));
    let view = id_of(&ann.text("view_link"));
    assert!(cuid2::is_cuid2(&edit) && cuid2::is_cuid2(&view) && edit != view);
    let b = db
        .brackets()
        .get(BracketId::new(view.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(&b.slots[..2], [Some(0), Some(4)]);
    assert_eq!(&b.slots[6..], [Some(3), None]);
    assert_eq!(b.slots.iter().flatten().count(), 5);
    assert_eq!(ann.text("t_played"), "0 of 4 matches played");
    assert_eq!(
        ann.client.attr("round0", "title"),
        Some(json!("Quarterfinals"))
    );
    assert_eq!(ann.client.attr("round2", "title"), Some(json!("Final")));
    assert_eq!(ann.text("g3"), "4. Owls goes through with a bye");
    assert_eq!(
        ann.text("g6"),
        "7. winner of match 5  vs  winner of match 6"
    );
    assert!(!ann.visible("t_champion"));

    // Cy follows on the view link.
    let mut cy = connect(&db, person(), &format!("/{view}"));
    assert!(!cy.visible("links") && !cy.client.contains("g0_save"));
    assert_eq!(cy.text("g0"), "1. Lions  vs  Foxes");

    // Bob keeps score on the edit link.
    let mut bob = connect(&db, person(), &format!("/{edit}"));
    assert!(!bob.visible("delete_btn"), "only the creator deletes");
    bob.write("score.g0.h", json!("2"));
    bob.write("score.g0.a", json!("2"));
    bob.press("g0_save");
    assert!(
        bob.feedback()
            .as_str()
            .unwrap()
            .starts_with("A knockout match")
    );
    bob.write("score.g0.a", json!(3));
    bob.press("g0_save");
    assert_eq!(bob.feedback(), json!("Result saved."));
    // The inputs show the saved result, so it can be corrected in place.
    assert_eq!(bob.client.value("score.g0.h"), Some(&json!(2)));
    assert_eq!(bob.client.value("score.g0.a"), Some(&json!(3)));
    assert_eq!(bob.text("t_played"), "1 of 4 matches played");
    // The owner's inputs follow the saved result too.
    ann.tick();
    assert_eq!(ann.client.value("score.g0.h"), Some(&json!(2)));
    assert_eq!(ann.client.value("score.g0.a"), Some(&json!(3)));
    cy.tick();
    assert_eq!(cy.text("g0"), "1. Lions  2 – 3  Foxes");
    assert!(cy.text("g4").starts_with("5. Foxes  vs  "));

    // Play out the rest, the first team always winning.
    for g in 1..7 {
        if bob.client.contains(&format!("g{g}_save")) {
            bob.write(&format!("score.g{g}.h"), json!("1"));
            bob.write(&format!("score.g{g}.a"), json!("0"));
            bob.press(&format!("g{g}_save"));
            assert_eq!(bob.feedback(), json!("Result saved."));
        }
    }
    assert!(bob.visible("t_champion"));
    let b = db
        .brackets()
        .get(BracketId::new(view.clone()))
        .unwrap()
        .unwrap();
    let champion = &b.teams[champion(&b).unwrap() as usize];
    assert_eq!(bob.text("t_champion"), format!("Champion: {champion}"));
    assert_eq!(bob.text("t_played"), "4 of 4 matches played");
    cy.tick();
    assert!(cy.visible("t_champion"));

    // Ann finds it on her start page and deletes it.
    drop(ann);
    let mut ann = connect(&db, ann_key, "/");
    assert_eq!(
        ann.client.attr("mine_0", "label"),
        Some(json!("Spring cup · 5 teams · 4 of 4 matches played"))
    );
    ann.press("mine_0");
    ann.press("delete_btn");
    assert_eq!(ann.view(), "home");
    assert!(db.edit_links().all().unwrap().is_empty());
    cy.tick();
    assert_eq!(cy.view(), "missing");
    for l in [&mut ann, &mut bob, &mut cy] {
        l.clean();
    }
}

#[test]
fn bringing_the_bracket_up_to_date_sends_only_what_changed() {
    let db = db();
    let mut bob = connect(&db, person(), "/");
    bob.write("draft.name", json!("Cup"));
    bob.write("draft.teams", json!("A\nB\nC\nD"));
    bob.press("next_btn");
    // Pinned, so that who plays whom is known.
    bob.write("seed.s0", json!(0));
    bob.write("seed.s1", json!(1));
    bob.write("seed.s2", json!(2));
    bob.write("seed.s3", json!(3));
    bob.press("create_btn");
    assert_eq!(bob.view(), "board");

    // The first result: the button that saved it and the final, whose
    // first side stops waiting.
    bob.write("score.g0.h", json!(1));
    bob.write("score.g0.a", json!(0));
    let put = bob.press("g0_save");
    assert_eq!(bob.feedback(), json!("Result saved."));
    assert_eq!(sent_ids(&put), ["g0_save", "g0_done", "g2"]);
    assert!(removed_ids(&put).is_empty());

    // The second one: only its button and badge, and the final, which now
    // has both teams and becomes inputs.
    bob.write("score.g1.h", json!(2));
    bob.write("score.g1.a", json!(1));
    let put = bob.press("g1_save");
    assert_eq!(
        sent_ids(&put),
        [
            "g1_save",
            "g1_done",
            "g2",
            "g2_n",
            "g2_home",
            "g2_h",
            "g2_dash",
            "g2_a",
            "g2_away",
            "g2_save"
        ]
    );

    // Brought up to date again, nothing has to go over the wire.
    assert_eq!(bob.refresh(), "");

    // A follower of the view link gets one whole copy, then silence.
    let view = id_of(&bob.text("view_link"));
    let mut cy = connect(&db, person(), &format!("/{view}"));
    assert_eq!(cy.text("g0"), "1. A  1 – 0  B");
    assert_eq!(cy.refresh(), "");

    // Correcting the first result re-sends its line of text, again only
    // that.
    bob.write("score.g0.h", json!(2));
    bob.press("g0_save");
    let put = cy.tick();
    assert_eq!(sent_ids(&put), ["g0"]);
    for l in [&mut bob, &mut cy] {
        l.clean();
    }
}

#[test]
fn idle_brackets_are_pruned() {
    let db = db();
    let mut ann = connect(&db, person(), "/");
    ann.write("draft.name", json!("Duel"));
    ann.write("draft.teams", json!("A\nB"));
    ann.press("next_btn");
    ann.press("create_btn");
    assert_eq!(ann.client.attr("round0", "title"), Some(json!("Final")));
    assert_eq!(prune(&db), 0);
    let mut b = db.brackets().all().unwrap().remove(0);
    b.touched = store::now() - IDLE - 1;
    db.brackets().update(&b).unwrap();
    assert_eq!(prune(&db), 1);
    assert!(db.edit_links().all().unwrap().is_empty());
    assert!(db.owners().all().unwrap().is_empty());
}
