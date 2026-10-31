//! Sign-up sheets.
//!
//! Anyone can make one: a title, a few words on what it is for, and the
//! items, one per line: the roles to fill or the things to bring. An item
//! takes one name unless its line ends in `x3` or the like. The sheet's link,
//! `hyperb://<host>/<id>`, is all there is: whoever opens it can put a name
//! down for an item, and take off the names they put down. The creator can
//! also take off anyone's, and add, change and remove items.
//!
//! Every name shows who put it down, and the sheet keeps a history of who
//! did what. Who is the [`tag`] of their key: a salted hash, short enough to
//! read and to tell people apart by, that does not give the key away.
//!
//! A session watches the sheet it has open on its tick, so names put down
//! elsewhere show up within a couple of seconds.

use crate::store::{self, Change, Db, Item, Name, Owner, OwnerKey, Sheet, SheetId, WriteTx};
use hyperb_core::identity::PublicKey;
use hypereffect::{Action, App, Element, Handler, Metadata, Outbox, Value, cel, json, ui};
use std::collections::BTreeMap;
use std::sync::Arc;

/// How often an open sheet is checked for news.
const WATCH_MS: u32 = 2000;
/// Hashed in front of a key to make its tag, so that tags here cannot be
/// matched against hashes of the same key elsewhere.
const SALT: &[u8] = b"hyperutils signup: who wrote this";
/// How many hex digits of the hash make a tag.
const TAG_LEN: usize = 8;
const MAX_ITEMS: usize = 100;
/// The most names an item takes, also when it has no limit of its own.
const MAX_NAMES: u32 = 100;
const MAX_TITLE_LEN: usize = 120;
const MAX_DESCRIPTION_LEN: usize = 2000;
const MAX_LABEL_LEN: usize = 80;
const MAX_NAME_LEN: usize = 40;
/// How many changes a sheet's history keeps.
const MAX_LOG: usize = 200;
/// How many sheets a key's start page lists.
const MAX_OWNED: usize = 50;
/// How long a sheet is kept without a change.
const IDLE: u64 = 180 * 24 * 60 * 60;

/// What is shown for `key` wherever it changed something: the first
/// [`TAG_LEN`] hex digits of the BLAKE3 hash of [`SALT`] and the key.
pub fn tag(key: &PublicKey) -> String {
    let mut h = blake3::Hasher::new();
    h.update(SALT);
    h.update(&key.to_bytes());
    h.finalize().to_hex()[..TAG_LEN].to_owned()
}

/// Deletes the sheets nobody changed for [`IDLE`], and owners left with none.
/// Returns how many sheets went.
pub fn prune(db: &Db) -> usize {
    let cutoff = store::now().saturating_sub(IDLE);
    store::write(db, |tx| {
        let mut n = 0;
        for s in tx.sheets().all()? {
            if s.touched <= cutoff {
                tx.sheets().delete(&s.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.sheets.is_empty() {
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
    Sheet,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Sheet => "sheet",
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

/// How many names an item takes, as typed: blank for no limit.
fn read_capacity(s: &str) -> Result<Option<u32>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    match s.parse::<u32>() {
        Ok(n) if (1..=MAX_NAMES).contains(&n) => Ok(Some(n)),
        _ => Err(format!(
            "How many is a whole number from 1 to {MAX_NAMES}, or blank for no limit."
        )),
    }
}

fn capacity_signal(c: Option<u32>) -> String {
    c.map(|n| n.to_string()).unwrap_or_default()
}

/// A line of the item list: a label, and how many it takes if the line ends
/// in `x3`, `×3` or the like, or one.
fn read_item(line: &str) -> (String, Option<u32>) {
    let line = line.trim();
    if let Some((label, last)) = line.rsplit_once(char::is_whitespace) {
        let n = last
            .strip_prefix(['x', 'X', '×'])
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| (1..=MAX_NAMES).contains(n));
        if let Some(n) = n.filter(|_| !label.trim().is_empty()) {
            return (limit(label.trim(), MAX_LABEL_LEN), Some(n));
        }
    }
    (limit(line, MAX_LABEL_LEN), Some(1))
}

fn same(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The items typed in, one per line, or what is wrong with them.
fn read_items(lines: &str) -> Result<Vec<(String, Option<u32>)>, String> {
    let mut items: Vec<(String, Option<u32>)> = Vec::new();
    for line in lines.lines().filter(|l| !l.trim().is_empty()) {
        let (label, capacity) = read_item(line);
        if items.iter().any(|(l, _)| same(l, &label)) {
            return Err(format!("“{label}” is in the list twice."));
        }
        items.push((label, capacity));
    }
    if items.is_empty() {
        return Err("List at least one item, one per line.".to_owned());
    }
    if items.len() > MAX_ITEMS {
        return Err(format!("At most {MAX_ITEMS} items, please."));
    }
    Ok(items)
}

/// “just now”, “5 min ago” and so on, for something that happened at `at`.
fn ago(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => match s / 86400 {
            1 => "yesterday".to_owned(),
            d => format!("{d} days ago"),
        },
    }
}

/// How full the sheet is, for its badge, and whether it is.
fn fill(s: &Sheet) -> (String, bool) {
    let names: usize = s.items.iter().map(|i| i.names.len()).sum();
    let spots: Option<u32> = s.items.iter().map(|i| i.capacity).sum();
    match spots {
        Some(spots) => (
            format!("{names} of {spots} spots taken"),
            names as u32 >= spots,
        ),
        None => (
            format!("{names} name{} down", if names == 1 { "" } else { "s" }),
            false,
        ),
    }
}

/// Notes `what` in the sheet's history, as done by `by`.
fn log(s: &mut Sheet, by: &str, what: String) {
    s.log.insert(
        0,
        Change {
            by: by.to_owned(),
            at: store::now(),
            what,
        },
    );
    s.log.truncate(MAX_LOG);
}

fn item_mut(s: &mut Sheet, id: u32) -> Result<&mut Item, String> {
    s.items
        .iter_mut()
        .find(|i| i.id == id)
        .ok_or_else(|| "That item is no longer on the sheet.".to_owned())
}

fn must_own(s: &Sheet, me: &str) -> Result<(), String> {
    if s.owner == me {
        Ok(())
    } else {
        Err("Only whoever made this sheet can change its items.".to_owned())
    }
}

pub struct Sheets {
    db: Arc<Db>,
    /// The key of this session, and its tag.
    viewer: PublicKey,
    tag: String,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    /// The sheet open in this session, and the version on screen.
    open: Option<SheetId>,
    seen: u64,
    /// The items as their edit inputs were last filled, by id, so that a
    /// refresh only resets the inputs of the items that changed.
    shown_items: BTreeMap<u32, (String, Option<u32>)>,
    /// The title and description in their edit inputs, likewise.
    shown_details: Option<(String, String)>,
    /// Ids of the elements that are replaced as a whole.
    mine_ids: Vec<String>,
    item_ids: Vec<String>,
    log_ids: Vec<String>,
}

impl Sheets {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Sheets {
            db,
            tag: tag(&viewer),
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            open: None,
            seen: 0,
            shown_items: BTreeMap::new(),
            shown_details: None,
            mine_ids: Vec::new(),
            item_ids: Vec::new(),
            log_ids: Vec::new(),
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
        Self::replace(out, &mut self.item_ids, Vec::new());
        Self::replace(out, &mut self.log_ids, Vec::new());
        self.open = None;
        self.shown_items.clear();
        self.shown_details = None;
        self.view = view;
        out.signal(json!({
            "view": view.name(),
            "feedback": {"kind": "info", "text": ""},
            "_editing": false,
        }));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let mine: Vec<Sheet> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut mine = Vec::new();
            for id in &o.sheets {
                if let Some(s) = tx.sheets().get(id)? {
                    mine.push(s);
                }
            }
            Ok(mine)
        });
        let buttons = mine
            .iter()
            .enumerate()
            .map(|(k, s)| {
                ui::button(format!("mine_{k}"))
                    .parent("mine_list")
                    .label(format!("{} · {}", s.title, fill(s).0))
                    .on_press(Handler::action("open").payload(s.id.0.as_str()))
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
        let title = limit(&text(action, "draft.title"), MAX_TITLE_LEN);
        if title.is_empty() {
            return fail(out, "Give the sheet a title.");
        }
        let description = limit(&text(action, "draft.description"), MAX_DESCRIPTION_LEN);
        let items = match read_items(&text(action, "draft.items")) {
            Ok(i) => i,
            Err(why) => return fail(out, &why),
        };
        let me = self.me();
        let by = self.tag.clone();
        let id = store::write(&self.db, |tx| {
            let id = new_id(tx)?;
            let mut s = Sheet {
                id: id.clone(),
                owner: me.clone(),
                title,
                description,
                items: Vec::new(),
                log: Vec::new(),
                next_id: 0,
                version: 1,
                touched: store::now(),
            };
            for (label, capacity) in items {
                s.items.push(Item {
                    id: s.next_id,
                    label,
                    capacity,
                    names: Vec::new(),
                });
                s.next_id += 1;
            }
            log(&mut s, &by, "made the sheet".to_owned());
            tx.sheets().insert(&s)?;
            let key = OwnerKey::new(me);
            let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                key,
                sheets: Vec::new(),
            });
            o.sheets.insert(0, id.clone());
            o.sheets.truncate(MAX_OWNED);
            tx.owners().upsert(&o)?;
            Ok(id)
        });
        out.signal(json!({"draft": {"title": "", "description": "", "items": ""}}));
        self.open_sheet(out, &id.0);
        out.signal(feedback(
            "success",
            "The sheet is ready. Share its link, and people can put their names down.",
        ));
    }

    // --- the sheet ---------------------------------------------------------

    /// Opens the sheet behind the link `id`.
    fn open_sheet(&mut self, out: &mut Outbox, id: &str) {
        let Some(s) = store::read(&self.db, |tx| tx.sheets().get(SheetId::new(id))) else {
            return self.missing(out);
        };
        if self.open.as_ref() != Some(&s.id) {
            self.show(out, View::Sheet);
        }
        self.open = Some(s.id.clone());
        self.render(out, &s);
    }

    /// Shows the open sheet as it is now again.
    fn refresh(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        match store::read(&self.db, |tx| tx.sheets().get(&id)) {
            Some(s) => self.render(out, &s),
            None => {
                self.missing(out);
                out.signal(feedback("warning", "This sheet was deleted."));
            }
        }
    }

    fn render(&mut self, out: &mut Outbox, s: &Sheet) {
        self.seen = s.version;
        let me = self.me();
        let is_owner = s.owner == me;
        let (filled, full) = fill(s);
        out.signal(json!({"sheet": {
            "title": s.title,
            "description": s.description,
            "is_owner": is_owner,
            "link": format!("{}/{}", self.origin, s.id.0),
            "fill": filled,
            "full": full,
            "me": format!("Your tag is {}. It is shown with every name you put down.", self.tag),
        }}));

        if is_owner {
            // Only what changed is reset, so that nothing being typed is lost.
            let details = (s.title.clone(), s.description.clone());
            if self.shown_details.as_ref() != Some(&details) {
                out.signal(json!({"details": {"title": details.0, "description": details.1}}));
                self.shown_details = Some(details);
            }
            let mut edits = hypereffect::Map::new();
            for i in &s.items {
                let now = (i.label.clone(), i.capacity);
                if self.shown_items.get(&i.id) != Some(&now) {
                    edits.insert(
                        format!("i{}", i.id),
                        json!({"label": i.label, "cap": capacity_signal(i.capacity)}),
                    );
                    self.shown_items.insert(i.id, now);
                }
            }
            if !edits.is_empty() {
                out.signal(json!({ "edit": edits }));
            }
        }

        let mut elements = Vec::new();
        for i in &s.items {
            let card = format!("item{}", i.id);
            elements.push(
                ui::card(&card)
                    .parent("items")
                    .title(i.label.as_str())
                    .gap("small"),
            );
            let head = format!("{card}_head");
            elements.push(ui::row(&head).parent(&card).justify("between").wrap(true));
            let n = i.names.len();
            let (count, variant) = match i.capacity {
                Some(c) => (
                    format!("{n} of {c}"),
                    if n as u32 >= c { "success" } else { "neutral" },
                ),
                None => (format!("{n}, no limit"), "info"),
            };
            elements.push(
                ui::badge(format!("{card}_count"))
                    .parent(&head)
                    .variant(variant)
                    .text(count),
            );
            if i.is_full(MAX_NAMES) {
                elements.push(
                    ui::text(format!("{card}_full"))
                        .parent(&head)
                        .variant("caption")
                        .text("Taken"),
                );
            } else {
                elements.push(
                    ui::button(format!("{card}_join"))
                        .parent(&head)
                        .label("Put my name down")
                        .variant("primary")
                        .disabled(cel("s.who == ''"))
                        .on_press(Handler::action("join").payload(i.id)),
                );
            }
            let list = format!("{card}_names");
            elements.push(ui::list(&list).parent(&card).empty("Nobody yet."));
            for name in &i.names {
                let row = format!("name{}", name.id);
                elements.push(ui::row(&row).parent(&list).wrap(true));
                elements.push(
                    ui::text(format!("{row}_name"))
                        .parent(&row)
                        .text(name.name.as_str()),
                );
                elements.push(
                    ui::text(format!("{row}_by"))
                        .parent(&row)
                        .variant("caption")
                        .text(format!("by {}", name.by)),
                );
                if name.key == me || is_owner {
                    elements.push(
                        ui::button(format!("{row}_leave"))
                            .parent(&row)
                            .label("Take off")
                            .variant("plain")
                            .on_press(Handler::action("leave").payload(name.id)),
                    );
                }
            }
            if is_owner {
                let edit = format!("{card}_edit");
                let at = format!("edit.i{}", i.id);
                elements.push(
                    ui::row(&edit)
                        .parent(&card)
                        .wrap(true)
                        .align("end")
                        .visible(cel("s._editing")),
                );
                elements.push(
                    ui::input(format!("{edit}_label"))
                        .parent(&edit)
                        .label("Item")
                        .bind(format!("{at}.label")),
                );
                elements.push(
                    ui::input(format!("{edit}_cap"))
                        .parent(&edit)
                        .label("How many")
                        .variant("number")
                        .placeholder("No limit")
                        .bind(format!("{at}.cap")),
                );
                elements.push(
                    ui::button(format!("{edit}_save"))
                        .parent(&edit)
                        .label("Save")
                        .on_press(Handler::action("save_item").payload(i.id)),
                );
                elements.push(
                    ui::button(format!("{edit}_remove"))
                        .parent(&edit)
                        .label("Remove item")
                        .variant("failure")
                        .on_press(Handler::action("remove_item").payload(i.id)),
                );
            }
        }
        Self::replace(out, &mut self.item_ids, elements);

        let now = store::now();
        let lines = s
            .log
            .iter()
            .enumerate()
            .map(|(k, c)| {
                ui::text(format!("log{k}")).parent("log").text(format!(
                    "{} {} · {}",
                    c.by,
                    c.what,
                    ago(c.at, now)
                ))
            })
            .collect();
        Self::replace(out, &mut self.log_ids, lines);
    }

    /// Changes the open sheet, if `change` says so, and shows how it is now.
    /// `change` gets the sheet, the viewer's key and their tag. Returns
    /// whether it changed.
    fn change(
        &mut self,
        out: &mut Outbox,
        change: impl FnOnce(&mut Sheet, &str, &str) -> Result<String, String>,
    ) -> bool {
        let Some(id) = self.open.clone() else {
            return false;
        };
        let (me, by) = (self.me(), self.tag.clone());
        let result = store::write(&self.db, |tx| {
            let Some(mut s) = tx.sheets().get(&id)? else {
                return Ok(Err("This sheet no longer exists.".to_owned()));
            };
            let done = match change(&mut s, &me, &by) {
                Ok(done) => done,
                Err(why) => return Ok(Err(why)),
            };
            s.version += 1;
            s.touched = store::now();
            tx.sheets().update(&s)?;
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

    /// Puts the name in `who` down for the item the action names.
    fn join(&mut self, out: &mut Outbox, action: &Action) {
        let Some(item) = action.payload_as::<u32>() else {
            return;
        };
        let name = limit(&text(action, "who"), MAX_NAME_LEN);
        if name.is_empty() {
            return out.signal(feedback("failure", "Type in a name first."));
        }
        self.change(out, |s, me, by| {
            let id = s.next_id;
            let i = item_mut(s, item)?;
            if i.is_full(MAX_NAMES) {
                return Err(format!("“{}” is already taken.", i.label));
            }
            if i.names.iter().any(|n| same(&n.name, &name)) {
                return Err(format!("“{name}” is already down for “{}”.", i.label));
            }
            i.names.push(Name {
                id,
                name: name.clone(),
                key: me.to_owned(),
                by: by.to_owned(),
            });
            let label = i.label.clone();
            s.next_id += 1;
            log(s, by, format!("put “{name}” down for “{label}”"));
            Ok(format!("“{name}” is down for “{label}”."))
        });
    }

    /// Takes off the name the action names: one's own, or any for the owner.
    fn leave(&mut self, out: &mut Outbox, action: &Action) {
        let Some(name) = action.payload_as::<u32>() else {
            return;
        };
        self.change(out, |s, me, by| {
            let is_owner = s.owner == me;
            let (i, k) = s
                .items
                .iter()
                .enumerate()
                .find_map(|(i, item)| Some((i, item.names.iter().position(|n| n.id == name)?)))
                .ok_or("That name is no longer on the sheet.")?;
            let item = &mut s.items[i];
            if item.names[k].key != me && !is_owner {
                return Err("You can only take off the names you put down.".to_owned());
            }
            let gone = item.names.remove(k);
            let label = item.label.clone();
            log(s, by, format!("took “{}” off “{label}”", gone.name));
            Ok(format!("“{}” is off “{label}”.", gone.name))
        });
    }

    fn save_item(&mut self, out: &mut Outbox, action: &Action) {
        let Some(item) = action.payload_as::<u32>() else {
            return;
        };
        let at = format!("edit.i{item}");
        let label = limit(&text(action, &format!("{at}.label")), MAX_LABEL_LEN);
        let capacity = read_capacity(&text(action, &format!("{at}.cap")));
        self.change(out, |s, me, by| {
            must_own(s, me)?;
            let capacity = capacity?;
            if label.is_empty() {
                return Err("An item needs a name.".to_owned());
            }
            if s.items
                .iter()
                .any(|i| i.id != item && same(&i.label, &label))
            {
                return Err(format!("“{label}” is on the sheet already."));
            }
            let i = item_mut(s, item)?;
            let n = i.names.len() as u32;
            if capacity.is_some_and(|c| c < n) {
                return Err(format!(
                    "“{}” has {n} names down already; take some off first.",
                    i.label
                ));
            }
            let mut what = Vec::new();
            if i.label != label {
                what.push(format!("renamed “{}” to “{label}”", i.label));
            }
            if i.capacity != capacity {
                let to = match capacity {
                    Some(c) => format!("{c}"),
                    None => "no limit".to_owned(),
                };
                what.push(format!("set “{label}” to take {to}"));
            }
            if what.is_empty() {
                return Err("Nothing to save.".to_owned());
            }
            i.label = label.clone();
            i.capacity = capacity;
            for w in what {
                log(s, by, w);
            }
            Ok(format!("“{label}” is saved."))
        });
    }

    fn remove_item(&mut self, out: &mut Outbox, action: &Action) {
        let Some(item) = action.payload_as::<u32>() else {
            return;
        };
        self.change(out, |s, me, by| {
            must_own(s, me)?;
            let k = s
                .items
                .iter()
                .position(|i| i.id == item)
                .ok_or("That item is no longer on the sheet.")?;
            let gone = s.items.remove(k);
            let names = gone.names.len();
            log(s, by, format!("removed “{}”", gone.label));
            Ok(match names {
                0 => format!("“{}” is removed.", gone.label),
                n => format!(
                    "“{}” is removed, with the {n} name{} down for it.",
                    gone.label,
                    if n == 1 { "" } else { "s" }
                ),
            })
        });
    }

    fn add_item(&mut self, out: &mut Outbox, action: &Action) {
        let label = limit(&text(action, "add.label"), MAX_LABEL_LEN);
        let capacity = read_capacity(&text(action, "add.cap"));
        let added = self.change(out, |s, me, by| {
            must_own(s, me)?;
            let capacity = capacity?;
            if label.is_empty() {
                return Err("An item needs a name.".to_owned());
            }
            if s.items.iter().any(|i| same(&i.label, &label)) {
                return Err(format!("“{label}” is on the sheet already."));
            }
            if s.items.len() >= MAX_ITEMS {
                return Err(format!("At most {MAX_ITEMS} items, please."));
            }
            s.items.push(Item {
                id: s.next_id,
                label: label.clone(),
                capacity,
                names: Vec::new(),
            });
            s.next_id += 1;
            log(s, by, format!("added “{label}”"));
            Ok(format!("“{label}” is added."))
        });
        if added {
            out.signal(json!({"add": {"label": "", "cap": "1"}}));
        }
    }

    fn save_details(&mut self, out: &mut Outbox, action: &Action) {
        let title = limit(&text(action, "details.title"), MAX_TITLE_LEN);
        let description = limit(&text(action, "details.description"), MAX_DESCRIPTION_LEN);
        self.change(out, |s, me, by| {
            must_own(s, me)?;
            if title.is_empty() {
                return Err("Give the sheet a title.".to_owned());
            }
            if s.title == title && s.description == description {
                return Err("Nothing to save.".to_owned());
            }
            if s.title != title {
                log(s, by, format!("renamed the sheet to “{title}”"));
            }
            if s.description != description {
                log(s, by, "changed the description".to_owned());
            }
            s.title = title;
            s.description = description;
            Ok("Saved.".to_owned())
        });
    }

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        let me = self.me();
        // It leaves its owner's list by itself.
        let deleted = store::write(&self.db, |tx| {
            if tx.sheets().get(&id)?.is_none_or(|s| s.owner != me) {
                return Ok(false);
            }
            tx.sheets().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The sheet is deleted."));
        } else {
            out.signal(feedback(
                "failure",
                "Only its creator can delete this sheet.",
            ));
        }
    }
}

/// A fresh id for a sheet.
fn new_id(tx: &WriteTx) -> orangedb::Result<SheetId> {
    loop {
        let id = SheetId::new(cuid2::create_id());
        if !tx.sheets().contains(&id)? {
            return Ok(id);
        }
    }
}

impl App for Sheets {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(Metadata::new().name("signup").description(
            "Sign-up sheets: list what is needed, share the link, people put their names down",
        ));
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"title": "", "description": "", "items": ""},
            "sheet": {"title": "", "description": "", "is_owner": false, "link": "",
                      "fill": "", "full": false, "me": ""},
            "who": "",
            "edit": {},
            "add": {"label": "", "cap": "1"},
            "details": {"title": "", "description": ""},
            "_editing": false,
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("signup"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("New sheet")
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

        // The start page: a new sheet, and the ones made before.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "List the roles to fill or the things to bring, share the link, \
             and people put their names down. Every name shows a short tag of \
             whoever wrote it.",
        ));
        out.element(ui::card("new").parent("home").gap("medium"));
        out.element(
            ui::input("title_in")
                .parent("new")
                .label("Title")
                .bind("draft.title")
                .placeholder("Picnic on Saturday"),
        );
        out.element(
            ui::input("description_in")
                .parent("new")
                .label("What it is for (optional)")
                .variant("multiline")
                .bind("draft.description"),
        );
        out.element(
            ui::input("items_in")
                .parent("new")
                .label("Items, one per line")
                .variant("multiline")
                .placeholder("Drinks\nDessert\nDrivers x3")
                .bind("draft.items"),
        );
        out.element(
            ui::text("items_caption")
                .parent("new")
                .variant("caption")
                .text("Each item takes one name. End a line in “x3” for an item three people can take."),
        );
        out.element(
            ui::button("create_btn")
                .parent("new")
                .label("Make the sheet")
                .variant("primary")
                .on_press(Handler::action("create")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your sheets")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // A link to a sheet that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no sheet here. It may have been deleted."),
        );

        // A sheet.
        out.element(
            ui::column("sheet")
                .parent("main")
                .visible(cel("s.view == 'sheet'")),
        );
        out.element(
            ui::row("sheet_top")
                .parent("sheet")
                .justify("between")
                .wrap(true),
        );
        out.element(
            ui::text("sheet_title")
                .parent("sheet_top")
                .variant("h2")
                .text(cel("s.sheet.title")),
        );
        out.element(
            ui::badge("sheet_fill")
                .parent("sheet_top")
                .variant(cel("s.sheet.full ? 'success' : 'info'"))
                .text(cel("s.sheet.fill")),
        );
        out.element(
            ui::text("sheet_description")
                .parent("sheet")
                .visible(cel("s.sheet.description != ''"))
                .text(cel("s.sheet.description")),
        );
        out.element(ui::card("share").parent("sheet").gap("small"));
        out.element(
            ui::text("share_caption")
                .parent("share")
                .variant("caption")
                .text("Anyone with this link can put their name down:"),
        );
        out.element(
            ui::text("share_link")
                .parent("share")
                .variant("code")
                .text(cel("s.sheet.link")),
        );
        out.element(ui::tabs("tabs").parent("sheet"));
        out.element(ui::tab("items_tab").parent("tabs").label("Sheet"));
        out.element(
            ui::row("who_row")
                .parent("items_tab")
                .wrap(true)
                .align("end"),
        );
        out.element(
            ui::input("who_in")
                .parent("who_row")
                .label("Your name")
                .placeholder("The name to put down")
                .bind("who"),
        );
        out.element(
            ui::button("edit_btn")
                .parent("who_row")
                .label(cel("s._editing ? 'Done editing' : 'Edit the sheet'"))
                .variant("plain")
                .visible(cel("s.sheet.is_owner"))
                .on_press(Handler::patch(json!({"_editing": cel("!s._editing")}))),
        );
        out.element(
            ui::text("me_caption")
                .parent("items_tab")
                .variant("caption")
                .text(cel("s.sheet.me")),
        );
        out.element(
            ui::card("details_card")
                .parent("items_tab")
                .title("Title and description")
                .visible(cel("s.sheet.is_owner && s._editing"))
                .gap("small"),
        );
        out.element(
            ui::input("details_title")
                .parent("details_card")
                .label("Title")
                .bind("details.title"),
        );
        out.element(
            ui::input("details_description")
                .parent("details_card")
                .label("What it is for")
                .variant("multiline")
                .bind("details.description"),
        );
        out.element(
            ui::button("details_btn")
                .parent("details_card")
                .label("Save")
                .on_press(Handler::action("save_details")),
        );
        out.element(ui::column("items").parent("items_tab").gap("medium"));
        out.element(
            ui::card("add_card")
                .parent("items_tab")
                .title("Add an item")
                .visible(cel("s.sheet.is_owner && s._editing"))
                .gap("small"),
        );
        out.element(
            ui::row("add_row")
                .parent("add_card")
                .wrap(true)
                .align("end"),
        );
        out.element(
            ui::input("add_label")
                .parent("add_row")
                .label("Item")
                .bind("add.label"),
        );
        out.element(
            ui::input("add_cap")
                .parent("add_row")
                .label("How many")
                .variant("number")
                .placeholder("No limit")
                .bind("add.cap"),
        );
        out.element(
            ui::button("add_btn")
                .parent("add_row")
                .label("Add")
                .on_press(Handler::action("add_item")),
        );
        out.element(
            ui::button("delete_btn")
                .parent("add_card")
                .label("Delete the sheet")
                .variant("failure")
                .on_press(Handler::action("delete")),
        );
        out.element(ui::tab("history_tab").parent("tabs").label("History"));
        out.element(ui::column("log").parent("history_tab").gap("small"));

        match self.route.clone() {
            Route::Home => self.go_home(out),
            Route::Open(id) => self.open_sheet(out, &id),
            Route::Unknown => self.missing(out),
        }
    }

    fn action(&mut self, out: &mut Outbox, action: &Action) {
        match action.name.as_str() {
            "home" => self.go_home(out),
            "create" => self.create(out, action),
            "open" => {
                if let Some(id) = action.payload_as::<String>() {
                    self.open_sheet(out, &id);
                }
            }
            "join" => self.join(out, action),
            "leave" => self.leave(out, action),
            "save_item" => self.save_item(out, action),
            "remove_item" => self.remove_item(out, action),
            "add_item" => self.add_item(out, action),
            "save_details" => self.save_details(out, action),
            "delete" => self.delete(out),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = store::read(&self.db, |tx| tx.sheets().get(id)).map(|s| s.version);
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
