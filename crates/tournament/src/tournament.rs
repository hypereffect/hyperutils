//! Round-robin tournaments.
//!
//! Anyone can make one: a name, the teams, one per line, what a win, a draw
//! and a loss are worth, and whether teams meet once or twice (home and
//! away). The fixtures are drawn up by the circle method, so every team
//! plays once a round, and an odd number of teams gives one of them a bye
//! each round. A tournament has two links: `hyperb://<host>/<edit id>`, where
//! anyone can enter and correct results and change the points, and
//! `hyperb://<host>/<id>`, which only shows the fixtures and the standings.
//! Tournaments are listed on the start page of the key that made them.
//!
//! A session watches the tournament it has open on its tick, so results
//! entered elsewhere show up within a couple of seconds.

use crate::store::{
    self, Db, EditId, EditLink, Game, Owner, OwnerKey, Points, Tournament, TournamentId, WriteTx,
};
use hyperb_core::identity::PublicKey;
use hypereffect::{Action, App, Element, Handler, Metadata, Outbox, Value, cel, json, ui};
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

/// How often an open tournament is checked for news.
const WATCH_MS: u32 = 2000;
const MIN_TEAMS: usize = 2;
const MAX_TEAMS: usize = 24;
const MAX_NAME_LEN: usize = 120;
const MAX_TEAM_LEN: usize = 40;
const MAX_POINTS: u32 = 99;
const MAX_SCORE: u32 = 999;
/// How many tournaments a key's start page lists.
const MAX_OWNED: usize = 50;
/// How long a tournament is kept without a change.
const IDLE: u64 = 180 * 24 * 60 * 60;

/// Deletes the tournaments nobody changed for [`IDLE`], and owners left with
/// none. Returns how many tournaments went.
pub fn prune(db: &Db) -> usize {
    let cutoff = store::now().saturating_sub(IDLE);
    store::write(db, |tx| {
        let mut n = 0;
        for t in tx.tournaments().all()? {
            if t.touched <= cutoff {
                tx.tournaments().delete(&t.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.tournaments.is_empty() {
                tx.owners().delete(&o.key)?;
            }
        }
        Ok(n)
    })
}

/// The games of a round robin between `teams` teams, round by round. With
/// `twice`, the second half repeats the first with home and away swapped.
pub fn schedule(teams: usize, twice: bool) -> Vec<Game> {
    // A team facing `None` has the bye.
    let mut slots: Vec<Option<u32>> = (0..teams as u32).map(Some).collect();
    if slots.len() % 2 == 1 {
        slots.push(None);
    }
    let n = slots.len();
    let rounds = n.saturating_sub(1);
    let mut games = Vec::new();
    for round in 0..rounds {
        for i in 0..n / 2 {
            let (Some(a), Some(b)) = (slots[i], slots[n - 1 - i]) else {
                continue;
            };
            // The other slots turn through both ends of the pairing, so only
            // the fixed one needs to alternate for home and away to come out
            // even, or one apart.
            let (home, away) = if i == 0 && round % 2 == 1 {
                (b, a)
            } else {
                (a, b)
            };
            games.push(Game {
                round: round as u32,
                home,
                away,
                score: None,
            });
        }
        // The first slot stays, the others turn one place.
        slots[1..].rotate_right(1);
    }
    if twice {
        let returns: Vec<Game> = games
            .iter()
            .map(|g| Game {
                round: g.round + rounds as u32,
                home: g.away,
                away: g.home,
                score: None,
            })
            .collect();
        games.extend(returns);
    }
    games
}

/// A team's line in the standings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub team: u32,
    /// Shared by teams level on points, goal difference and goals scored.
    pub position: usize,
    pub played: u32,
    pub won: u32,
    pub drawn: u32,
    pub lost: u32,
    pub scored: u32,
    pub conceded: u32,
    pub points: u32,
}

impl Line {
    pub fn difference(&self) -> i64 {
        self.scored as i64 - self.conceded as i64
    }

    fn rank(&self) -> (u32, i64, u32) {
        (self.points, self.difference(), self.scored)
    }
}

/// The standings: by points, then goal difference, then goals scored, then
/// name.
pub fn standings(t: &Tournament) -> Vec<Line> {
    let mut lines: Vec<Line> = (0..t.teams.len() as u32)
        .map(|team| Line {
            team,
            ..Line::default()
        })
        .collect();
    for g in &t.games {
        let Some([h, a]) = g.score else { continue };
        for (team, ours, theirs) in [(g.home, h, a), (g.away, a, h)] {
            let Some(l) = lines.get_mut(team as usize) else {
                continue;
            };
            l.played += 1;
            l.scored += ours;
            l.conceded += theirs;
            let (won, drawn, lost, points) = match ours.cmp(&theirs) {
                std::cmp::Ordering::Greater => (1, 0, 0, t.points.win),
                std::cmp::Ordering::Equal => (0, 1, 0, t.points.draw),
                std::cmp::Ordering::Less => (0, 0, 1, t.points.loss),
            };
            l.won += won;
            l.drawn += drawn;
            l.lost += lost;
            l.points += points;
        }
    }
    lines.sort_by(|x, y| {
        Reverse(x.rank())
            .cmp(&Reverse(y.rank()))
            .then_with(|| t.teams[x.team as usize].cmp(&t.teams[y.team as usize]))
    });
    for k in 0..lines.len() {
        lines[k].position = if k > 0 && lines[k].rank() == lines[k - 1].rank() {
            lines[k - 1].position
        } else {
            k + 1
        };
    }
    lines
}

/// The standings as the items of the list `table`, one per team, from the
/// top. Pressing a team shows its record, and pressing it again hides it;
/// that happens in the renderer, under `_open.t<team>`, which follows the team
/// when the order changes and is never sent back.
fn standing_items(t: &Tournament) -> Vec<Element> {
    let mut elements = Vec::new();
    for (k, l) in standings(t).iter().enumerate() {
        let item = format!("s{k}");
        let top = format!("{item}_top");
        let open = format!("s._open.t{}", l.team);
        elements.push(ui::column(&item).parent("table").gap("none"));
        elements.push(ui::row(&top).parent(&item).justify("between"));
        elements.push(
            ui::button(format!("{item}_team"))
                .parent(&top)
                .variant("plain")
                .label(format!("{}. {}", l.position, t.teams[l.team as usize]))
                .on_press(Handler::patch(
                    json!({"_open": {format!("t{}", l.team): cel(format!("!{open}"))}}),
                )),
        );
        elements.push(
            ui::badge(format!("{item}_pts"))
                .parent(&top)
                .variant(if l.position == 1 && l.played > 0 {
                    "success"
                } else {
                    "info"
                })
                .text(format!(
                    "{} pt{}",
                    l.points,
                    if l.points == 1 { "" } else { "s" }
                )),
        );
        elements.push(
            ui::text(format!("{item}_stats"))
                .parent(&item)
                .variant("caption")
                .visible(cel(open))
                .text(format!(
                    "Played {} · W {} · D {} · L {} · Goals {}–{} ({:+})",
                    l.played,
                    l.won,
                    l.drawn,
                    l.lost,
                    l.scored,
                    l.conceded,
                    l.difference()
                )),
        );
    }
    elements
}

/// Where the address points.
#[derive(Clone, Debug, PartialEq)]
enum Route {
    Home,
    Open(String),
    Unknown,
}

impl Route {
    fn parse(path: &str) -> Route {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match segs.as_slice() {
            [] => Route::Home,
            [id] => Route::Open(id.to_string()),
            _ => Route::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum View {
    Home,
    Board,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Board => "board",
            View::Missing => "missing",
        }
    }
}

fn limit(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// A signal as trimmed text; numbers are written out.
fn text(action: &Action, path: &str) -> String {
    match action.signal(path) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn flag(action: &Action, path: &str) -> bool {
    action.signal(path).and_then(Value::as_bool) == Some(true)
}

/// A whole number from `0` to `max` in a signal, if it is one.
fn count(action: &Action, path: &str, max: u32) -> Option<u32> {
    text(action, path).parse().ok().filter(|&n| n <= max)
}

/// The `feedback` signal patch for a notice.
fn feedback(kind: &str, text: impl Into<String>) -> Value {
    json!({"feedback": {"kind": kind, "text": text.into()}})
}

/// The teams typed in, one per line, or what is wrong with them.
fn read_teams(lines: &str) -> Result<Vec<String>, String> {
    let mut teams: Vec<String> = Vec::new();
    for line in lines.lines() {
        let name = limit(line.trim(), MAX_TEAM_LEN);
        if name.is_empty() {
            continue;
        }
        if teams
            .iter()
            .any(|t| t.to_lowercase() == name.to_lowercase())
        {
            return Err(format!("“{name}” is in the list twice."));
        }
        teams.push(name);
    }
    if teams.len() < MIN_TEAMS {
        return Err(format!("Type in at least {MIN_TEAMS} teams, one per line."));
    }
    if teams.len() > MAX_TEAMS {
        return Err(format!("At most {MAX_TEAMS} teams, please."));
    }
    Ok(teams)
}

/// The points the action carries under `at`, or what is wrong with them.
fn read_points(action: &Action, at: &str) -> Result<Points, String> {
    let get = |what: &str| {
        count(action, &format!("{at}.{what}"), MAX_POINTS).ok_or(format!(
            "Points for a {what} are a whole number from 0 to {MAX_POINTS}."
        ))
    };
    Ok(Points {
        win: get("win")?,
        draw: get("draw")?,
        loss: get("loss")?,
    })
}

fn points_signal(p: Points) -> Value {
    json!({"win": p.win.to_string(), "draw": p.draw.to_string(), "loss": p.loss.to_string()})
}

fn score_signal(score: Option<[u32; 2]>) -> Value {
    match score {
        Some([h, a]) => json!({"h": h.to_string(), "a": a.to_string()}),
        None => json!({"h": "", "a": ""}),
    }
}

/// The number of rounds of `t`.
fn rounds(t: &Tournament) -> u32 {
    t.games.iter().map(|g| g.round + 1).max().unwrap_or(0)
}

/// The tournament a link opens, and whether the link can enter results.
fn find(db: &Db, id: &str) -> Option<(Tournament, bool)> {
    store::read(db, |tx| {
        if let Some(t) = tx.tournaments().get(TournamentId::new(id))? {
            return Ok(Some((t, false)));
        }
        let Some(link) = tx.edit_links().get(EditId::new(id))? else {
            return Ok(None);
        };
        Ok(tx.tournaments().get(&link.tournament)?.map(|t| (t, true)))
    })
}

pub struct Tournaments {
    db: Arc<Db>,
    /// The key of this session.
    viewer: PublicKey,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    /// The tournament open in this session, whether this session can enter
    /// results, and the version on screen.
    open: Option<TournamentId>,
    can_edit: bool,
    seen: u64,
    /// The scores on screen, by game, so that a refresh only resets the
    /// inputs of the games whose result changed.
    shown: Vec<Option<[u32; 2]>>,
    /// The points on screen, likewise.
    shown_points: Option<Points>,
    /// Whether the signals that show and hide the records are set up.
    toggles: bool,
    /// Ids of the elements that are replaced as a whole.
    mine_ids: Vec<String>,
    standing_ids: Vec<String>,
    fixture_ids: Vec<String>,
}

impl Tournaments {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Tournaments {
            db,
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            open: None,
            can_edit: false,
            seen: 0,
            shown: Vec::new(),
            shown_points: None,
            toggles: false,
            mine_ids: Vec::new(),
            standing_ids: Vec::new(),
            fixture_ids: Vec::new(),
        }
    }

    fn me(&self) -> String {
        self.viewer.to_string()
    }

    /// Replaces the elements `ids` with `new`.
    fn replace(out: &mut Outbox, ids: &mut Vec<String>, new: Vec<Element>) {
        for id in ids.drain(..) {
            out.remove(id);
        }
        for e in new {
            ids.push(e.id.clone());
            out.element(e);
        }
    }

    /// Switches to `view`, dropping what belonged to the one before.
    fn show(&mut self, out: &mut Outbox, view: View) {
        Self::replace(out, &mut self.standing_ids, Vec::new());
        Self::replace(out, &mut self.fixture_ids, Vec::new());
        self.open = None;
        self.can_edit = false;
        self.shown.clear();
        self.shown_points = None;
        self.toggles = false;
        self.view = view;
        out.signal(json!({"view": view.name(), "feedback": {"kind": "info", "text": ""}}));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let mine: Vec<Tournament> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut mine = Vec::new();
            for id in &o.tournaments {
                if let Some(t) = tx.tournaments().get(id)? {
                    mine.push(t);
                }
            }
            Ok(mine)
        });
        let buttons = mine
            .iter()
            .enumerate()
            .map(|(k, t)| {
                let played = t.games.iter().filter(|g| g.score.is_some()).count();
                ui::button(format!("mine_{k}"))
                    .parent("mine_list")
                    .label(format!(
                        "{} · {} teams · {played} of {} games played",
                        t.name,
                        t.teams.len(),
                        t.games.len()
                    ))
                    .on_press(Handler::action("open").payload(t.edit.as_str()))
            })
            .collect();
        out.signal(json!({"has_mine": !mine.is_empty()}));
        Self::replace(out, &mut self.mine_ids, buttons);
    }

    fn missing(&mut self, out: &mut Outbox) {
        self.show(out, View::Missing);
    }

    fn create(&mut self, out: &mut Outbox, action: &Action) {
        if self.view != View::Home {
            return;
        }
        let fail = |out: &mut Outbox, why: &str| out.signal(feedback("failure", why));
        let name = limit(&text(action, "draft.name"), MAX_NAME_LEN);
        if name.is_empty() {
            return fail(out, "Give the tournament a name.");
        }
        let teams = match read_teams(&text(action, "draft.teams")) {
            Ok(t) => t,
            Err(why) => return fail(out, &why),
        };
        let points = match read_points(action, "draft.points") {
            Ok(p) => p,
            Err(why) => return fail(out, &why),
        };
        let games = schedule(teams.len(), flag(action, "draft.twice"));
        let me = self.me();
        let edit = store::write(&self.db, |tx| {
            let (id, edit) = new_ids(tx)?;
            tx.tournaments().insert(&Tournament {
                id: id.clone(),
                edit: edit.0.clone(),
                owner: me.clone(),
                name,
                teams,
                points,
                games,
                version: 1,
                touched: store::now(),
            })?;
            tx.edit_links().insert(&EditLink {
                id: edit.clone(),
                tournament: id.clone(),
            })?;
            let key = OwnerKey::new(me);
            let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                key,
                tournaments: Vec::new(),
            });
            o.tournaments.insert(0, id);
            o.tournaments.truncate(MAX_OWNED);
            tx.owners().upsert(&o)?;
            Ok(edit)
        });
        out.signal(json!({"draft": {"name": "", "teams": ""}}));
        self.open_board(out, &edit.0);
        out.signal(feedback(
            "success",
            "The fixtures are ready. Share the links below: one to enter results, one to follow them.",
        ));
    }

    // --- the tournament ------------------------------------------------------

    /// Opens the tournament behind the link `id`.
    fn open_board(&mut self, out: &mut Outbox, id: &str) {
        let Some((t, can_edit)) = find(&self.db, id) else {
            return self.missing(out);
        };
        if self.open.as_ref() != Some(&t.id) || self.can_edit != can_edit {
            self.show(out, View::Board);
        }
        self.open = Some(t.id.clone());
        self.can_edit = can_edit;
        self.render(out, &t);
    }

    /// Shows the open tournament as it is now again.
    fn refresh(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        match store::read(&self.db, |tx| tx.tournaments().get(&id)) {
            Some(t) => self.render(out, &t),
            None => {
                self.missing(out);
                out.signal(feedback("warning", "This tournament was deleted."));
            }
        }
    }

    fn render(&mut self, out: &mut Outbox, t: &Tournament) {
        self.seen = t.version;
        let played = t.games.iter().filter(|g| g.score.is_some()).count();
        let p = t.points;
        out.signal(json!({"t": {
            "name": t.name,
            "can_edit": self.can_edit,
            "is_owner": self.can_edit && t.owner == self.me(),
            "link": format!("{}/{}", self.origin, t.id.0),
            "edit_link": format!("{}/{}", self.origin, t.edit),
            "points": format!("Win {} · Draw {} · Loss {}", p.win, p.draw, p.loss),
            "played": format!("{played} of {} games played", t.games.len()),
            "done": played == t.games.len(),
        }}));

        if self.can_edit {
            // Only what changed is reset, so that nothing being typed is lost.
            if self.shown_points != Some(p) {
                out.signal(json!({"points": points_signal(p)}));
                self.shown_points = Some(p);
            }
            let fresh = self.shown.len() != t.games.len();
            if fresh {
                self.shown = vec![None; t.games.len()];
            }
            let mut scores = hypereffect::Map::new();
            for (k, g) in t.games.iter().enumerate() {
                if fresh || self.shown[k] != g.score {
                    scores.insert(format!("g{k}"), score_signal(g.score));
                    self.shown[k] = g.score;
                }
            }
            if !scores.is_empty() {
                out.signal(json!({ "score": scores }));
            }
        }

        if !self.toggles {
            // Every record starts hidden; null first, so that the teams of a
            // tournament open before do not linger.
            let closed: hypereffect::Map<String, Value> = (0..t.teams.len())
                .map(|k| (format!("t{k}"), false.into()))
                .collect();
            out.signal(json!({"_open": null}));
            out.signal(json!({ "_open": closed }));
            self.toggles = true;
        }
        Self::replace(out, &mut self.standing_ids, standing_items(t));

        let team = |i: u32| t.teams[i as usize].as_str();
        let mut elements = Vec::new();
        for round in 0..rounds(t) {
            let card = format!("round{round}");
            elements.push(
                ui::card(&card)
                    .parent("fixtures")
                    .title(format!("Round {}", round + 1))
                    .gap("small"),
            );
            let mut playing = vec![false; t.teams.len()];
            for (k, g) in t.games.iter().enumerate().filter(|(_, g)| g.round == round) {
                playing[g.home as usize] = true;
                playing[g.away as usize] = true;
                let id = format!("g{k}");
                if !self.can_edit {
                    let line = match g.score {
                        Some([h, a]) => format!("{}  {h} – {a}  {}", team(g.home), team(g.away)),
                        None => format!("{}  vs  {}", team(g.home), team(g.away)),
                    };
                    elements.push(ui::text(id).parent(&card).text(line));
                    continue;
                }
                elements.push(ui::row(&id).parent(&card).wrap(true));
                elements.push(
                    ui::text(format!("{id}_home"))
                        .parent(&id)
                        .text(team(g.home)),
                );
                elements.push(
                    ui::input(format!("{id}_h"))
                        .parent(&id)
                        .variant("number")
                        .bind(format!("score.{id}.h")),
                );
                elements.push(ui::text(format!("{id}_dash")).parent(&id).text("–"));
                elements.push(
                    ui::input(format!("{id}_a"))
                        .parent(&id)
                        .variant("number")
                        .bind(format!("score.{id}.a")),
                );
                elements.push(
                    ui::text(format!("{id}_away"))
                        .parent(&id)
                        .text(team(g.away)),
                );
                elements.push(
                    ui::button(format!("{id}_save"))
                        .parent(&id)
                        .label(if g.score.is_some() { "Update" } else { "Save" })
                        .variant(if g.score.is_some() {
                            "plain"
                        } else {
                            "primary"
                        })
                        .on_press(Handler::action("score").payload(k)),
                );
                if g.score.is_some() {
                    elements.push(
                        ui::badge(format!("{id}_done"))
                            .parent(&id)
                            .variant("success")
                            .text("Played"),
                    );
                }
            }
            if let Some(bye) = playing.iter().position(|p| !p) {
                elements.push(
                    ui::text(format!("{card}_bye"))
                        .parent(&card)
                        .variant("caption")
                        .text(format!("Bye: {}", t.teams[bye])),
                );
            }
        }
        Self::replace(out, &mut self.fixture_ids, elements);
    }

    /// Changes the tournament open with an edit link, if `change` says so,
    /// and shows how it is now.
    fn change(
        &mut self,
        out: &mut Outbox,
        change: impl FnOnce(&mut Tournament) -> Result<&'static str, String>,
    ) {
        let Some(id) = self.open.clone().filter(|_| self.can_edit) else {
            return;
        };
        let result = store::write(&self.db, |tx| {
            let Some(mut t) = tx.tournaments().get(&id)? else {
                return Ok(Err("This tournament no longer exists.".to_owned()));
            };
            let done = match change(&mut t) {
                Ok(done) => done,
                Err(why) => return Ok(Err(why)),
            };
            t.version += 1;
            t.touched = store::now();
            tx.tournaments().update(&t)?;
            Ok(Ok(done))
        });
        match result {
            Ok(done) => {
                self.refresh(out);
                out.signal(feedback("success", done));
            }
            Err(why) => out.signal(feedback("failure", why)),
        }
    }

    /// Saves, corrects or clears the result of the game the action names.
    fn score(&mut self, out: &mut Outbox, action: &Action) {
        let Some(k) = action.payload_as::<usize>() else {
            return;
        };
        let sig = format!("score.g{k}");
        let (h, a) = (
            text(action, &format!("{sig}.h")),
            text(action, &format!("{sig}.a")),
        );
        let score = match (h.is_empty(), a.is_empty()) {
            (true, true) => None,
            (false, false) => {
                let (Ok(h), Ok(a)) = (h.parse::<u32>(), a.parse::<u32>()) else {
                    return out.signal(feedback("failure", "Scores are whole numbers."));
                };
                if h > MAX_SCORE || a > MAX_SCORE {
                    return out
                        .signal(feedback("failure", format!("Scores go up to {MAX_SCORE}.")));
                }
                Some([h, a])
            }
            _ => {
                return out.signal(feedback(
                    "failure",
                    "Enter both scores, or clear both to undo a result.",
                ));
            }
        };
        self.change(out, |t| {
            let g = t.games.get_mut(k).ok_or("There is no such game.")?;
            g.score = score;
            Ok(if score.is_some() {
                "Result saved."
            } else {
                "Result cleared."
            })
        });
    }

    fn set_points(&mut self, out: &mut Outbox, action: &Action) {
        let points = match read_points(action, "points") {
            Ok(p) => p,
            Err(why) => return out.signal(feedback("failure", why)),
        };
        self.change(out, |t| {
            t.points = points;
            Ok("Points saved; the standings are counted again.")
        });
    }

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone().filter(|_| self.can_edit) else {
            return;
        };
        let me = self.me();
        // Its edit link goes with it, and it leaves its owner's list.
        let deleted = store::write(&self.db, |tx| {
            if tx.tournaments().get(&id)?.is_none_or(|t| t.owner != me) {
                return Ok(false);
            }
            tx.tournaments().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The tournament is deleted."));
        } else {
            out.signal(feedback(
                "failure",
                "Only its creator can delete this tournament.",
            ));
        }
    }
}

/// A fresh id for a tournament, and one for its edit link.
fn new_ids(tx: &WriteTx) -> orangedb::Result<(TournamentId, EditId)> {
    let fresh = || -> orangedb::Result<String> {
        loop {
            let id = cuid2::create_id();
            if !tx.tournaments().contains(TournamentId::new(&id))?
                && !tx.edit_links().contains(EditId::new(&id))?
            {
                return Ok(id);
            }
        }
    };
    let id = fresh()?;
    let edit = fresh()?;
    Ok((TournamentId::new(id), EditId::new(edit)))
}

impl App for Tournaments {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(
            Metadata::new()
                .name("tournament")
                .description("Round-robin fixtures and standings, with links to share"),
        );
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"name": "", "teams": "", "twice": false,
                      "points": points_signal(Points::default())},
            "t": {"name": "", "can_edit": false, "is_owner": false, "link": "",
                  "edit_link": "", "points": "", "played": "", "done": false},
            "points": points_signal(Points::default()),
            "score": {},
            "_open": {},
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("tournament"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("New tournament")
                .variant("plain")
                .visible(cel("s.view != 'home'"))
                .on_press(Handler::action("home")),
        );
        out.element(
            ui::notice("feedback")
                .parent("main")
                .variant(cel("s.feedback.kind"))
                .visible(cel("s.feedback.text != ''"))
                .text(cel("s.feedback.text")),
        );

        // The start page: a new tournament, and the ones made before.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "Type in the teams and get a round-robin schedule where everyone \
             meets everyone. Share one link to enter the results and another \
             for following the standings.",
        ));
        out.element(ui::card("new").parent("home").gap("medium"));
        out.element(
            ui::input("name_in")
                .parent("new")
                .label("Name")
                .bind("draft.name")
                .placeholder("Summer league"),
        );
        out.element(
            ui::input("teams_in")
                .parent("new")
                .label("Teams, one per line")
                .variant("multiline")
                .bind("draft.teams"),
        );
        out.element(
            ui::checkbox("twice_in")
                .parent("new")
                .label("Play each other twice, home and away")
                .variant("switch")
                .bind("draft.twice"),
        );
        Self::points_inputs(out, "new", "draft_points", "draft.points");
        out.element(
            ui::button("create_btn")
                .parent("new")
                .label("Make the fixtures")
                .variant("primary")
                .on_press(Handler::action("create")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your tournaments")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // A link to a tournament that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no tournament here. It may have been deleted."),
        );

        // A tournament.
        out.element(
            ui::column("board")
                .parent("main")
                .visible(cel("s.view == 'board'")),
        );
        out.element(
            ui::text("t_name")
                .parent("board")
                .variant("h2")
                .text(cel("s.t.name")),
        );
        out.element(ui::row("t_badges").parent("board").wrap(true));
        out.element(
            ui::badge("t_points")
                .parent("t_badges")
                .variant("info")
                .text(cel("s.t.points")),
        );
        out.element(
            ui::badge("t_played")
                .parent("t_badges")
                .variant(cel("s.t.done ? 'success' : 'neutral'"))
                .text(cel("s.t.played")),
        );
        out.element(
            ui::badge("t_mode")
                .parent("t_badges")
                .variant(cel("s.t.can_edit ? 'warning' : 'neutral'"))
                .text(cel(
                    "s.t.can_edit ? 'Anyone with this link can enter results' : 'View only'",
                )),
        );
        out.element(
            ui::card("links")
                .parent("board")
                .title("Links")
                .visible(cel("s.t.can_edit"))
                .gap("small"),
        );
        out.element(
            ui::text("edit_caption")
                .parent("links")
                .variant("caption")
                .text("To enter results (keep it to those who keep score):"),
        );
        out.element(
            ui::text("edit_link")
                .parent("links")
                .variant("code")
                .text(cel("s.t.edit_link")),
        );
        out.element(
            ui::text("view_caption")
                .parent("links")
                .variant("caption")
                .text("To follow the fixtures and standings:"),
        );
        out.element(
            ui::text("view_link")
                .parent("links")
                .variant("code")
                .text(cel("s.t.link")),
        );
        out.element(ui::tabs("tabs").parent("board"));
        out.element(ui::tab("standings").parent("tabs").label("Standings"));
        out.element(ui::list("table").parent("standings"));
        out.element(
            ui::text("table_caption")
                .parent("standings")
                .variant("caption")
                .text("Ranked by points, then goal difference, then goals scored."),
        );
        out.element(
            ui::card("points_card")
                .parent("standings")
                .title("Points")
                .visible(cel("s.t.can_edit"))
                .gap("small"),
        );
        Self::points_inputs(out, "points_card", "points", "points");
        out.element(
            ui::button("points_btn")
                .parent("points_card")
                .label("Save points")
                .on_press(Handler::action("points")),
        );
        out.element(ui::tab("fixtures").parent("tabs").label("Fixtures"));
        out.element(
            ui::button("delete_btn")
                .parent("board")
                .label("Delete tournament")
                .variant("failure")
                .visible(cel("s.t.is_owner"))
                .on_press(Handler::action("delete")),
        );

        match self.route.clone() {
            Route::Home => self.go_home(out),
            Route::Open(id) => self.open_board(out, &id),
            Route::Unknown => self.missing(out),
        }
    }

    fn action(&mut self, out: &mut Outbox, action: &Action) {
        match action.name.as_str() {
            "home" => self.go_home(out),
            "create" => self.create(out, action),
            "open" => {
                if let Some(id) = action.payload_as::<String>() {
                    self.open_board(out, &id);
                }
            }
            "score" => self.score(out, action),
            "points" => self.set_points(out, action),
            "delete" => self.delete(out),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = store::read(&self.db, |tx| tx.tournaments().get(id)).map(|t| t.version);
        if version != Some(self.seen) {
            self.refresh(out);
        }
    }

    fn tick_interval(&self) -> Option<u32> {
        self.open.is_some().then_some(WATCH_MS)
    }
}

impl Tournaments {
    /// Inputs for a win, a draw and a loss, in a row of `parent`, bound
    /// under `at`.
    fn points_inputs(out: &mut Outbox, parent: &str, prefix: &str, at: &str) {
        let row = format!("{prefix}_row");
        out.element(ui::row(&row).parent(parent).wrap(true));
        for (what, label) in [("win", "Win"), ("draw", "Draw"), ("loss", "Loss")] {
            out.element(
                ui::input(format!("{prefix}_{what}"))
                    .parent(&row)
                    .label(format!("Points for a {}", label.to_lowercase()))
                    .variant("number")
                    .bind(format!("{at}.{what}")),
            );
        }
    }
}

#[cfg(test)]
mod tests;
