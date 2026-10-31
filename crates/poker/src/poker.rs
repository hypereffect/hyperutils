//! Planning poker.
//!
//! A manager opens a room and shares its link, `hyperb://<host>/<id>`.
//! Whoever opens the link types a name and joins; the manager may join too,
//! to vote along. The manager starts each round, saying what is being
//! estimated if they like. Every player then types a number. Who has voted
//! is shown to all, but not what: the numbers stay on the server until
//! everyone in the room has voted, or the manager reveals the round early.
//! Then everyone sees all of them at once, with the average and the spread.
//!
//! People are recognized by their key: whoever comes back with the same key
//! is still in the room, with their vote, and only the key that opened a
//! room can run its rounds.
//!
//! A session watches the room it has open on its tick, so votes and reveals
//! show up within a second.

use crate::store::{
    self, Db, Owner, OwnerKey, Player, Room, RoomId, Round, Summary, Vote, WriteTx,
};
use hyperb_core::identity::PublicKey;
use hypereffect::{Action, App, Element, Handler, Metadata, Outbox, Value, cel, json, ui};
use std::collections::BTreeMap;
use std::sync::Arc;

/// How often an open room is checked for news.
const WATCH_MS: u32 = 1000;
const MAX_PLAYERS: usize = 50;
const MAX_TITLE_LEN: usize = 80;
const MAX_TOPIC_LEN: usize = 120;
const MAX_NAME_LEN: usize = 40;
/// The largest number a vote may be.
const MAX_VOTE: f64 = 10_000.0;
/// How many past rounds a room keeps.
const MAX_HISTORY: usize = 50;
/// How many rooms a key's start page lists.
const MAX_OWNED: usize = 50;
/// How long a room is kept without a change.
const IDLE: u64 = 30 * 24 * 60 * 60;
const DEFAULT_TITLE: &str = "Planning poker";

/// Deletes the rooms nobody used for [`IDLE`], and owners left with none.
/// Returns how many rooms went.
pub fn prune(db: &Db) -> usize {
    let cutoff = store::now().saturating_sub(IDLE);
    store::write(db, |tx| {
        let mut n = 0;
        for r in tx.rooms().all()? {
            if r.touched <= cutoff {
                tx.rooms().delete(&r.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.rooms.is_empty() {
                tx.owners().delete(&o.key)?;
            }
        }
        Ok(n)
    })
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
    Room,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Room => "room",
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

/// A vote as typed, or what is wrong with it. A decimal comma is taken.
fn read_vote(s: &str) -> Result<f64, String> {
    let why = || format!("A vote is a number from 0 to {}.", number(MAX_VOTE));
    if s.is_empty() {
        return Err("Type a number first.".to_owned());
    }
    match s.replace(',', ".").parse::<f64>() {
        Ok(v) if v.is_finite() && (0.0..=MAX_VOTE).contains(&v) => Ok(v),
        _ => Err(why()),
    }
}

/// A number with at most two decimals, and none if it is whole.
fn number(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The average, median and range of the votes, or that everyone agrees.
fn stats(votes: &[Vote]) -> String {
    let mut values: Vec<f64> = votes.iter().map(|v| v.value).collect();
    if values.is_empty() {
        return "Nobody voted.".to_owned();
    }
    values.sort_by(f64::total_cmp);
    let (low, high) = (values[0], values[values.len() - 1]);
    let n = values.len();
    let votes = format!("{n} vote{}", if n == 1 { "" } else { "s" });
    if low == high {
        return match n {
            1 => format!("{} ({votes})", number(low)),
            _ => format!("Everyone said {} ({votes})", number(low)),
        };
    }
    let average = values.iter().sum::<f64>() / n as f64;
    let median = if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    };
    format!(
        "Average {} · median {} · from {} to {} ({votes})",
        number(average),
        number(median),
        number(low),
        number(high)
    )
}

/// “Round 3” or “Round 3 · Login page”.
fn heading(number: u32, topic: &str) -> String {
    if topic.is_empty() {
        format!("Round {number}")
    } else {
        format!("Round {number} · {topic}")
    }
}

/// Reveals the round if everyone in the room has voted.
fn reveal_if_done(r: &mut Room) {
    let Some(round) = r.round.as_mut().filter(|x| !x.revealed) else {
        return;
    };
    let all = !r.players.is_empty()
        && r.players
            .iter()
            .all(|p| round.votes.iter().any(|v| v.key == p.key));
    if all {
        round.revealed = true;
    }
}

fn must_own(r: &Room, me: &str) -> Result<(), String> {
    if r.owner == me {
        Ok(())
    } else {
        Err("Only the manager of this room can do that.".to_owned())
    }
}

pub struct Poker {
    db: Arc<Db>,
    /// The key of this session.
    viewer: PublicKey,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    /// The room open in this session, and the version on screen.
    open: Option<RoomId>,
    seen: u64,
    /// The round the vote input was last filled for, so that a refresh does
    /// not wipe a number being typed.
    shown_round: Option<u32>,
    /// Ids of the elements that are replaced as a whole.
    mine_ids: Vec<String>,
    player_ids: Vec<String>,
    history_ids: Vec<String>,
}

impl Poker {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Poker {
            db,
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            open: None,
            seen: 0,
            shown_round: None,
            mine_ids: Vec::new(),
            player_ids: Vec::new(),
            history_ids: Vec::new(),
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
        Self::replace(out, &mut self.player_ids, Vec::new());
        Self::replace(out, &mut self.history_ids, Vec::new());
        self.open = None;
        self.shown_round = None;
        self.view = view;
        out.signal(json!({
            "view": view.name(),
            "feedback": {"kind": "info", "text": ""},
            "vote": "",
        }));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let mine: Vec<Room> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut mine = Vec::new();
            for id in &o.rooms {
                if let Some(r) = tx.rooms().get(id)? {
                    mine.push(r);
                }
            }
            Ok(mine)
        });
        let buttons = mine
            .iter()
            .enumerate()
            .map(|(k, r)| {
                let n = r.players.len();
                ui::button(format!("mine_{k}"))
                    .parent("mine_list")
                    .label(format!(
                        "{} · {n} player{}",
                        r.title,
                        if n == 1 { "" } else { "s" }
                    ))
                    .on_press(Handler::action("open").payload(r.id.0.as_str()))
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
        let mut title = limit(&text(action, "draft.title"), MAX_TITLE_LEN);
        if title.is_empty() {
            title = DEFAULT_TITLE.to_owned();
        }
        let me = self.me();
        let id = store::write(&self.db, |tx| {
            let id = new_id(tx)?;
            tx.rooms().insert(&Room {
                id: id.clone(),
                owner: me.clone(),
                title,
                players: Vec::new(),
                round: None,
                history: Vec::new(),
                next_id: 0,
                version: 1,
                touched: store::now(),
            })?;
            let key = OwnerKey::new(me);
            let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                key,
                rooms: Vec::new(),
            });
            o.rooms.insert(0, id.clone());
            o.rooms.truncate(MAX_OWNED);
            tx.owners().upsert(&o)?;
            Ok(id)
        });
        out.signal(json!({"draft": {"title": ""}}));
        self.open_room(out, &id.0);
        out.signal(feedback(
            "success",
            "The room is open. Send the link to the players, and start a round once they are in.",
        ));
    }

    // --- the room ----------------------------------------------------------

    /// Opens the room behind the link `id`.
    fn open_room(&mut self, out: &mut Outbox, id: &str) {
        let Some(r) = store::read(&self.db, |tx| tx.rooms().get(RoomId::new(id))) else {
            return self.missing(out);
        };
        if self.open.as_ref() != Some(&r.id) {
            self.show(out, View::Room);
        }
        self.open = Some(r.id.clone());
        self.render(out, &r);
    }

    /// Shows the open room as it is now again.
    fn refresh(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        match store::read(&self.db, |tx| tx.rooms().get(&id)) {
            Some(r) => self.render(out, &r),
            None => {
                self.missing(out);
                out.signal(feedback("warning", "The manager closed this room."));
            }
        }
    }

    fn render(&mut self, out: &mut Outbox, r: &Room) {
        self.seen = r.version;
        let me = self.me();
        let is_owner = r.owner == me;
        let player = r.players.iter().find(|p| p.key == me);
        let round = r.round.as_ref();
        let phase = match round {
            None => "waiting",
            Some(x) if x.revealed => "revealed",
            Some(_) => "voting",
        };
        let voted = |key: &str| round.and_then(|x| x.votes.iter().find(|v| v.key == key));
        let mine = voted(&me);
        let count = round.map_or(0, |x| {
            r.players
                .iter()
                .filter(|p| x.votes.iter().any(|v| v.key == p.key))
                .count()
        });

        // A new round empties the vote input, and with it any old notice.
        let number_now = round.map(|x| x.number);
        if self.shown_round != number_now {
            let vote = mine
                .filter(|_| phase == "voting")
                .map(|v| number(v.value))
                .unwrap_or_default();
            out.signal(json!({"vote": vote, "feedback": {"kind": "info", "text": ""}}));
            self.shown_round = number_now;
        }

        let next = round.map_or(1, |x| x.number + 1);
        out.signal(json!({"room": {
            "title": r.title,
            "link": format!("{}/{}", self.origin, r.id.0),
            "is_owner": is_owner,
            "joined": player.is_some(),
            "me": player.map_or(String::new(), |p| format!("You are in as {}.", p.name)),
            "phase": phase,
            "heading": round.map_or(String::new(), |x| heading(x.number, &x.topic)),
            "progress": format!("{count} of {} voted", r.players.len()),
            "mine": match mine {
                Some(v) => format!(
                    "Your vote: {}. Nobody sees it until the round is revealed; you can still change it.",
                    number(v.value)
                ),
                None => "Nobody sees your number until everyone has voted or the manager reveals the round.".to_owned(),
            },
            "result": round.filter(|x| x.revealed).map_or(String::new(), |x| stats(&x.votes)),
            "start": if round.is_none() {
                "Start the first round".to_owned()
            } else {
                format!("Start round {next}")
            },
            "has_history": !r.history.is_empty(),
        }}));

        // Who is in, and what they voted once it is revealed. Votes of people
        // who left since are shown too.
        let mut elements = Vec::new();
        let mut row = |id: String, name: String, badge: (String, &str), remove: Option<u32>| {
            elements.push(ui::row(&id).parent("players").justify("between").wrap(true));
            elements.push(ui::text(format!("{id}_name")).parent(&id).text(name));
            let right = format!("{id}_right");
            elements.push(ui::row(&right).parent(&id).gap("small"));
            elements.push(
                ui::badge(format!("{id}_vote"))
                    .parent(&right)
                    .variant(badge.1)
                    .text(badge.0),
            );
            if let Some(pid) = remove {
                elements.push(
                    ui::button(format!("{id}_remove"))
                        .parent(&right)
                        .label("Remove")
                        .variant("plain")
                        .on_press(Handler::action("remove").payload(pid)),
                );
            }
        };
        for p in &r.players {
            let name = if p.key == me {
                format!("{} (you)", p.name)
            } else {
                p.name.clone()
            };
            let badge = match (phase, voted(&p.key)) {
                ("waiting", _) => ("in".to_owned(), "neutral"),
                ("voting", Some(_)) => ("voted".to_owned(), "success"),
                ("voting", None) => ("thinking…".to_owned(), "neutral"),
                (_, Some(v)) => (number(v.value), "info"),
                (_, None) => ("no vote".to_owned(), "neutral"),
            };
            row(format!("p{}", p.id), name, badge, is_owner.then_some(p.id));
        }
        if let Some(x) = round.filter(|x| x.revealed) {
            let gone = x
                .votes
                .iter()
                .filter(|v| !r.players.iter().any(|p| p.key == v.key));
            for (k, v) in gone.enumerate() {
                row(
                    format!("gone{k}"),
                    format!("{} (left)", v.name),
                    (number(v.value), "info"),
                    None,
                );
            }
        }
        Self::replace(out, &mut self.player_ids, elements);

        let lines = r
            .history
            .iter()
            .enumerate()
            .map(|(k, h)| {
                ui::text(format!("past{k}")).parent("history").text(format!(
                    "{}: {}",
                    heading(h.number, &h.topic),
                    h.result
                ))
            })
            .collect();
        Self::replace(out, &mut self.history_ids, lines);
    }

    /// Changes the open room, if `change` says so, and shows how it is now.
    /// `change` gets the room and the viewer's key, and returns what to tell
    /// them. Returns whether it changed.
    fn change(
        &mut self,
        out: &mut Outbox,
        change: impl FnOnce(&mut Room, &str) -> Result<String, String>,
    ) -> bool {
        let Some(id) = self.open.clone() else {
            return false;
        };
        let me = self.me();
        let result = store::write(&self.db, |tx| {
            let Some(mut r) = tx.rooms().get(&id)? else {
                return Ok(Err("This room no longer exists.".to_owned()));
            };
            let done = match change(&mut r, &me) {
                Ok(done) => done,
                Err(why) => return Ok(Err(why)),
            };
            reveal_if_done(&mut r);
            r.version += 1;
            r.touched = store::now();
            tx.rooms().update(&r)?;
            Ok(Ok(done))
        });
        match result {
            Ok(done) => {
                self.refresh(out);
                out.signal(feedback("success", done));
                true
            }
            Err(why) => {
                // It may have been refused over something this screen did not
                // show yet.
                self.refresh(out);
                out.signal(feedback("failure", why));
                false
            }
        }
    }

    /// Joins the room under the name typed in, or renames the player.
    fn join(&mut self, out: &mut Outbox, action: &Action) {
        let name = limit(&text(action, "name"), MAX_NAME_LEN);
        if name.is_empty() {
            return out.signal(feedback("failure", "Type your name first."));
        }
        self.change(out, |r, me| {
            if let Some(p) = r.players.iter_mut().find(|p| p.key == me) {
                p.name = name.clone();
                return Ok(format!("You are now {name}."));
            }
            if r.players.len() >= MAX_PLAYERS {
                return Err(format!("This room is full: {MAX_PLAYERS} players."));
            }
            r.players.push(Player {
                id: r.next_id,
                key: me.to_owned(),
                name: name.clone(),
            });
            r.next_id += 1;
            Ok(format!("Welcome, {name}."))
        });
    }

    fn vote(&mut self, out: &mut Outbox, action: &Action) {
        let value = read_vote(&text(action, "vote"));
        self.change(out, |r, me| {
            let value = value?;
            let name = r
                .players
                .iter()
                .find(|p| p.key == me)
                .ok_or("Join the room to vote.")?
                .name
                .clone();
            let round = r
                .round
                .as_mut()
                .filter(|x| !x.revealed)
                .ok_or("There is no round to vote in right now.")?;
            round.votes.retain(|v| v.key != me);
            round.votes.push(Vote {
                key: me.to_owned(),
                name,
                value,
            });
            Ok(format!("You voted {}.", number(value)))
        });
    }

    fn start(&mut self, out: &mut Outbox, action: &Action) {
        let topic = limit(&text(action, "next.topic"), MAX_TOPIC_LEN);
        let started = self.change(out, |r, me| {
            must_own(r, me)?;
            let number = match r.round.take() {
                None => 1,
                Some(x) if !x.revealed => {
                    r.round = Some(x);
                    return Err("Reveal this round before starting the next.".to_owned());
                }
                Some(x) => {
                    r.history.insert(
                        0,
                        Summary {
                            number: x.number,
                            topic: x.topic,
                            result: stats(&x.votes),
                        },
                    );
                    r.history.truncate(MAX_HISTORY);
                    x.number + 1
                }
            };
            r.round = Some(Round {
                number,
                topic: topic.clone(),
                votes: Vec::new(),
                revealed: false,
            });
            Ok(format!("{} has started.", heading(number, &topic)))
        });
        if started {
            out.signal(json!({"next": {"topic": ""}}));
        }
    }

    fn reveal(&mut self, out: &mut Outbox) {
        self.change(out, |r, me| {
            must_own(r, me)?;
            let round = r
                .round
                .as_mut()
                .filter(|x| !x.revealed)
                .ok_or("There is no round to reveal.")?;
            round.revealed = true;
            Ok("The votes are revealed.".to_owned())
        });
    }

    /// Takes a player out of the room, for one who went away mid-round.
    fn remove(&mut self, out: &mut Outbox, action: &Action) {
        let Some(id) = action.payload_as::<u32>() else {
            return;
        };
        self.change(out, |r, me| {
            must_own(r, me)?;
            let k = r
                .players
                .iter()
                .position(|p| p.id == id)
                .ok_or("That player already left.")?;
            let gone = r.players.remove(k);
            // A vote not yet revealed goes with them.
            if let Some(round) = r.round.as_mut().filter(|x| !x.revealed) {
                round.votes.retain(|v| v.key != gone.key);
            }
            Ok(format!("{} is out of the room.", gone.name))
        });
    }

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        let me = self.me();
        // It leaves its owner's list by itself.
        let deleted = store::write(&self.db, |tx| {
            if tx.rooms().get(&id)?.is_none_or(|r| r.owner != me) {
                return Ok(false);
            }
            tx.rooms().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The room is closed."));
        } else {
            out.signal(feedback(
                "failure",
                "Only the manager of this room can do that.",
            ));
        }
    }
}

/// A fresh id for a room.
fn new_id(tx: &WriteTx) -> orangedb::Result<RoomId> {
    loop {
        let id = RoomId::new(cuid2::create_id());
        if !tx.rooms().contains(&id)? {
            return Ok(id);
        }
    }
}

impl App for Poker {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(Metadata::new().name("poker").description(
            "Planning poker: everyone estimates, nobody sees a number until all are in",
        ));
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"title": ""},
            "room": {"title": "", "link": "", "is_owner": false, "joined": false, "me": "",
                     "phase": "waiting", "heading": "", "progress": "", "mine": "",
                     "result": "", "start": "", "has_history": false},
            "name": "",
            "vote": "",
            "next": {"topic": ""},
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("poker"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("New room")
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

        // The start page: a new room, and the ones opened before.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "Open a room and send its link to the team. You start each round; \
             everyone types a number, and nobody sees the numbers until all \
             are in or you reveal them.",
        ));
        out.element(ui::card("new").parent("home").gap("medium"));
        out.element(
            ui::input("title_in")
                .parent("new")
                .label("Room name (optional)")
                .bind("draft.title")
                .placeholder("Sprint 14 planning")
                .on_submit(Handler::action("create")),
        );
        out.element(
            ui::button("create_btn")
                .parent("new")
                .label("Open a room")
                .variant("primary")
                .on_press(Handler::action("create")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your rooms")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // A link to a room that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no room here. Its manager may have closed it."),
        );

        // A room.
        out.element(
            ui::column("room")
                .parent("main")
                .visible(cel("s.view == 'room'")),
        );
        out.element(
            ui::text("room_title")
                .parent("room")
                .variant("h2")
                .text(cel("s.room.title")),
        );
        out.element(ui::card("share").parent("room").gap("small"));
        out.element(
            ui::text("share_caption")
                .parent("share")
                .variant("caption")
                .text("Send this link to the players:"),
        );
        out.element(
            ui::text("share_link")
                .parent("share")
                .variant("code")
                .text(cel("s.room.link")),
        );

        // Joining, or the name already in.
        out.element(
            ui::card("join_card")
                .parent("room")
                .title(cel(
                    "s.room.is_owner ? 'Vote too (optional)' : 'Join the room'",
                ))
                .visible(cel("!s.room.joined"))
                .gap("small"),
        );
        out.element(
            ui::row("join_row")
                .parent("join_card")
                .wrap(true)
                .align("end"),
        );
        out.element(
            ui::input("name_in")
                .parent("join_row")
                .label("Your name")
                .bind("name")
                .on_submit(Handler::action("join")),
        );
        out.element(
            ui::button("join_btn")
                .parent("join_row")
                .label("Join")
                .variant("primary")
                .on_press(Handler::action("join")),
        );
        out.element(
            ui::text("me_caption")
                .parent("room")
                .variant("caption")
                .visible(cel("s.room.joined"))
                .text(cel("s.room.me")),
        );

        // The manager's controls.
        out.element(
            ui::card("manager")
                .parent("room")
                .title("You run this room")
                .visible(cel("s.room.is_owner"))
                .gap("small"),
        );
        out.element(
            ui::row("start_row")
                .parent("manager")
                .wrap(true)
                .align("end")
                .visible(cel("s.room.phase != 'voting'")),
        );
        out.element(
            ui::input("topic_in")
                .parent("start_row")
                .label("What to estimate (optional)")
                .placeholder("Story or ticket")
                .bind("next.topic")
                .on_submit(Handler::action("start")),
        );
        out.element(
            ui::button("start_btn")
                .parent("start_row")
                .label(cel("s.room.start"))
                .variant("primary")
                .on_press(Handler::action("start")),
        );
        out.element(
            ui::row("reveal_row")
                .parent("manager")
                .wrap(true)
                .align("center")
                .visible(cel("s.room.phase == 'voting'")),
        );
        out.element(
            ui::badge("progress")
                .parent("reveal_row")
                .variant("info")
                .text(cel("s.room.progress")),
        );
        out.element(
            ui::button("reveal_btn")
                .parent("reveal_row")
                .label("Reveal now")
                .variant("primary")
                .on_press(Handler::action("reveal")),
        );
        out.element(
            ui::text("manager_caption")
                .parent("manager")
                .variant("caption")
                .visible(cel("s.room.phase == 'voting'"))
                .text("The votes are revealed by themselves once everyone has voted."),
        );

        // The round.
        out.element(
            ui::notice("waiting")
                .parent("room")
                .visible(cel("s.room.phase == 'waiting' && !s.room.is_owner"))
                .text("Waiting for the manager to start the first round."),
        );
        out.element(
            ui::card("round")
                .parent("room")
                .title(cel("s.room.heading"))
                .visible(cel("s.room.phase != 'waiting'"))
                .gap("medium"),
        );
        out.element(
            ui::row("vote_row")
                .parent("round")
                .wrap(true)
                .align("end")
                .visible(cel("s.room.phase == 'voting' && s.room.joined")),
        );
        out.element(
            ui::input("vote_in")
                .parent("vote_row")
                .label("Your estimate")
                .variant("number")
                .bind("vote")
                .on_submit(Handler::action("vote")),
        );
        out.element(
            ui::button("vote_btn")
                .parent("vote_row")
                .label("Vote")
                .variant("primary")
                .on_press(Handler::action("vote")),
        );
        out.element(
            ui::text("mine_caption")
                .parent("round")
                .variant("caption")
                .visible(cel("s.room.phase == 'voting' && s.room.joined"))
                .text(cel("s.room.mine")),
        );
        out.element(
            ui::badge("waiting_badge")
                .parent("round")
                .variant("neutral")
                .visible(cel("s.room.phase == 'voting'"))
                .text(cel("s.room.progress")),
        );
        out.element(
            ui::badge("result")
                .parent("round")
                .variant("success")
                .visible(cel("s.room.phase == 'revealed'"))
                .text(cel("s.room.result")),
        );
        out.element(
            ui::text("next_caption")
                .parent("round")
                .variant("caption")
                .visible(cel("s.room.phase == 'revealed' && !s.room.is_owner"))
                .text("Waiting for the manager to start the next round."),
        );
        out.element(
            ui::card("players_card")
                .parent("room")
                .title("Players")
                .gap("small"),
        );
        out.element(
            ui::list("players")
                .parent("players_card")
                .empty("Nobody has joined yet."),
        );
        out.element(
            ui::card("history_card")
                .parent("room")
                .title("Earlier rounds")
                .visible(cel("s.room.has_history"))
                .gap("small"),
        );
        out.element(ui::column("history").parent("history_card").gap("small"));
        out.element(
            ui::button("delete_btn")
                .parent("room")
                .label("Close the room")
                .variant("failure")
                .visible(cel("s.room.is_owner"))
                .on_press(Handler::action("delete")),
        );

        match self.route.clone() {
            Route::Home => self.go_home(out),
            Route::Open(id) => self.open_room(out, &id),
            Route::Unknown => self.missing(out),
        }
    }

    fn action(&mut self, out: &mut Outbox, action: &Action) {
        match action.name.as_str() {
            "home" => self.go_home(out),
            "create" => self.create(out, action),
            "open" => {
                if let Some(id) = action.payload_as::<String>() {
                    self.open_room(out, &id);
                }
            }
            "join" => self.join(out, action),
            "vote" => self.vote(out, action),
            "start" => self.start(out, action),
            "reveal" => self.reveal(out),
            "remove" => self.remove(out, action),
            "delete" => self.delete(out),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = store::read(&self.db, |tx| tx.rooms().get(id)).map(|r| r.version);
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
