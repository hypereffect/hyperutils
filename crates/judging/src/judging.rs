//! Judged contests.
//!
//! The organizer types in a name, the entries and the judges, one per line,
//! and the range scores go in. Every judge gets a link of their own,
//! `hyperb://<host>/<judge id>`, which opens on their name and their own
//! scores, and nobody else's. For each entry the highest and the lowest score
//! are dropped, once it has three or more, and the rest are averaged; the
//! leaderboard ranks the entries on that.
//!
//! The organizer's link, `hyperb://<host>/<admin id>`, lists the judges'
//! links and how far each one has got, shows the leaderboard as it fills in,
//! adds judges, and reveals the results. Once revealed, the leaderboard shows
//! on `hyperb://<host>/<id>` and on the judges' links, and scores are locked;
//! hiding it again reopens the scoring. Contests are listed on the start page
//! of the key that made them.
//!
//! A session watches the contest it has open on its tick, so scores and
//! reveals made elsewhere show up within a couple of seconds.

use crate::store::{
    self, AdminId, AdminLink, Contest, ContestId, Db, Judge, JudgeId, JudgeLink, Owner, OwnerKey,
    Scale, WriteTx,
};
use hyperb_core::identity::PublicKey;
use hypereffect::vtree::{self, Node};
use hypereffect::{Action, App, Handler, Metadata, Outbox, Value, cel, json, ui};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

/// How often an open contest is checked for news.
const WATCH_MS: u32 = 2000;
const MIN_ENTRIES: usize = 2;
const MAX_ENTRIES: usize = 100;
const MIN_JUDGES: usize = 1;
const MAX_JUDGES: usize = 30;
const MAX_NAME_LEN: usize = 120;
const MAX_ENTRY_LEN: usize = 80;
const MAX_JUDGE_LEN: usize = 40;
const MAX_SCORE: u32 = 1000;
/// How many scores an entry needs before its highest and lowest are dropped.
const TRIM_FROM: usize = 3;
/// How many contests a key's start page lists.
const MAX_OWNED: usize = 50;
/// How long a contest is kept without a change.
const IDLE: u64 = 180 * 24 * 60 * 60;

/// Deletes the contests nobody changed for [`IDLE`], and owners left with
/// none. Returns how many contests went.
pub fn prune(db: &Db) -> usize {
    let cutoff = store::now().saturating_sub(IDLE);
    store::write(db, |tx| {
        let mut n = 0;
        for c in tx.contests().all()? {
            if c.touched <= cutoff {
                tx.contests().delete(&c.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.contests.is_empty() {
                tx.owners().delete(&o.key)?;
            }
        }
        Ok(n)
    })
}

/// An entry's line on the leaderboard.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub entry: u32,
    /// Shared by entries with the same average; `None` while nobody has
    /// scored it.
    pub position: Option<usize>,
    /// Every score it got, lowest first.
    pub scores: Vec<u32>,
}

impl Line {
    /// Whether the lowest and the highest score are dropped.
    pub fn trimmed(&self) -> bool {
        self.scores.len() >= TRIM_FROM
    }

    /// The scores that count.
    pub fn counted(&self) -> &[u32] {
        if self.trimmed() {
            &self.scores[1..self.scores.len() - 1]
        } else {
            &self.scores
        }
    }

    fn sum(&self) -> u64 {
        self.counted().iter().map(|&s| s as u64).sum()
    }

    /// Compares the averages exactly. An entry without scores comes below
    /// any with.
    fn cmp_average(&self, other: &Line) -> Ordering {
        let (a, b) = (self.counted().len() as u64, other.counted().len() as u64);
        match (a, b) {
            (0, 0) => Ordering::Equal,
            (0, _) => Ordering::Less,
            (_, 0) => Ordering::Greater,
            _ => (self.sum() * b).cmp(&(other.sum() * a)),
        }
    }

    /// The average of the scores that count, to two decimals at most.
    pub fn average(&self) -> Option<String> {
        let n = self.counted().len();
        if n == 0 {
            return None;
        }
        let s = format!("{:.2}", self.sum() as f64 / n as f64);
        Some(s.trim_end_matches('0').trim_end_matches('.').to_owned())
    }

    /// The scores, what was dropped, and the average, in a sentence.
    fn detail(&self) -> String {
        let Some(average) = self.average() else {
            return "No scores yet.".to_owned();
        };
        let all = self
            .scores
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" · ");
        if self.trimmed() {
            format!(
                "Scores {all}. Dropping {} and {}, the other {} average {average}.",
                self.scores[0],
                self.scores[self.scores.len() - 1],
                self.counted().len()
            )
        } else {
            format!(
                "Scores {all}, averaged as they are: the highest and lowest are dropped from {TRIM_FROM} scores on."
            )
        }
    }
}

/// The leaderboard: by average once the highest and lowest are dropped, then
/// by name, with the entries nobody scored at the bottom.
pub fn leaderboard(c: &Contest) -> Vec<Line> {
    let mut lines: Vec<Line> = (0..c.entries.len())
        .map(|e| {
            let mut scores: Vec<u32> = c
                .judges
                .iter()
                .filter_map(|j| j.scores.get(e).copied().flatten())
                .collect();
            scores.sort_unstable();
            Line {
                entry: e as u32,
                position: None,
                scores,
            }
        })
        .collect();
    lines.sort_by(|x, y| {
        y.cmp_average(x)
            .then_with(|| c.entries[x.entry as usize].cmp(&c.entries[y.entry as usize]))
    });
    for k in 0..lines.len() {
        if lines[k].scores.is_empty() {
            continue;
        }
        lines[k].position = if k > 0 && lines[k].cmp_average(&lines[k - 1]) == Ordering::Equal {
            lines[k - 1].position
        } else {
            Some(k + 1)
        };
    }
    lines
}

/// The leaderboard as the items of the list `board`, one per entry, from the
/// top. Pressing an entry shows its scores, and pressing it again hides them;
/// that happens in the renderer, under `_open.e<entry>`, which follows the
/// entry when the order changes and is never sent back.
fn board_items(c: &Contest) -> Vec<Node> {
    let item = |l: &Line| {
        let item = format!("b{}", l.entry);
        let open = format!("s._open.e{}", l.entry);
        let name = &c.entries[l.entry as usize];
        let top = vtree::row(format!("{item}_top"))
            .justify("between")
            .children([
                vtree::button(format!("{item}_entry"))
                    .variant("plain")
                    .label(match l.position {
                        Some(p) => format!("{p}. {name}"),
                        None => format!("– {name}"),
                    })
                    .on_press(Handler::patch(
                        json!({"_open": {format!("e{}", l.entry): cel(format!("!{open}"))}}),
                    )),
                vtree::badge(format!("{item}_avg"))
                    .variant(match l.position {
                        Some(1) => "success",
                        Some(_) => "info",
                        None => "neutral",
                    })
                    .text(l.average().unwrap_or_else(|| "No scores".to_owned())),
            ]);
        vtree::column(&item).gap("none").child(top).child(
            vtree::text(format!("{item}_scores"))
                .variant("caption")
                .visible(cel(open))
                .text(l.detail()),
        )
    };
    leaderboard(c).iter().map(item).collect()
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
    Contest,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Contest => "contest",
            View::Missing => "missing",
        }
    }
}

/// What the link a session opened lets it do.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Role {
    /// Sees the judges' links and the leaderboard, adds judges, reveals.
    Organizer,
    /// Scores, as the judge at this index.
    Judge(usize),
    /// Sees the leaderboard once it is out.
    Viewer,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Organizer => "organizer",
            Role::Judge(_) => "judge",
            Role::Viewer => "viewer",
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

/// The names typed in, one per line, or what is wrong with them. `what` is
/// the plural, as in "entries".
fn read_lines(
    lines: &str,
    what: &str,
    min: usize,
    max: usize,
    max_len: usize,
) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for line in lines.lines() {
        let name = limit(line.trim(), max_len);
        if name.is_empty() {
            continue;
        }
        if names
            .iter()
            .any(|n| n.to_lowercase() == name.to_lowercase())
        {
            return Err(format!("“{name}” is in the {what} twice."));
        }
        names.push(name);
    }
    if names.len() < min {
        return Err(if min == 1 {
            format!("Type in the {what}, one per line.")
        } else {
            format!("Type in at least {min} {what}, one per line.")
        });
    }
    if names.len() > max {
        return Err(format!("At most {max} {what}, please."));
    }
    Ok(names)
}

/// The scale the action carries under `at`, or what is wrong with it.
fn read_scale(action: &Action, at: &str) -> Result<Scale, String> {
    let get = |what: &str| text(action, &format!("{at}.{what}")).parse::<u32>().ok();
    match (get("min"), get("max")) {
        (Some(min), Some(max)) if min < max && max <= MAX_SCORE => Ok(Scale { min, max }),
        _ => Err(format!(
            "Scores go from one whole number up to a larger one, at most {MAX_SCORE}."
        )),
    }
}

/// The scale signal: numbers, as number inputs bind them.
fn scale_signal(s: Scale) -> Value {
    json!({"min": s.min, "max": s.max})
}

/// A judge's score signal: a number, or nothing for an entry left blank.
fn score_signal(score: Option<u32>) -> Value {
    match score {
        Some(s) => json!(s),
        None => Value::Null,
    }
}

/// How many of `scores` are given.
fn given(scores: &[Option<u32>]) -> usize {
    scores.iter().filter(|s| s.is_some()).count()
}

/// The contest a link opens, and what the link lets it do.
fn find(db: &Db, id: &str) -> Option<(Contest, Role)> {
    store::read(db, |tx| {
        if let Some(c) = tx.contests().get(ContestId::new(id))? {
            return Ok(Some((c, Role::Viewer)));
        }
        if let Some(link) = tx.admin_links().get(AdminId::new(id))? {
            return Ok(tx
                .contests()
                .get(&link.contest)?
                .map(|c| (c, Role::Organizer)));
        }
        let Some(link) = tx.judge_links().get(JudgeId::new(id))? else {
            return Ok(None);
        };
        Ok(tx
            .contests()
            .get(&link.contest)?
            .map(|c| (c, Role::Judge(link.judge as usize))))
    })
}

/// A fresh id, used by no contest and no link.
fn fresh_id(tx: &WriteTx) -> orangedb::Result<String> {
    loop {
        let id = cuid2::create_id();
        if !tx.contests().contains(ContestId::new(&id))?
            && !tx.admin_links().contains(AdminId::new(&id))?
            && !tx.judge_links().contains(JudgeId::new(&id))?
        {
            return Ok(id);
        }
    }
}

pub struct Contests {
    db: Arc<Db>,
    /// The key of this session.
    viewer: PublicKey,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    /// The contest open in this session, what its link lets this session
    /// do, and the version on screen.
    open: Option<ContestId>,
    role: Role,
    seen: u64,
    /// A judge's scores on screen, by entry, so that a refresh only resets
    /// the inputs whose score changed.
    shown: Option<Vec<Option<u32>>>,
    /// Whether the signals that show and hide the scores are set up.
    toggles: bool,
    /// The trees sent last, so that a refresh re-sends only what changed.
    mine: Vec<Node>,
    board: Vec<Node>,
    sheet: Vec<Node>,
    judges: Vec<Node>,
    /// The `c` signal last shown, so that an unchanged contest stays silent.
    shown_c: Option<Value>,
}

impl Contests {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Contests {
            db,
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            open: None,
            role: Role::Viewer,
            seen: 0,
            shown: None,
            toggles: false,
            mine: Vec::new(),
            board: Vec::new(),
            sheet: Vec::new(),
            judges: Vec::new(),
            shown_c: None,
        }
    }

    fn me(&self) -> String {
        self.viewer.to_string()
    }

    fn link(&self, id: &str) -> String {
        format!("{}/{id}", self.origin)
    }

    /// Turns the children of `mount` from `shown` into `new`, sending only
    /// the difference.
    fn replace(out: &mut Outbox, mount: &str, shown: &mut Vec<Node>, new: Vec<Node>) {
        out.extend(vtree::diff_into(Some(mount), shown, &new));
        *shown = new;
    }

    /// Switches to `view`, dropping what belonged to the one before.
    fn show(&mut self, out: &mut Outbox, view: View) {
        Self::replace(out, "board", &mut self.board, Vec::new());
        Self::replace(out, "sheet_list", &mut self.sheet, Vec::new());
        Self::replace(out, "judge_list", &mut self.judges, Vec::new());
        self.open = None;
        self.role = Role::Viewer;
        self.shown = None;
        self.shown_c = None;
        self.toggles = false;
        self.view = view;
        out.signal(json!({"view": view.name(), "feedback": {"kind": "info", "text": ""}}));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let mine: Vec<Contest> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut mine = Vec::new();
            for id in &o.contests {
                if let Some(c) = tx.contests().get(id)? {
                    mine.push(c);
                }
            }
            Ok(mine)
        });
        let buttons = mine
            .iter()
            .map(|c| {
                vtree::button(format!("mine_{}", c.id.0))
                    .label(format!(
                        "{} · {} entries · {} judges · {}",
                        c.name,
                        c.entries.len(),
                        c.judges.len(),
                        if c.revealed { "results out" } else { "scoring" }
                    ))
                    .on_press(Handler::action("open").payload(c.admin.as_str()))
            })
            .collect();
        out.signal(json!({"has_mine": !mine.is_empty()}));
        Self::replace(out, "mine_list", &mut self.mine, buttons);
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
            return fail(out, "Give the contest a name.");
        }
        let entries = match read_lines(
            &text(action, "draft.entries"),
            "entries",
            MIN_ENTRIES,
            MAX_ENTRIES,
            MAX_ENTRY_LEN,
        ) {
            Ok(e) => e,
            Err(why) => return fail(out, &why),
        };
        let judges = match read_lines(
            &text(action, "draft.judges"),
            "judges",
            MIN_JUDGES,
            MAX_JUDGES,
            MAX_JUDGE_LEN,
        ) {
            Ok(j) => j,
            Err(why) => return fail(out, &why),
        };
        let scale = match read_scale(action, "draft.scale") {
            Ok(s) => s,
            Err(why) => return fail(out, &why),
        };
        let me = self.me();
        let admin = store::write(&self.db, |tx| {
            let id = ContestId::new(fresh_id(tx)?);
            let admin = AdminId::new(fresh_id(tx)?);
            let mut links = Vec::new();
            for _ in &judges {
                links.push(fresh_id(tx)?);
            }
            tx.contests().insert(&Contest {
                id: id.clone(),
                admin: admin.0.clone(),
                owner: me.clone(),
                name,
                judges: judges
                    .into_iter()
                    .zip(&links)
                    .map(|(name, link)| Judge {
                        name,
                        link: link.clone(),
                        scores: vec![None; entries.len()],
                    })
                    .collect(),
                entries,
                scale,
                revealed: false,
                version: 1,
                touched: store::now(),
            })?;
            tx.admin_links().insert(&AdminLink {
                id: admin.clone(),
                contest: id.clone(),
            })?;
            for (k, link) in links.into_iter().enumerate() {
                tx.judge_links().insert(&JudgeLink {
                    id: JudgeId::new(link),
                    contest: id.clone(),
                    judge: k as u32,
                })?;
            }
            let key = OwnerKey::new(me);
            let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                key,
                contests: Vec::new(),
            });
            o.contests.insert(0, id);
            o.contests.truncate(MAX_OWNED);
            tx.owners().upsert(&o)?;
            Ok(admin)
        });
        out.signal(json!({"draft": {"name": "", "entries": "", "judges": ""}}));
        self.open_contest(out, &admin.0);
        out.signal(feedback(
            "success",
            "The contest is ready. Send each judge their own link below; \
             the leaderboard stays with you until you reveal it.",
        ));
    }

    // --- the contest ---------------------------------------------------------

    /// Opens the contest behind the link `id`.
    fn open_contest(&mut self, out: &mut Outbox, id: &str) {
        let Some((c, role)) = find(&self.db, id) else {
            return self.missing(out);
        };
        if self.open.as_ref() != Some(&c.id) || self.role != role {
            self.show(out, View::Contest);
        }
        self.open = Some(c.id.clone());
        self.role = role;
        self.render(out, &c);
    }

    /// Shows the open contest as it is now again.
    fn refresh(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        match store::read(&self.db, |tx| tx.contests().get(&id)) {
            Some(c) => self.render(out, &c),
            None => {
                self.missing(out);
                out.signal(feedback("warning", "This contest was deleted."));
            }
        }
    }

    fn render(&mut self, out: &mut Outbox, c: &Contest) {
        self.seen = c.version;
        let total = c.entries.len() * c.judges.len();
        let all_given: usize = c.judges.iter().map(|j| given(&j.scores)).sum();
        let judge = match self.role {
            Role::Judge(j) => c.judges.get(j),
            _ => None,
        };
        let shown_c = json!({"c": {
            "name": c.name,
            "role": self.role.name(),
            "judge": judge.map(|j| j.name.as_str()).unwrap_or(""),
            "revealed": c.revealed,
            "is_owner": self.role == Role::Organizer && c.owner == self.me(),
            "link": self.link(&c.id.0),
            "scale": format!("Scores from {} to {}", c.scale.min, c.scale.max),
            "progress": format!("{all_given} of {total} scores in"),
            "mine": judge.map(|j| format!(
                "You have scored {} of {} entries.",
                given(&j.scores),
                c.entries.len()
            )).unwrap_or_default(),
        }});
        if Some(&shown_c) != self.shown_c.as_ref() {
            out.signal(shown_c.clone());
            self.shown_c = Some(shown_c);
        }

        if let Some(j) = judge {
            // Only what changed is reset, so that nothing being typed is lost.
            let shown = self
                .shown
                .get_or_insert_with(|| vec![None; c.entries.len()]);
            let fresh = self.sheet.is_empty();
            let mut scores = hypereffect::Map::new();
            for (k, &s) in j.scores.iter().enumerate() {
                if fresh || shown.get(k) != Some(&s) {
                    scores.insert(format!("e{k}"), score_signal(s));
                }
            }
            *shown = j.scores.clone();
            if !scores.is_empty() {
                out.signal(json!({ "score": scores }));
            }
            if fresh {
                let sheet = c
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(k, entry)| {
                        vtree::input(format!("e{k}_in"))
                            .label(entry.as_str())
                            .variant("number")
                            .bind(format!("score.e{k}"))
                    })
                    .collect();
                Self::replace(out, "sheet_list", &mut self.sheet, sheet);
            }
        }

        if self.role == Role::Organizer {
            let mut items = Vec::new();
            for (k, j) in c.judges.iter().enumerate() {
                let item = format!("j{k}");
                let done = given(&j.scores);
                let top = vtree::row(format!("{item}_top"))
                    .justify("between")
                    .children([
                        vtree::text(format!("{item}_name")).text(j.name.as_str()),
                        vtree::badge(format!("{item}_done"))
                            .variant(if done == c.entries.len() {
                                "success"
                            } else {
                                "neutral"
                            })
                            .text(format!("{done} of {} scored", c.entries.len())),
                    ]);
                items.push(
                    vtree::column(&item).gap("none").child(top).child(
                        vtree::text(format!("{item}_link"))
                            .variant("code")
                            .text(self.link(&j.link)),
                    ),
                );
            }
            Self::replace(out, "judge_list", &mut self.judges, items);
        }

        if c.revealed || self.role == Role::Organizer {
            if !self.toggles {
                // Every entry's scores start hidden; null first, so that the
                // entries of a contest open before do not linger.
                let closed: hypereffect::Map<String, Value> = (0..c.entries.len())
                    .map(|k| (format!("e{k}"), false.into()))
                    .collect();
                out.signal(json!({"_open": null}));
                out.signal(json!({ "_open": closed }));
                self.toggles = true;
            }
            Self::replace(out, "board", &mut self.board, board_items(c));
        } else {
            Self::replace(out, "board", &mut self.board, Vec::new());
        }
    }

    /// Changes the open contest, if `change` says so, and shows how it is
    /// now. The caller checks that this session may.
    fn change(
        &mut self,
        out: &mut Outbox,
        change: impl FnOnce(&mut Contest) -> Result<String, String>,
    ) {
        let Some(id) = self.open.clone() else { return };
        let result = store::write(&self.db, |tx| {
            let Some(mut c) = tx.contests().get(&id)? else {
                return Ok(Err("This contest no longer exists.".to_owned()));
            };
            let done = match change(&mut c) {
                Ok(done) => done,
                Err(why) => return Ok(Err(why)),
            };
            c.version += 1;
            c.touched = store::now();
            tx.contests().update(&c)?;
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

    /// Saves the open judge's scores, all of them at once. A blank one is
    /// not scored.
    fn save(&mut self, out: &mut Outbox, action: &Action) {
        let Role::Judge(j) = self.role else { return };
        self.change(out, |c| {
            if c.revealed {
                return Err("The results are out, so the scores are locked.".to_owned());
            }
            let mut scores = Vec::with_capacity(c.entries.len());
            for (k, entry) in c.entries.iter().enumerate() {
                let s = text(action, &format!("score.e{k}"));
                if s.is_empty() {
                    scores.push(None);
                    continue;
                }
                match s.parse::<u32>() {
                    Ok(s) if (c.scale.min..=c.scale.max).contains(&s) => scores.push(Some(s)),
                    _ => {
                        return Err(format!(
                            "The score for “{entry}” should be a whole number from {} to {}.",
                            c.scale.min, c.scale.max
                        ));
                    }
                }
            }
            let judge = c.judges.get_mut(j).ok_or("There is no such judge.")?;
            judge.scores = scores;
            Ok(format!(
                "Saved. You have scored {} of {} entries.",
                given(&judge.scores),
                c.entries.len()
            ))
        });
    }

    fn reveal(&mut self, out: &mut Outbox) {
        if self.role != Role::Organizer {
            return;
        }
        self.change(out, |c| {
            c.revealed = !c.revealed;
            Ok(if c.revealed {
                "The leaderboard is out, and the scores are locked."
            } else {
                "The leaderboard is hidden again, and the judges can change their scores."
            }
            .to_owned())
        });
    }

    fn add_judge(&mut self, out: &mut Outbox, action: &Action) {
        let Some(id) = self.open.clone().filter(|_| self.role == Role::Organizer) else {
            return;
        };
        let name = limit(&text(action, "new_judge"), MAX_JUDGE_LEN);
        if name.is_empty() {
            return out.signal(feedback("failure", "Type the judge's name."));
        }
        let result = store::write(&self.db, |tx| {
            let Some(mut c) = tx.contests().get(&id)? else {
                return Ok(Err("This contest no longer exists.".to_owned()));
            };
            if c.judges
                .iter()
                .any(|j| j.name.to_lowercase() == name.to_lowercase())
            {
                return Ok(Err(format!("“{name}” is judging already.")));
            }
            if c.judges.len() >= MAX_JUDGES {
                return Ok(Err(format!("At most {MAX_JUDGES} judges, please.")));
            }
            let link = fresh_id(tx)?;
            c.judges.push(Judge {
                name: name.clone(),
                link: link.clone(),
                scores: vec![None; c.entries.len()],
            });
            c.version += 1;
            c.touched = store::now();
            tx.contests().update(&c)?;
            tx.judge_links().insert(&JudgeLink {
                id: JudgeId::new(link),
                contest: id.clone(),
                judge: (c.judges.len() - 1) as u32,
            })?;
            Ok(Ok(()))
        });
        match result {
            Ok(()) => {
                self.refresh(out);
                out.signal(json!({"new_judge": ""}));
                out.signal(feedback(
                    "success",
                    format!("{name} is judging too. Send them their link below."),
                ));
            }
            Err(why) => out.signal(feedback("failure", why)),
        }
    }

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone().filter(|_| self.role == Role::Organizer) else {
            return;
        };
        let me = self.me();
        // Its links go with it, and it leaves its owner's list.
        let deleted = store::write(&self.db, |tx| {
            if tx.contests().get(&id)?.is_none_or(|c| c.owner != me) {
                return Ok(false);
            }
            tx.contests().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The contest is deleted."));
        } else {
            out.signal(feedback(
                "failure",
                "Only its creator can delete this contest.",
            ));
        }
    }
}

impl App for Contests {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(Metadata::new().name("judging").description(
            "Judged contests: private scorecards, high and low dropped, a leaderboard",
        ));
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"name": "", "entries": "", "judges": "",
                      "scale": scale_signal(Scale::default())},
            "c": {"name": "", "role": "viewer", "judge": "", "revealed": false,
                  "is_owner": false, "link": "", "scale": "", "progress": "", "mine": ""},
            "score": {},
            "new_judge": "",
            "_open": {},
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("judging"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("New contest")
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

        // The start page: a new contest, and the ones made before.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "Photo contests, hackathons, bake-offs. Type in the entries and the \
             judges, and every judge gets a link of their own to score privately. \
             The highest and lowest score of each entry are dropped, the rest \
             averaged, and the leaderboard appears when you reveal it.",
        ));
        out.element(ui::card("new").parent("home").gap("medium"));
        out.element(
            ui::input("name_in")
                .parent("new")
                .label("Name")
                .bind("draft.name")
                .placeholder("Village bake-off"),
        );
        out.element(
            ui::input("entries_in")
                .parent("new")
                .label("Entries, one per line")
                .variant("multiline")
                .bind("draft.entries"),
        );
        out.element(
            ui::input("judges_in")
                .parent("new")
                .label("Judges, one per line")
                .variant("multiline")
                .bind("draft.judges"),
        );
        out.element(ui::row("scale_row").parent("new").wrap(true));
        out.element(
            ui::input("scale_min")
                .parent("scale_row")
                .label("Lowest score")
                .variant("number")
                .bind("draft.scale.min"),
        );
        out.element(
            ui::input("scale_max")
                .parent("scale_row")
                .label("Highest score")
                .variant("number")
                .bind("draft.scale.max"),
        );
        out.element(
            ui::button("create_btn")
                .parent("new")
                .label("Make the judges' links")
                .variant("primary")
                .on_press(Handler::action("create")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your contests")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // A link to a contest that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no contest here. It may have been deleted."),
        );

        // A contest.
        out.element(
            ui::column("contest")
                .parent("main")
                .visible(cel("s.view == 'contest'")),
        );
        out.element(
            ui::text("c_name")
                .parent("contest")
                .variant("h2")
                .text(cel("s.c.name")),
        );
        out.element(ui::row("c_badges").parent("contest").wrap(true));
        out.element(
            ui::badge("c_status")
                .parent("c_badges")
                .variant(cel("s.c.revealed ? 'success' : 'warning'"))
                .text(cel("s.c.revealed ? 'Results are out' : 'Scoring is open'")),
        );
        out.element(
            ui::badge("c_scale")
                .parent("c_badges")
                .variant("info")
                .text(cel("s.c.scale")),
        );
        out.element(
            ui::badge("c_progress")
                .parent("c_badges")
                .variant("neutral")
                .visible(cel("s.c.role == 'organizer'"))
                .text(cel("s.c.progress")),
        );

        // What a judge sees: their own scorecard.
        out.element(
            ui::card("sheet")
                .parent("contest")
                .title(cel("'Scorecard of ' + s.c.judge"))
                .visible(cel("s.c.role == 'judge'"))
                .gap("small"),
        );
        out.element(
            ui::text("sheet_intro")
                .parent("sheet")
                .variant("caption")
                .text(
                    "Your scores are private: the other judges never see them, and the \
             leaderboard only shows them mixed in with everyone else's. Leave an \
             entry blank to skip it, and come back to change any score until \
             the results are out.",
                ),
        );
        out.element(ui::column("sheet_list").parent("sheet").gap("small"));
        out.element(
            ui::text("sheet_mine")
                .parent("sheet")
                .variant("caption")
                .text(cel("s.c.mine")),
        );
        out.element(
            ui::button("save_btn")
                .parent("sheet")
                .label("Save my scores")
                .variant("primary")
                .visible(cel("!s.c.revealed"))
                .on_press(Handler::action("save")),
        );
        out.element(
            ui::notice("locked")
                .parent("sheet")
                .variant("info")
                .visible(cel("s.c.revealed"))
                .text("The results are out, so the scores are locked."),
        );

        // What anyone else sees while the judges are at work.
        out.element(
            ui::notice("waiting")
                .parent("contest")
                .variant("info")
                .visible(cel("s.c.role == 'viewer' && !s.c.revealed"))
                .text("The judges are still at work. The leaderboard shows up here once the results are out."),
        );

        // What the organizer sees: the judges and their links.
        out.element(
            ui::card("judges")
                .parent("contest")
                .title("Judges")
                .visible(cel("s.c.role == 'organizer'"))
                .gap("medium"),
        );
        out.element(
            ui::text("judges_caption")
                .parent("judges")
                .variant("caption")
                .text(
                    "Send each judge their own link: it opens on their name and their scorecard. \
             Anyone holding it can score as that judge.",
                ),
        );
        out.element(ui::list("judge_list").parent("judges"));
        out.element(ui::row("add_row").parent("judges").wrap(true));
        out.element(
            ui::input("new_judge_in")
                .parent("add_row")
                .placeholder("Another judge")
                .bind("new_judge"),
        );
        out.element(
            ui::button("add_btn")
                .parent("add_row")
                .label("Add judge")
                .on_press(Handler::action("add_judge")),
        );
        out.element(
            ui::text("view_caption")
                .parent("judges")
                .variant("caption")
                .text("For everyone else, to see the leaderboard once it is out:"),
        );
        out.element(
            ui::text("view_link")
                .parent("judges")
                .variant("code")
                .text(cel("s.c.link")),
        );

        // The leaderboard, for the organizer all along and for everyone once
        // it is out.
        out.element(
            ui::card("results")
                .parent("contest")
                .title(cel(
                    "s.c.revealed ? 'Leaderboard' : 'Leaderboard (only you see it until you reveal it)'",
                ))
                .visible(cel("s.c.revealed || s.c.role == 'organizer'"))
                .gap("small"),
        );
        out.element(ui::list("board").parent("results"));
        out.element(
            ui::text("board_caption")
                .parent("results")
                .variant("caption")
                .text(
                    "Ranked by the average score, dropping each entry's highest and lowest \
             once it has three or more. Press an entry to see its scores.",
                ),
        );
        out.element(
            ui::button("reveal_btn")
                .parent("contest")
                .label(cel(
                    "s.c.revealed ? 'Hide the results and reopen scoring' : 'Reveal the results'",
                ))
                .variant(cel("s.c.revealed ? 'plain' : 'primary'"))
                .visible(cel("s.c.role == 'organizer'"))
                .on_press(Handler::action("reveal")),
        );
        out.element(
            ui::button("delete_btn")
                .parent("contest")
                .label("Delete contest")
                .variant("failure")
                .visible(cel("s.c.is_owner"))
                .on_press(Handler::action("delete")),
        );

        match self.route.clone() {
            Route::Home => self.go_home(out),
            Route::Open(id) => self.open_contest(out, &id),
            Route::Unknown => self.missing(out),
        }
    }

    fn action(&mut self, out: &mut Outbox, action: &Action) {
        match action.name.as_str() {
            "home" => self.go_home(out),
            "create" => self.create(out, action),
            "open" => {
                if let Some(id) = action.payload_as::<String>() {
                    self.open_contest(out, &id);
                }
            }
            "save" => self.save(out, action),
            "reveal" => self.reveal(out),
            "add_judge" => self.add_judge(out, action),
            "delete" => self.delete(out),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = store::read(&self.db, |tx| tx.contests().get(id)).map(|c| c.version);
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
