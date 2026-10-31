//! Knockout brackets.
//!
//! Anyone can make one: a name and the teams, one per line. The first round
//! has as many spots as the next power of two, so 5 to 8 teams make
//! quarterfinals, 9 to 16 a round of 16, and so on, and the spots left over
//! are byes: the team facing one goes through without playing. Before the
//! bracket is made, the organizer can put any team in any spot, or a bye,
//! and the draw fills the spots left open, never leaving a match without a
//! team. A bracket has two links: `hyperb://<host>/<edit id>`, where anyone
//! can enter and correct results, and `hyperb://<host>/<id>`, which only
//! shows them. Brackets are listed on the start page of the key that made
//! them.
//!
//! A knockout match needs a winner, so a result cannot be a draw. Correcting
//! a result so that the other team goes through clears the later results
//! that team's opponent played in.
//!
//! A session watches the bracket it has open on its tick, so results
//! entered elsewhere show up within a couple of seconds.

use crate::store::{self, Bracket, BracketId, Db, EditId, EditLink, Owner, OwnerKey, WriteTx};
use hyperb_core::identity::PublicKey;
use hypereffect::{Action, App, Element, Handler, Metadata, Outbox, Value, cel, json, ui};
use std::collections::BTreeMap;
use std::sync::Arc;

/// How often an open bracket is checked for news.
const WATCH_MS: u32 = 2000;
const MIN_TEAMS: usize = 2;
const MAX_TEAMS: usize = 32;
const MAX_NAME_LEN: usize = 120;
const MAX_TEAM_LEN: usize = 40;
const MAX_SCORE: u32 = 999;
/// How many brackets a key's start page lists.
const MAX_OWNED: usize = 50;
/// How long a bracket is kept without a change.
const IDLE: u64 = 180 * 24 * 60 * 60;

/// Deletes the brackets nobody changed for [`IDLE`], and owners left with
/// none. Returns how many brackets went.
pub fn prune(db: &Db) -> usize {
    let cutoff = store::now().saturating_sub(IDLE);
    store::write(db, |tx| {
        let mut n = 0;
        for b in tx.brackets().all()? {
            if b.touched <= cutoff {
                tx.brackets().delete(&b.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.brackets.is_empty() {
                tx.owners().delete(&o.key)?;
            }
        }
        Ok(n)
    })
}

/// What the organizer chose for a spot of the first round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Left to the draw.
    Draw,
    Team(u32),
    Bye,
}

/// The first round for `teams`, two spots a match: the `picks` where they
/// were made, and the draw for the rest. There is a pick for every spot,
/// and `below(n)` is a random number under `n`.
pub fn draw(
    teams: &[String],
    picks: &[Pick],
    mut below: impl FnMut(usize) -> usize,
) -> Result<Vec<Option<u32>>, String> {
    let size = picks.len();
    let byes = size - teams.len();
    let mut placed = vec![false; teams.len()];
    for p in picks {
        if let Pick::Team(t) = *p {
            if placed[t as usize] {
                return Err(format!("“{}” is in two spots.", teams[t as usize]));
            }
            placed[t as usize] = true;
        }
    }
    if picks.iter().filter(|&&p| p == Pick::Bye).count() > byes {
        return Err(match byes {
            0 => format!("With {} teams there are no byes.", teams.len()),
            1 => "There is only one bye.".to_owned(),
            n => format!("There are only {n} byes."),
        });
    }
    let mut slots: Vec<Option<u32>> = picks
        .iter()
        .map(|p| match p {
            Pick::Team(t) => Some(*t),
            _ => None,
        })
        .collect();
    let open = |slots: &[Option<u32>], k: usize| picks[k] == Pick::Draw && slots[k].is_none();
    let mut left: Vec<u32> = (0..teams.len() as u32)
        .filter(|&t| !placed[t as usize])
        .collect();
    shuffle(&mut left, &mut below);

    // Every match needs a team, so the ones without get theirs first.
    let empty: Vec<usize> = (0..size / 2)
        .filter(|m| slots[2 * m].is_none() && slots[2 * m + 1].is_none())
        .collect();
    for &m in &empty {
        if picks[2 * m] == Pick::Bye && picks[2 * m + 1] == Pick::Bye {
            return Err(format!("Match {} has a bye in both spots.", m + 1));
        }
    }
    if empty.len() > left.len() {
        return Err(
            "Every first-round match needs a team: spread out the teams you placed.".into(),
        );
    }
    for m in empty {
        let spots: Vec<usize> = [2 * m, 2 * m + 1]
            .into_iter()
            .filter(|&k| open(&slots, k))
            .collect();
        slots[spots[below(spots.len())]] = left.pop();
    }
    let mut rest: Vec<usize> = (0..size).filter(|&k| open(&slots, k)).collect();
    shuffle(&mut rest, &mut below);
    // What is left over once the teams run out are the byes.
    for (k, t) in rest.into_iter().zip(left) {
        slots[k] = Some(t);
    }
    Ok(slots)
}

fn shuffle<T>(items: &mut [T], below: &mut impl FnMut(usize) -> usize) {
    for k in (1..items.len()).rev() {
        items.swap(k, below(k + 1));
    }
}

/// A random number under `n`.
fn below(n: usize) -> usize {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("system random number generator");
    (u64::from_le_bytes(bytes) % n as u64) as usize
}

/// One side of a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Team(u32),
    /// Nobody: the other side goes through.
    Bye,
    /// The winner of a match not played yet.
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub sides: [Side; 2],
    pub score: Option<[u32; 2]>,
}

impl Match {
    /// Who goes through, as far as it is known.
    pub fn winner(&self) -> Side {
        match (self.sides, self.score) {
            ([Side::Team(a), Side::Team(b)], Some([x, y])) if x != y => {
                Side::Team(if x > y { a } else { b })
            }
            ([t @ Side::Team(_), Side::Bye], _) | ([Side::Bye, t @ Side::Team(_)], _) => t,
            ([Side::Bye, Side::Bye], _) => Side::Bye,
            _ => Side::Waiting,
        }
    }

    pub fn is_bye(&self) -> bool {
        self.sides.contains(&Side::Bye)
    }
}

/// Every match, round by round, from the first round to the final.
pub fn matches(b: &Bracket) -> Vec<Vec<Match>> {
    let mut sides: Vec<Side> = b
        .slots
        .iter()
        .map(|s| s.map_or(Side::Bye, Side::Team))
        .collect();
    let mut rounds: Vec<Vec<Match>> = Vec::new();
    while sides.len() >= 2 {
        let r = rounds.len();
        let round: Vec<Match> = sides
            .chunks(2)
            .enumerate()
            .map(|(i, pair)| Match {
                sides: [pair[0], pair[1]],
                score: b.scores.get(r).and_then(|s| s.get(i)).copied().flatten(),
            })
            .collect();
        sides = round.iter().map(Match::winner).collect();
        rounds.push(round);
    }
    rounds
}

/// No results yet for a first round of `size` spots.
fn no_scores(size: usize) -> Vec<Vec<Option<[u32; 2]>>> {
    let mut scores = Vec::new();
    let mut n = size / 2;
    while n >= 1 {
        scores.push(vec![None; n]);
        n /= 2;
    }
    scores
}

/// "Final", "Semifinals", "Quarterfinals", then "Round of 16" and so on.
pub fn round_name(round: usize, rounds: usize) -> String {
    match rounds - round {
        1 => "Final".into(),
        2 => "Semifinals".into(),
        3 => "Quarterfinals".into(),
        n => format!("Round of {}", 1 << n),
    }
}

/// The winner of the final, once it is played.
pub fn champion(b: &Bracket) -> Option<u32> {
    match matches(b).last()?.first()?.winner() {
        Side::Team(t) => Some(t),
        _ => None,
    }
}

/// Saves or clears the result of match `i` of `round`. If that changes who
/// goes through, the later results on that team's path are cleared.
pub fn set_score(
    b: &mut Bracket,
    round: usize,
    i: usize,
    score: Option<[u32; 2]>,
) -> Result<&'static str, String> {
    let m = *matches(b)
        .get(round)
        .and_then(|r| r.get(i))
        .ok_or("There is no such match.")?;
    if m.is_bye() {
        return Err("A bye needs no result.".into());
    }
    if m.sides.contains(&Side::Waiting) {
        return Err("This match is still waiting for its teams.".into());
    }
    if score.is_some_and(|[x, y]| x == y) {
        return Err(
            "A knockout match needs a winner: enter the score after extra time or penalties."
                .into(),
        );
    }
    b.scores[round][i] = score;
    let mut cleared = false;
    if (Match { score, ..m }).winner() != m.winner() {
        let mut i = i;
        for later in &mut b.scores[round + 1..] {
            i /= 2;
            cleared |= later[i].take().is_some();
        }
    }
    Ok(match (score.is_some(), cleared) {
        (true, false) => "Result saved.",
        (false, false) => "Result cleared.",
        (true, true) => "Result saved. The later results it changed were cleared.",
        (false, true) => "Result cleared, and the later results that depended on it.",
    })
}

/// How many results are in, and how many matches there are to play.
fn progress(b: &Bracket) -> (usize, usize) {
    let played = b.scores.iter().flatten().filter(|s| s.is_some()).count();
    (played, b.teams.len().saturating_sub(1))
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
    Seed,
    Board,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Seed => "seed",
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

/// The value of the pick select for a bye; a team's is its index.
const BYE: &str = "bye";

/// The pick the action carries for spot `k`.
fn read_pick(action: &Action, k: usize, teams: usize) -> Pick {
    let v = text(action, &format!("seed.s{k}"));
    if v == BYE {
        return Pick::Bye;
    }
    match v.parse::<u32>() {
        Ok(t) if (t as usize) < teams => Pick::Team(t),
        _ => Pick::Draw,
    }
}

fn score_signal(score: Option<[u32; 2]>) -> Value {
    match score {
        Some([h, a]) => json!({"h": h.to_string(), "a": a.to_string()}),
        None => json!({"h": "", "a": ""}),
    }
}

/// The bracket a link opens, and whether the link can enter results.
fn find(db: &Db, id: &str) -> Option<(Bracket, bool)> {
    store::read(db, |tx| {
        if let Some(b) = tx.brackets().get(BracketId::new(id))? {
            return Ok(Some((b, false)));
        }
        let Some(link) = tx.edit_links().get(EditId::new(id))? else {
            return Ok(None);
        };
        Ok(tx.brackets().get(&link.bracket)?.map(|b| (b, true)))
    })
}

/// The round and place in it of match `g`, counting through all the rounds.
fn locate(b: &Bracket, mut g: usize) -> Option<(usize, usize)> {
    for (r, round) in b.scores.iter().enumerate() {
        if g < round.len() {
            return Some((r, g));
        }
        g -= round.len();
    }
    None
}

/// A bracket being made: its name and teams, while the organizer places
/// them.
struct Draft {
    name: String,
    teams: Vec<String>,
}

pub struct Brackets {
    db: Arc<Db>,
    /// The key of this session.
    viewer: PublicKey,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    draft: Option<Draft>,
    /// The bracket open in this session, whether this session can enter
    /// results, and the version on screen.
    open: Option<BracketId>,
    can_edit: bool,
    seen: u64,
    /// The scores on screen, by match, so that a refresh only resets the
    /// inputs of the matches whose result changed.
    shown: Vec<Option<[u32; 2]>>,
    /// Ids of the elements that are replaced as a whole.
    mine_ids: Vec<String>,
    seed_ids: Vec<String>,
    round_ids: Vec<String>,
}

impl Brackets {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Brackets {
            db,
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            draft: None,
            open: None,
            can_edit: false,
            seen: 0,
            shown: Vec::new(),
            mine_ids: Vec::new(),
            seed_ids: Vec::new(),
            round_ids: Vec::new(),
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
        Self::replace(out, &mut self.seed_ids, Vec::new());
        Self::replace(out, &mut self.round_ids, Vec::new());
        self.open = None;
        self.can_edit = false;
        self.shown.clear();
        self.view = view;
        out.signal(json!({"view": view.name(), "feedback": {"kind": "info", "text": ""}}));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let mine: Vec<Bracket> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut mine = Vec::new();
            for id in &o.brackets {
                if let Some(b) = tx.brackets().get(id)? {
                    mine.push(b);
                }
            }
            Ok(mine)
        });
        let buttons = mine
            .iter()
            .enumerate()
            .map(|(k, b)| {
                let (played, total) = progress(b);
                ui::button(format!("mine_{k}"))
                    .parent("mine_list")
                    .label(format!(
                        "{} · {} teams · {played} of {total} matches played",
                        b.name,
                        b.teams.len(),
                    ))
                    .on_press(Handler::action("open").payload(b.edit.as_str()))
            })
            .collect();
        out.signal(json!({"has_mine": !mine.is_empty()}));
        Self::replace(out, &mut self.mine_ids, buttons);
    }

    fn missing(&mut self, out: &mut Outbox) {
        self.show(out, View::Missing);
    }

    /// Checks the name and the teams, and moves on to placing them.
    fn next(&mut self, out: &mut Outbox, action: &Action) {
        if self.view != View::Home {
            return;
        }
        let name = limit(&text(action, "draft.name"), MAX_NAME_LEN);
        if name.is_empty() {
            return out.signal(feedback("failure", "Give the bracket a name."));
        }
        let teams = match read_teams(&text(action, "draft.teams")) {
            Ok(t) => t,
            Err(why) => return out.signal(feedback("failure", why)),
        };
        self.show(out, View::Seed);
        let size = teams.len().next_power_of_two();
        let byes = size - teams.len();
        let rounds = size.trailing_zeros() as usize;
        let mut info = format!(
            "{} teams make a bracket starting with the {}.",
            teams.len(),
            round_name(0, rounds).to_lowercase()
        );
        match byes {
            0 => {}
            1 => info.push_str(" One team gets a bye and goes straight to the next round."),
            n => info.push_str(&format!(
                " {n} teams get a bye and go straight to the next round."
            )),
        }
        info.push_str(
            " Choose a team for any spot you want to fill; the spots left on Draw are filled at random.",
        );

        let mut options = vec![json!({"value": "", "label": "Draw"})];
        if byes > 0 {
            options.push(json!({"value": BYE, "label": "Bye"}));
        }
        for (k, t) in teams.iter().enumerate() {
            options.push(json!({"value": k.to_string(), "label": t}));
        }
        let options = Value::Array(options);
        let picks: hypereffect::Map<String, Value> =
            (0..size).map(|k| (format!("s{k}"), "".into())).collect();
        // Null first, so that the spots of a larger bracket made before do
        // not linger.
        out.signal(json!({"seed": null}));
        out.signal(json!({"seed": picks, "seed_info": info}));
        let mut elements = Vec::new();
        for m in 0..size / 2 {
            let card = format!("pm{m}");
            elements.push(
                ui::card(&card)
                    .parent("seed_matches")
                    .title(format!("Match {}", m + 1))
                    .gap("small"),
            );
            let row = format!("{card}_row");
            elements.push(ui::row(&row).parent(&card).wrap(true));
            for k in [2 * m, 2 * m + 1] {
                elements.push(
                    ui::select(format!("pick{k}"))
                        .parent(&row)
                        .options(options.clone())
                        .bind(format!("seed.s{k}")),
                );
            }
        }
        Self::replace(out, &mut self.seed_ids, elements);
        self.draft = Some(Draft { name, teams });
    }

    /// Back to the start page, where the name and the teams are as typed.
    fn back(&mut self, out: &mut Outbox) {
        if self.view == View::Seed {
            self.draft = None;
            self.go_home(out);
        }
    }

    fn create(&mut self, out: &mut Outbox, action: &Action) {
        let Some(draft) = self.draft.as_ref().filter(|_| self.view == View::Seed) else {
            return;
        };
        let size = draft.teams.len().next_power_of_two();
        let picks: Vec<Pick> = (0..size)
            .map(|k| read_pick(action, k, draft.teams.len()))
            .collect();
        let slots = match draw(&draft.teams, &picks, below) {
            Ok(s) => s,
            Err(why) => return out.signal(feedback("failure", why)),
        };
        let Some(Draft { name, teams }) = self.draft.take() else {
            return;
        };
        let me = self.me();
        let edit = store::write(&self.db, |tx| {
            let (id, edit) = new_ids(tx)?;
            tx.brackets().insert(&Bracket {
                id: id.clone(),
                edit: edit.0.clone(),
                owner: me.clone(),
                name,
                teams,
                slots,
                scores: no_scores(size),
                version: 1,
                touched: store::now(),
            })?;
            tx.edit_links().insert(&EditLink {
                id: edit.clone(),
                bracket: id.clone(),
            })?;
            let key = OwnerKey::new(me);
            let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                key,
                brackets: Vec::new(),
            });
            o.brackets.insert(0, id);
            o.brackets.truncate(MAX_OWNED);
            tx.owners().upsert(&o)?;
            Ok(edit)
        });
        out.signal(json!({"draft": {"name": "", "teams": ""}}));
        self.open_board(out, &edit.0);
        out.signal(feedback(
            "success",
            "The bracket is ready. Share the links below: one to enter results, one to follow them.",
        ));
    }

    // --- the bracket -------------------------------------------------------

    /// Opens the bracket behind the link `id`.
    fn open_board(&mut self, out: &mut Outbox, id: &str) {
        let Some((b, can_edit)) = find(&self.db, id) else {
            return self.missing(out);
        };
        if self.open.as_ref() != Some(&b.id) || self.can_edit != can_edit {
            self.show(out, View::Board);
        }
        self.open = Some(b.id.clone());
        self.can_edit = can_edit;
        self.render(out, &b);
    }

    /// Shows the open bracket as it is now again.
    fn refresh(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        match store::read(&self.db, |tx| tx.brackets().get(&id)) {
            Some(b) => self.render(out, &b),
            None => {
                self.missing(out);
                out.signal(feedback("warning", "This bracket was deleted."));
            }
        }
    }

    fn render(&mut self, out: &mut Outbox, b: &Bracket) {
        self.seen = b.version;
        let (played, total) = progress(b);
        let champion = champion(b).map(|t| format!("Champion: {}", b.teams[t as usize]));
        out.signal(json!({"t": {
            "name": b.name,
            "can_edit": self.can_edit,
            "is_owner": self.can_edit && b.owner == self.me(),
            "link": format!("{}/{}", self.origin, b.id.0),
            "edit_link": format!("{}/{}", self.origin, b.edit),
            "teams": format!("{} teams", b.teams.len()),
            "played": format!("{played} of {total} matches played"),
            "done": played == total,
            "champion": champion.unwrap_or_default(),
        }}));

        let all: Vec<Option<[u32; 2]>> = b.scores.iter().flatten().copied().collect();
        if self.can_edit {
            // Only what changed is reset, so that nothing being typed is lost.
            let fresh = self.shown.len() != all.len();
            if fresh {
                self.shown = vec![None; all.len()];
            }
            let mut scores = hypereffect::Map::new();
            for (g, score) in all.iter().enumerate() {
                if fresh || self.shown[g] != *score {
                    scores.insert(format!("g{g}"), score_signal(*score));
                    self.shown[g] = *score;
                }
            }
            if !scores.is_empty() {
                out.signal(json!({ "score": scores }));
            }
        }

        let rounds = matches(b);
        let team = |t: u32| b.teams[t as usize].as_str();
        let mut elements = Vec::new();
        // The number of the first match of the round, counting from 0.
        let mut first = 0;
        for (r, round) in rounds.iter().enumerate() {
            let card = format!("round{r}");
            elements.push(
                ui::card(&card)
                    .parent("rounds")
                    .title(round_name(r, rounds.len()))
                    .gap("small"),
            );
            let before = first - if r > 0 { rounds[r - 1].len() } else { 0 };
            let side = |i: usize, s: usize| match round[i].sides[s] {
                Side::Team(t) => team(t).to_owned(),
                Side::Bye => "bye".to_owned(),
                Side::Waiting => format!("winner of match {}", before + 2 * i + s + 1),
            };
            for (i, m) in round.iter().enumerate() {
                let g = first + i;
                let id = format!("g{g}");
                let n = g + 1;
                match m.sides {
                    [Side::Team(t), Side::Bye] | [Side::Bye, Side::Team(t)] => {
                        elements.push(
                            ui::text(id)
                                .parent(&card)
                                .variant("caption")
                                .text(format!("{n}. {} goes through with a bye", team(t))),
                        );
                    }
                    [Side::Team(h), Side::Team(a)] if self.can_edit => {
                        elements.extend(self.score_row(&card, g, team(h), team(a), m.score));
                    }
                    [Side::Team(h), Side::Team(a)] => {
                        let line = match m.score {
                            Some([x, y]) => format!("{n}. {}  {x} – {y}  {}", team(h), team(a)),
                            None => format!("{n}. {}  vs  {}", team(h), team(a)),
                        };
                        elements.push(ui::text(id).parent(&card).text(line));
                    }
                    _ => {
                        elements.push(ui::text(id).parent(&card).text(format!(
                            "{n}. {}  vs  {}",
                            side(i, 0),
                            side(i, 1)
                        )));
                    }
                }
            }
            first += round.len();
        }
        Self::replace(out, &mut self.round_ids, elements);
    }

    /// The inputs for the result of match `g`, between `home` and `away`.
    fn score_row(
        &self,
        card: &str,
        g: usize,
        home: &str,
        away: &str,
        score: Option<[u32; 2]>,
    ) -> Vec<Element> {
        let id = format!("g{g}");
        let mut elements = vec![
            ui::row(&id).parent(card).wrap(true),
            ui::text(format!("{id}_n"))
                .parent(&id)
                .variant("caption")
                .text(format!("{}.", g + 1)),
            ui::text(format!("{id}_home")).parent(&id).text(home),
            ui::input(format!("{id}_h"))
                .parent(&id)
                .variant("number")
                .bind(format!("score.{id}.h")),
            ui::text(format!("{id}_dash")).parent(&id).text("–"),
            ui::input(format!("{id}_a"))
                .parent(&id)
                .variant("number")
                .bind(format!("score.{id}.a")),
            ui::text(format!("{id}_away")).parent(&id).text(away),
            ui::button(format!("{id}_save"))
                .parent(&id)
                .label(if score.is_some() { "Update" } else { "Save" })
                .variant(if score.is_some() { "plain" } else { "primary" })
                .on_press(Handler::action("score").payload(g)),
        ];
        if score.is_some() {
            elements.push(
                ui::badge(format!("{id}_done"))
                    .parent(&id)
                    .variant("success")
                    .text("Played"),
            );
        }
        elements
    }

    /// Saves, corrects or clears the result of the match the action names.
    fn score(&mut self, out: &mut Outbox, action: &Action) {
        let Some(g) = action.payload_as::<usize>() else {
            return;
        };
        let Some(id) = self.open.clone().filter(|_| self.can_edit) else {
            return;
        };
        let sig = format!("score.g{g}");
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
        let result = store::write(&self.db, |tx| {
            let Some(mut b) = tx.brackets().get(&id)? else {
                return Ok(Err("This bracket no longer exists.".to_owned()));
            };
            let Some((round, i)) = locate(&b, g) else {
                return Ok(Err("There is no such match.".to_owned()));
            };
            let done = match set_score(&mut b, round, i, score) {
                Ok(done) => done,
                Err(why) => return Ok(Err(why)),
            };
            b.version += 1;
            b.touched = store::now();
            tx.brackets().update(&b)?;
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

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone().filter(|_| self.can_edit) else {
            return;
        };
        let me = self.me();
        // Its edit link goes with it, and it leaves its owner's list.
        let deleted = store::write(&self.db, |tx| {
            if tx.brackets().get(&id)?.is_none_or(|b| b.owner != me) {
                return Ok(false);
            }
            tx.brackets().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The bracket is deleted."));
        } else {
            out.signal(feedback(
                "failure",
                "Only its creator can delete this bracket.",
            ));
        }
    }
}

/// A fresh id for a bracket, and one for its edit link.
fn new_ids(tx: &WriteTx) -> orangedb::Result<(BracketId, EditId)> {
    let fresh = || -> orangedb::Result<String> {
        loop {
            let id = cuid2::create_id();
            if !tx.brackets().contains(BracketId::new(&id))?
                && !tx.edit_links().contains(EditId::new(&id))?
            {
                return Ok(id);
            }
        }
    };
    let id = fresh()?;
    let edit = fresh()?;
    Ok((BracketId::new(id), EditId::new(edit)))
}

impl App for Brackets {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(
            Metadata::new()
                .name("bracket")
                .description("Knockout brackets, with links to share"),
        );
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"name": "", "teams": ""},
            "seed": {},
            "seed_info": "",
            "t": {"name": "", "can_edit": false, "is_owner": false, "link": "",
                  "edit_link": "", "teams": "", "played": "", "done": false,
                  "champion": ""},
            "score": {},
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("bracket"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("New bracket")
                .variant("plain")
                .visible(cel("s.view == 'board' || s.view == 'missing'"))
                .on_press(Handler::action("home")),
        );
        out.element(
            ui::notice("feedback")
                .parent("main")
                .variant(cel("s.feedback.kind"))
                .visible(cel("s.feedback.text != ''"))
                .text(cel("s.feedback.text")),
        );

        // The start page: a new bracket, and the ones made before.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "Type in the teams, place the ones you want in the first round and \
             let the draw place the rest. Share one link to enter the results \
             and another for following the bracket.",
        ));
        out.element(ui::card("new").parent("home").gap("medium"));
        out.element(
            ui::input("name_in")
                .parent("new")
                .label("Name")
                .bind("draft.name")
                .placeholder("Spring cup"),
        );
        out.element(
            ui::input("teams_in")
                .parent("new")
                .label("Teams, one per line")
                .variant("multiline")
                .bind("draft.teams"),
        );
        out.element(
            ui::button("next_btn")
                .parent("new")
                .label("Next: place the teams")
                .variant("primary")
                .on_press(Handler::action("next")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your brackets")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // Placing the teams in the first round.
        out.element(
            ui::column("seed")
                .parent("main")
                .visible(cel("s.view == 'seed'")),
        );
        out.element(
            ui::text("seed_title")
                .parent("seed")
                .variant("h2")
                .text(cel("s.draft.name")),
        );
        out.element(
            ui::text("seed_info")
                .parent("seed")
                .text(cel("s.seed_info")),
        );
        out.element(ui::column("seed_matches").parent("seed").gap("small"));
        out.element(ui::row("seed_btns").parent("seed").wrap(true));
        out.element(
            ui::button("back_btn")
                .parent("seed_btns")
                .label("Back")
                .variant("plain")
                .on_press(Handler::action("back")),
        );
        out.element(
            ui::button("create_btn")
                .parent("seed_btns")
                .label("Make the bracket")
                .variant("primary")
                .on_press(Handler::action("create")),
        );

        // A link to a bracket that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no bracket here. It may have been deleted."),
        );

        // A bracket.
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
            ui::badge("t_teams")
                .parent("t_badges")
                .variant("info")
                .text(cel("s.t.teams")),
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
            ui::notice("t_champion")
                .parent("board")
                .variant("success")
                .visible(cel("s.t.champion != ''"))
                .text(cel("s.t.champion")),
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
                .text("To follow the bracket:"),
        );
        out.element(
            ui::text("view_link")
                .parent("links")
                .variant("code")
                .text(cel("s.t.link")),
        );
        out.element(ui::column("rounds").parent("board"));
        out.element(
            ui::text("rounds_caption")
                .parent("board")
                .variant("caption")
                .text("A knockout match needs a winner: enter the score after extra time or penalties."),
        );
        out.element(
            ui::button("delete_btn")
                .parent("board")
                .label("Delete bracket")
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
            "next" => self.next(out, action),
            "back" => self.back(out),
            "create" => self.create(out, action),
            "open" => {
                if let Some(id) = action.payload_as::<String>() {
                    self.open_board(out, &id);
                }
            }
            "score" => self.score(out, action),
            "delete" => self.delete(out),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = store::read(&self.db, |tx| tx.brackets().get(id)).map(|b| b.version);
        if version != Some(self.seen) {
            self.refresh(out);
        }
    }

    fn tick_interval(&self) -> Option<u32> {
        self.open.is_some().then_some(WATCH_MS)
    }
}

#[cfg(test)]
mod tests;
