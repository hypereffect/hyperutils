//! Forms in the spirit of Google Forms, built on identities.
//!
//! Anyone can make a form: a title, a description and any number of
//! questions, which they can change at any time. It lives for the time they
//! picked (a day to a month) and has a link, `hyperb://<host>/<id>`, to hand
//! around. Whoever opens the link fills the form in; each key has one set of
//! answers, which it finds again and can change when it comes back. The
//! results, at `/<id>/results`, are for the key that made the form, or for
//! everyone if it marked the form public. Forms are listed on the start page
//! of the key that made them, so their owner can come back to them.
//!
//! A session watches the form it has open on its tick, so new answers and
//! edits show up within a couple of seconds.

use crate::store::{
    self, Answer, Db, Field, Form, FormId, Kind, Owner, OwnerKey, Response, WriteTx,
};
use hyperb_core::identity::PublicKey;
use hypereffect::{Action, App, Element, Handler, Map, Metadata, Outbox, Value, cel, json, ui};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// How often an open form is checked for news.
const WATCH_MS: u32 = 2000;
const MAX_FIELDS: usize = 40;
const MAX_OPTIONS: usize = 20;
const MAX_TITLE_LEN: usize = 120;
const MAX_DESCRIPTION_LEN: usize = 1000;
const MAX_LABEL_LEN: usize = 200;
const MAX_OPTION_LEN: usize = 100;
const MAX_ANSWER_LEN: usize = 2000;
const MAX_RESPONSES: usize = 2000;
/// How many forms a key's start page lists.
const MAX_OWNED: usize = 50;
/// How many text answers the results list per question.
const MAX_SHOWN: usize = 100;
const DAY: u64 = 24 * 60 * 60;
/// The lifetimes to choose from: the signal value, its label, and seconds.
const LIFETIMES: [(&str, &str, u64); 4] = [
    ("1d", "1 day", DAY),
    ("3d", "3 days", 3 * DAY),
    ("7d", "7 days", 7 * DAY),
    ("30d", "1 month", 30 * DAY),
];

/// Deletes the forms that expired, with their responses, and owners left
/// with none. Returns how many forms went.
pub fn prune(db: &Db) -> usize {
    let now = store::now();
    store::write(db, |tx| {
        let mut n = 0;
        for f in tx.forms().all()? {
            if f.expires <= now {
                tx.forms().delete(&f.id)?;
                n += 1;
            }
        }
        for o in tx.owners().all()? {
            if o.forms.is_empty() {
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
    Fill(String),
    Results(String),
    Edit(String),
    Unknown,
}

impl Route {
    fn parse(path: &str) -> Route {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match segs.as_slice() {
            [] => Route::Home,
            [id] => Route::Fill(id.to_string()),
            [id, "results"] => Route::Results(id.to_string()),
            [id, "edit"] => Route::Edit(id.to_string()),
            _ => Route::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum View {
    Home,
    Edit,
    Fill,
    Results,
    Missing,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Edit => "edit",
            View::Fill => "fill",
            View::Results => "results",
            View::Missing => "missing",
        }
    }
}

fn limit(s: String, max: usize) -> String {
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

/// The `feedback` signal patch for a notice.
fn feedback(kind: &str, text: impl Into<String>) -> Value {
    json!({"feedback": {"kind": kind, "text": text.into()}})
}

/// How long until `at`, roughly.
fn remaining(at: u64) -> String {
    let left = at.saturating_sub(store::now());
    let (n, unit) = match left {
        s if s >= DAY => (s / DAY, "day"),
        s if s >= 3600 => (s / 3600, "hour"),
        s => ((s / 60).max(1), "minute"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// The form with this id, unless it is gone or expired.
fn load(db: &Db, id: &FormId) -> Option<Form> {
    let now = store::now();
    store::read(db, |tx| tx.forms().get(id)).filter(|f| f.expires > now)
}

/// The cards' worth of a field, for the start of an edit.
fn draft_signal(f: &Field) -> Value {
    json!({
        "label": f.label,
        "kind": f.kind.name(),
        "options": f.options.join("\n"),
        "required": f.required,
    })
}

/// A blank answer to `f`, in the shape its controls bind.
fn blank_answer(f: &Field, old: Option<&Answer>) -> Value {
    let values = old.map_or(&[][..], |a| &a.values[..]);
    if f.kind == Kind::Checkboxes {
        let picks: Map<String, Value> = f
            .options
            .iter()
            .enumerate()
            .map(|(k, o)| (format!("o{k}"), values.contains(o).into()))
            .collect();
        Value::Object(picks)
    } else {
        values.first().cloned().unwrap_or_default().into()
    }
}

pub struct Forms {
    db: Arc<Db>,
    /// The key of this session.
    viewer: PublicKey,
    /// `hyperb://<host>`, to write links with.
    origin: String,
    /// Where the address pointed, for the hello.
    route: Route,
    view: View,
    /// The form open in this session, and the version of it on screen.
    open: Option<FormId>,
    seen: u64,
    /// The form the editor saves to; `None` while making a new one.
    editing: Option<FormId>,
    /// The field ids in the editor, in order, and the id the next one gets.
    draft: Vec<u32>,
    next_field: u32,
    /// The shape of the answer signal set up for each field on screen, so
    /// that an edit only resets the answers it has to.
    answer_shapes: HashMap<u32, (Kind, Vec<String>)>,
    /// Ids of the elements that are replaced as a whole.
    mine_ids: Vec<String>,
    draft_ids: Vec<String>,
    fill_ids: Vec<String>,
    result_ids: Vec<String>,
}

impl Forms {
    pub fn new(db: Arc<Db>, viewer: PublicKey, origin: &str, path: &str) -> Self {
        Forms {
            db,
            viewer,
            origin: origin.trim_end_matches('/').to_owned(),
            route: Route::parse(path),
            view: View::Home,
            open: None,
            seen: 0,
            editing: None,
            draft: Vec::new(),
            next_field: 0,
            answer_shapes: HashMap::new(),
            mine_ids: Vec::new(),
            draft_ids: Vec::new(),
            fill_ids: Vec::new(),
            result_ids: Vec::new(),
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
        if view != View::Edit {
            Self::replace(out, &mut self.draft_ids, Vec::new());
        }
        if view != View::Fill {
            Self::replace(out, &mut self.fill_ids, Vec::new());
            self.answer_shapes.clear();
        }
        if view != View::Results {
            Self::replace(out, &mut self.result_ids, Vec::new());
        }
        if !matches!(view, View::Fill | View::Results) {
            self.open = None;
        }
        if view != View::Edit {
            self.editing = None;
        }
        self.view = view;
        out.signal(json!({"view": view.name(), "feedback": {"kind": "info", "text": ""}}));
    }

    // --- the start page ----------------------------------------------------

    fn go_home(&mut self, out: &mut Outbox) {
        self.show(out, View::Home);
        let now = store::now();
        let forms: Vec<(Form, usize)> = store::read(&self.db, |tx| {
            let Some(o) = tx.owners().get(OwnerKey::new(self.me()))? else {
                return Ok(Vec::new());
            };
            let mut forms = Vec::new();
            for id in &o.forms {
                if let Some(f) = tx.forms().get(id)?.filter(|f| f.expires > now) {
                    let n = tx.responses().ids_by_form(id)?.len();
                    forms.push((f, n));
                }
            }
            Ok(forms)
        });
        let buttons = forms
            .iter()
            .enumerate()
            .map(|(k, (f, n))| {
                ui::button(format!("mine_{k}"))
                    .parent("mine_list")
                    .label(format!(
                        "{} · {n} response{} · {} left",
                        f.title,
                        if *n == 1 { "" } else { "s" },
                        remaining(f.expires)
                    ))
                    .on_press(Handler::action("open").payload(f.id.0.as_str()))
            })
            .collect();
        out.signal(json!({"has_mine": !forms.is_empty()}));
        Self::replace(out, &mut self.mine_ids, buttons);
    }

    fn missing(&mut self, out: &mut Outbox) {
        self.show(out, View::Missing);
    }

    // --- the editor ----------------------------------------------------------

    /// Opens the editor on a new form, or on `id` if this key owns it.
    fn start_edit(&mut self, out: &mut Outbox, id: Option<FormId>) {
        let (title, description, public, fields, next, expires) = match &id {
            None => {
                let blank = Field {
                    id: 0,
                    label: String::new(),
                    kind: Kind::Text,
                    options: Vec::new(),
                    required: false,
                };
                (
                    String::new(),
                    String::new(),
                    false,
                    vec![blank],
                    1,
                    String::new(),
                )
            }
            Some(id) => {
                let Some(f) = load(&self.db, id) else {
                    return self.missing(out);
                };
                if f.owner != self.me() {
                    self.open_fill(out, id.clone());
                    return out.signal(feedback("failure", "Only its creator can edit this form."));
                }
                let expires = format!("Expires in {}.", remaining(f.expires));
                (
                    f.title,
                    f.description,
                    f.public,
                    f.fields,
                    f.next_field,
                    expires,
                )
            }
        };
        self.show(out, View::Edit);
        self.editing = id;
        self.draft = fields.iter().map(|f| f.id).collect();
        self.next_field = next;
        let drafts: Map<String, Value> = fields
            .iter()
            .map(|f| (format!("f{}", f.id), draft_signal(f)))
            .collect();
        // The old questions go first, so they do not merge into these.
        out.signal(json!({"draft": {"fields": null}}));
        out.signal(json!({"draft": {
            "title": title,
            "description": description,
            "public": public,
            "lifetime": "7d",
            "editing": self.editing.is_some(),
            "expires": expires,
            "fields": drafts,
        }}));
        self.render_draft(out);
    }

    /// Lays out the questions of the editor in their order.
    fn render_draft(&mut self, out: &mut Outbox) {
        let kinds: Vec<Value> = Kind::ALL
            .iter()
            .map(|k| json!({"value": k.name(), "label": k.label()}))
            .collect();
        let last = self.draft.len().saturating_sub(1);
        let mut elements = Vec::new();
        for (pos, &id) in self.draft.iter().enumerate() {
            let (card, sig) = (format!("d{id}"), format!("draft.fields.f{id}"));
            elements.push(
                ui::card(&card)
                    .parent("draft_fields")
                    .title(format!("Question {}", pos + 1))
                    .gap("small"),
            );
            elements.push(
                ui::input(format!("{card}_label"))
                    .parent(&card)
                    .label("Question")
                    .bind(format!("{sig}.label")),
            );
            elements.push(
                ui::select(format!("{card}_kind"))
                    .parent(&card)
                    .label("Type")
                    .options(kinds.clone())
                    .bind(format!("{sig}.kind")),
            );
            let s = format!("s.{sig}.kind");
            elements.push(
                ui::input(format!("{card}_options"))
                    .parent(&card)
                    .label("Options, one per line")
                    .variant("multiline")
                    .bind(format!("{sig}.options"))
                    .visible(cel(format!(
                        "{s} == 'choice' || {s} == 'dropdown' || {s} == 'checkboxes'"
                    ))),
            );
            elements.push(
                ui::checkbox(format!("{card}_required"))
                    .parent(&card)
                    .label("Required")
                    .variant("switch")
                    .bind(format!("{sig}.required")),
            );
            elements.push(ui::row(format!("{card}_bar")).parent(&card).wrap(true));
            elements.push(
                ui::button(format!("{card}_up"))
                    .parent(format!("{card}_bar"))
                    .label("↑ Up")
                    .variant("plain")
                    .disabled(pos == 0)
                    .on_press(Handler::action("up").payload(id)),
            );
            elements.push(
                ui::button(format!("{card}_down"))
                    .parent(format!("{card}_bar"))
                    .label("↓ Down")
                    .variant("plain")
                    .disabled(pos == last)
                    .on_press(Handler::action("down").payload(id)),
            );
            elements.push(
                ui::button(format!("{card}_remove"))
                    .parent(format!("{card}_bar"))
                    .label("Remove")
                    .variant("plain")
                    .on_press(Handler::action("remove").payload(id)),
            );
        }
        Self::replace(out, &mut self.draft_ids, elements);
    }

    /// Moves, adds or removes a question of the editor.
    fn rearrange(&mut self, out: &mut Outbox, action: &Action) {
        if self.view != View::Edit {
            return;
        }
        let at = action
            .payload_as::<u32>()
            .and_then(|id| self.draft.iter().position(|&d| d == id));
        match (action.name.as_str(), at) {
            ("add", _) => {
                if self.draft.len() >= MAX_FIELDS {
                    return out.signal(feedback(
                        "failure",
                        format!("At most {MAX_FIELDS} questions, please."),
                    ));
                }
                let id = self.next_field;
                self.next_field += 1;
                self.draft.push(id);
                out.signal(json!({"draft": {"fields": {format!("f{id}"): {
                    "label": "", "kind": "text", "options": "", "required": false,
                }}}}));
            }
            ("remove", Some(k)) => {
                self.draft.remove(k);
            }
            ("up", Some(k)) if k > 0 => self.draft.swap(k, k - 1),
            ("down", Some(k)) if k + 1 < self.draft.len() => self.draft.swap(k, k + 1),
            _ => return,
        }
        self.render_draft(out);
    }

    /// The questions in the editor, or why they cannot be saved.
    fn read_draft(&self, action: &Action) -> Result<Vec<Field>, String> {
        if self.draft.is_empty() {
            return Err("Add at least one question.".into());
        }
        let mut fields = Vec::new();
        for (pos, &id) in self.draft.iter().enumerate() {
            let sig = format!("draft.fields.f{id}");
            let label = limit(text(action, &format!("{sig}.label")), MAX_LABEL_LEN);
            if label.is_empty() {
                return Err(format!("Question {} needs a text.", pos + 1));
            }
            let kind = Kind::from_name(&text(action, &format!("{sig}.kind"))).unwrap_or(Kind::Text);
            let mut options: Vec<String> = Vec::new();
            if kind.has_options() {
                for line in text(action, &format!("{sig}.options")).lines() {
                    let line = limit(line.trim().to_owned(), MAX_OPTION_LEN);
                    if !line.is_empty() && !options.contains(&line) {
                        options.push(line);
                    }
                }
                if options.is_empty() {
                    return Err(format!(
                        "Give question {} some options, one per line.",
                        pos + 1
                    ));
                }
                if options.len() > MAX_OPTIONS {
                    return Err(format!(
                        "Question {} has too many options; at most {MAX_OPTIONS}.",
                        pos + 1
                    ));
                }
            }
            fields.push(Field {
                id,
                label,
                kind,
                options,
                required: flag(action, &format!("{sig}.required")),
            });
        }
        Ok(fields)
    }

    fn save(&mut self, out: &mut Outbox, action: &Action) {
        if self.view != View::Edit {
            return;
        }
        let fail = |out: &mut Outbox, why: &str| out.signal(feedback("failure", why));
        let title = limit(text(action, "draft.title"), MAX_TITLE_LEN);
        if title.is_empty() {
            return fail(out, "Give the form a title.");
        }
        let fields = match self.read_draft(action) {
            Ok(f) => f,
            Err(why) => return fail(out, &why),
        };
        let description = limit(text(action, "draft.description"), MAX_DESCRIPTION_LEN);
        let public = flag(action, "draft.public");
        let next_field = self.next_field;
        let me = self.me();
        let saved = match self.editing.clone() {
            Some(id) => store::write(&self.db, |tx| {
                let Some(mut f) = tx.forms().get(&id)? else {
                    return Ok(Err("This form no longer exists."));
                };
                if f.owner != me {
                    return Ok(Err("Only its creator can edit this form."));
                }
                f.title = title;
                f.description = description;
                f.public = public;
                f.fields = fields;
                f.next_field = next_field;
                f.version += 1;
                tx.forms().update(&f)?;
                Ok(Ok(id))
            }),
            None => {
                let lifetime = text(action, "draft.lifetime");
                let Some(&(_, _, secs)) = LIFETIMES.iter().find(|l| l.0 == lifetime) else {
                    return fail(out, "Pick how long the form stays open.");
                };
                Ok(store::write(&self.db, |tx| {
                    let id = new_id(tx)?;
                    tx.forms().insert(&Form {
                        id: id.clone(),
                        owner: me.clone(),
                        title,
                        description,
                        fields,
                        public,
                        expires: store::now() + secs,
                        next_field,
                        version: 1,
                    })?;
                    let key = OwnerKey::new(me);
                    let mut o = tx.owners().get(&key)?.unwrap_or(Owner {
                        key,
                        forms: Vec::new(),
                    });
                    o.forms.insert(0, id.clone());
                    o.forms.truncate(MAX_OWNED);
                    tx.owners().upsert(&o)?;
                    Ok(id)
                }))
            }
        };
        let created = self.editing.is_none();
        match saved {
            Ok(id) => {
                self.open_fill(out, id);
                out.signal(feedback(
                    "success",
                    if created {
                        "Your form is ready. Share the link below."
                    } else {
                        "Saved."
                    },
                ));
            }
            Err(why) => fail(out, why),
        }
    }

    fn delete(&mut self, out: &mut Outbox) {
        let Some(id) = self.editing.clone().filter(|_| self.view == View::Edit) else {
            return;
        };
        let me = self.me();
        // Its responses go with it, and it leaves its owner's list.
        let deleted = store::write(&self.db, |tx| {
            if tx.forms().get(&id)?.is_none_or(|f| f.owner != me) {
                return Ok(false);
            }
            tx.forms().delete(&id)?;
            Ok(true)
        });
        if deleted {
            self.go_home(out);
            out.signal(feedback("info", "The form is deleted, with its responses."));
        } else {
            out.signal(feedback(
                "failure",
                "Only its creator can delete this form.",
            ));
        }
    }

    // --- filling in ----------------------------------------------------------

    /// What every view of a form shows about it.
    fn form_signal(&self, f: &Form, responses: usize, answered: bool) -> Value {
        let link = format!("{}/{}", self.origin, f.id.0);
        let is_owner = f.owner == self.me();
        json!({"form": {
            "title": f.title,
            "description": f.description,
            "public": f.public,
            "is_owner": is_owner,
            "can_see": f.public || is_owner,
            "answered": answered,
            "link": link,
            "results_link": format!("{link}/results"),
            "expires": format!("Closes in {}", remaining(f.expires)),
            "count": format!("{responses} response{}", if responses == 1 { "" } else { "s" }),
        }})
    }

    fn open_fill(&mut self, out: &mut Outbox, id: FormId) {
        if self.view != View::Fill || self.open.as_ref() != Some(&id) {
            self.show(out, View::Fill);
        }
        self.open = Some(id);
        self.refresh_fill(out);
    }

    /// Shows the open form as it is now, keeping the answers being typed
    /// for the questions that did not change.
    fn refresh_fill(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        let me = self.me();
        let found = store::read(&self.db, |tx| {
            let Some(f) = tx.forms().get(&id)? else {
                return Ok(None);
            };
            let responses = tx.responses().by_form(&id)?;
            let n = responses.len();
            Ok(Some((f, n, responses.into_iter().find(|r| r.key == me))))
        });
        let Some((f, n, mine)) = found.filter(|(f, ..)| f.expires > store::now()) else {
            return self.missing(out);
        };
        self.seen = f.version;
        out.signal(self.form_signal(&f, n, mine.is_some()));
        let mut answers = Map::new();
        for field in &f.fields {
            let shape = (field.kind, field.options.clone());
            if self.answer_shapes.get(&field.id) != Some(&shape) {
                let old = mine
                    .as_ref()
                    .and_then(|r| r.answers.iter().find(|a| a.field == field.id));
                // Null first, so that a new shape does not merge into the old.
                out.signal(json!({"answer": {format!("f{}", field.id): null}}));
                answers.insert(format!("f{}", field.id), blank_answer(field, old));
                self.answer_shapes.insert(field.id, shape);
            }
        }
        if !answers.is_empty() {
            out.signal(json!({ "answer": answers }));
        }
        let mut elements = Vec::new();
        for field in &f.fields {
            let (card, sig) = (format!("q{}", field.id), format!("answer.f{}", field.id));
            let title = if field.required {
                format!("{} *", field.label)
            } else {
                field.label.clone()
            };
            elements.push(
                ui::card(&card)
                    .parent("fill_fields")
                    .title(title)
                    .gap("small"),
            );
            let options: Vec<Value> = field
                .options
                .iter()
                .map(|o| json!({"value": o, "label": o}))
                .collect();
            let input = format!("{card}_in");
            match field.kind {
                Kind::Text => elements.push(ui::input(input).parent(&card).bind(sig)),
                Kind::Paragraph => elements.push(
                    ui::input(input)
                        .parent(&card)
                        .variant("multiline")
                        .bind(sig),
                ),
                Kind::Number => {
                    elements.push(ui::input(input).parent(&card).variant("number").bind(sig))
                }
                Kind::Choice | Kind::Dropdown => elements.push(
                    ui::select(input)
                        .parent(&card)
                        .variant(if field.kind == Kind::Choice {
                            "radio"
                        } else {
                            "dropdown"
                        })
                        .options(options)
                        .bind(sig),
                ),
                Kind::Checkboxes => {
                    for (k, o) in field.options.iter().enumerate() {
                        elements.push(
                            ui::checkbox(format!("{card}_o{k}"))
                                .parent(&card)
                                .label(o.as_str())
                                .bind(format!("{sig}.o{k}")),
                        );
                    }
                }
            }
        }
        Self::replace(out, &mut self.fill_ids, elements);
    }

    fn submit(&mut self, out: &mut Outbox, action: &Action) {
        let Some(id) = self.open.clone().filter(|_| self.view == View::Fill) else {
            return;
        };
        let me = self.me();
        let result = store::write(&self.db, |tx| {
            let Some(mut f) = tx.forms().get(&id)?.filter(|f| f.expires > store::now()) else {
                return Ok(Err("This form no longer exists.".to_owned()));
            };
            let answers = match read_answers(&f, action) {
                Ok(a) => a,
                Err(why) => return Ok(Err(why)),
            };
            let responses = tx.responses().by_form(&id)?;
            match responses.into_iter().find(|r| r.key == me) {
                Some(mut r) => {
                    r.answers = answers;
                    r.at = store::now();
                    tx.responses().update(&r)?;
                }
                None if tx.responses().ids_by_form(&id)?.len() >= MAX_RESPONSES => {
                    return Ok(Err("This form takes no more responses.".to_owned()));
                }
                None => {
                    tx.responses().create(|rid| Response {
                        id: rid,
                        form: id.clone(),
                        key: me.clone(),
                        answers,
                        at: store::now(),
                    })?;
                }
            }
            f.version += 1;
            tx.forms().update(&f)?;
            Ok(Ok(()))
        });
        match result {
            Ok(()) => {
                self.refresh_fill(out);
                out.signal(feedback(
                    "success",
                    "Thank you, your answers are saved. You can come back and change them.",
                ));
            }
            Err(why) => out.signal(feedback("failure", why)),
        }
    }

    // --- results -------------------------------------------------------------

    fn open_results(&mut self, out: &mut Outbox, id: FormId) {
        if self.view != View::Results || self.open.as_ref() != Some(&id) {
            self.show(out, View::Results);
        }
        self.open = Some(id);
        self.refresh_results(out);
    }

    fn refresh_results(&mut self, out: &mut Outbox) {
        let Some(id) = self.open.clone() else { return };
        let found = store::read(&self.db, |tx| match tx.forms().get(&id)? {
            Some(f) => Ok(Some((f, tx.responses().by_form(&id)?))),
            None => Ok(None),
        });
        let Some((f, mut responses)) = found.filter(|(f, _)| f.expires > store::now()) else {
            return self.missing(out);
        };
        self.seen = f.version;
        let me = self.me();
        out.signal(self.form_signal(&f, responses.len(), responses.iter().any(|r| r.key == me)));
        if !(f.public || f.owner == me) {
            return Self::replace(out, &mut self.result_ids, Vec::new());
        }
        responses.sort_by_key(|r| std::cmp::Reverse(r.at));
        let total = responses.len();
        let mut elements = Vec::new();
        for field in &f.fields {
            let card = format!("r{}", field.id);
            let values: Vec<&Vec<String>> = responses
                .iter()
                .filter_map(|r| r.answers.iter().find(|a| a.field == field.id))
                .map(|a| &a.values)
                .filter(|v| !v.is_empty())
                .collect();
            elements.push(
                ui::card(&card)
                    .parent("result_fields")
                    .title(field.label.as_str())
                    .gap("small"),
            );
            elements.push(
                ui::text(format!("{card}_count"))
                    .parent(&card)
                    .variant("caption")
                    .text(format!("{} of {total} answered", values.len())),
            );
            if field.kind.has_options() {
                for (k, o) in field.options.iter().enumerate() {
                    let n = values.iter().filter(|v| v.contains(o)).count();
                    let share = if total == 0 {
                        0.0
                    } else {
                        n as f64 / total as f64
                    };
                    elements.push(
                        ui::progress(format!("{card}_o{k}"))
                            .parent(&card)
                            .label(format!("{o} — {n}"))
                            .value(share),
                    );
                }
                continue;
            }
            if field.kind == Kind::Number {
                let nums: Vec<f64> = values.iter().filter_map(|v| v[0].parse().ok()).collect();
                if !nums.is_empty() {
                    let sum: f64 = nums.iter().sum();
                    let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
                    let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    elements.push(
                        ui::text(format!("{card}_stats"))
                            .parent(&card)
                            .text(format!(
                                "Average {} · lowest {min} · highest {max}",
                                (sum / nums.len() as f64 * 100.0).round() / 100.0
                            )),
                    );
                }
            }
            for (n, v) in values.iter().take(MAX_SHOWN).enumerate() {
                elements.push(
                    ui::text(format!("{card}_{n}"))
                        .parent(&card)
                        .text(v[0].as_str()),
                );
            }
            if values.len() > MAX_SHOWN {
                elements.push(
                    ui::text(format!("{card}_more"))
                        .parent(&card)
                        .variant("caption")
                        .text(format!("…and {} more", values.len() - MAX_SHOWN)),
                );
            }
        }
        Self::replace(out, &mut self.result_ids, elements);
    }

    /// Opens the form the action names, in `view`.
    fn goto(&mut self, out: &mut Outbox, view: View, id: Option<FormId>) {
        let Some(id) = id
            .or_else(|| self.open.clone())
            .or_else(|| self.editing.clone())
        else {
            return self.go_home(out);
        };
        match view {
            View::Results => self.open_results(out, id),
            View::Edit => self.start_edit(out, Some(id)),
            _ => self.open_fill(out, id),
        }
    }
}

/// The answers the action carries for `f`, or what is wrong with them.
fn read_answers(f: &Form, action: &Action) -> Result<Vec<store::Answer>, String> {
    let mut answers = Vec::new();
    for field in &f.fields {
        let sig = format!("answer.f{}", field.id);
        let values: Vec<String> = match field.kind {
            Kind::Text | Kind::Paragraph | Kind::Number => {
                let t = limit(text(action, &sig), MAX_ANSWER_LEN);
                if field.kind == Kind::Number && !t.is_empty() && t.parse::<f64>().is_err() {
                    return Err(format!("“{}” takes a number.", field.label));
                }
                Some(t).filter(|t| !t.is_empty()).into_iter().collect()
            }
            Kind::Choice | Kind::Dropdown => {
                let t = text(action, &sig);
                field.options.iter().filter(|o| **o == t).cloned().collect()
            }
            Kind::Checkboxes => field
                .options
                .iter()
                .enumerate()
                .filter(|(k, _)| flag(action, &format!("{sig}.o{k}")))
                .map(|(_, o)| o.clone())
                .collect(),
        };
        if field.required && values.is_empty() {
            return Err(format!("“{}” needs an answer.", field.label));
        }
        answers.push(store::Answer {
            field: field.id,
            values,
        });
    }
    Ok(answers)
}

fn new_id(tx: &WriteTx) -> orangedb::Result<FormId> {
    loop {
        let id = FormId::new(cuid2::create_id());
        if !tx.forms().contains(&id)? {
            return Ok(id);
        }
    }
}

impl App for Forms {
    fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
        out.metadata(
            Metadata::new()
                .name("forms")
                .description("Make a form, share its link, collect the answers"),
        );
        let lifetimes: Vec<Value> = LIFETIMES
            .iter()
            .map(|(v, l, _)| json!({"value": v, "label": l}))
            .collect();
        out.signal(json!({
            "view": "home",
            "has_mine": false,
            "feedback": {"kind": "info", "text": ""},
            "draft": {"title": "", "description": "", "public": false, "lifetime": "7d",
                      "editing": false, "expires": "", "fields": {}},
            "form": {"title": "", "description": "", "public": false, "is_owner": false,
                     "can_see": false, "answered": false, "link": "", "results_link": "",
                     "expires": "", "count": ""},
            "answer": {},
        }));
        out.element(ui::column("main").gap("large"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("app_title")
                .parent("top")
                .variant("h1")
                .text("forms"),
        );
        out.element(
            ui::button("home_btn")
                .parent("top")
                .label("Your forms")
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

        // The start page.
        out.element(
            ui::column("home")
                .parent("main")
                .visible(cel("s.view == 'home'")),
        );
        out.element(ui::text("intro").parent("home").text(
            "Make a form with as many questions as you like, share its link, and \
             see the answers come in. Forms are tied to your identity, so you \
             can come back to edit them and read the results.",
        ));
        out.element(
            ui::button("new_btn")
                .parent("home")
                .label("New form")
                .variant("primary")
                .on_press(Handler::action("new")),
        );
        out.element(
            ui::card("mine")
                .parent("home")
                .title("Your forms")
                .visible(cel("s.has_mine")),
        );
        out.element(ui::list("mine_list").parent("mine"));

        // A link to a form that is not there.
        out.element(
            ui::column("missing")
                .parent("main")
                .visible(cel("s.view == 'missing'")),
        );
        out.element(
            ui::notice("missing_notice")
                .parent("missing")
                .variant("warning")
                .text("There is no form here. It may have expired, or been deleted."),
        );
        out.element(
            ui::button("missing_new")
                .parent("missing")
                .label("Make your own form")
                .on_press(Handler::action("new")),
        );

        // The editor.
        out.element(
            ui::column("edit")
                .parent("main")
                .visible(cel("s.view == 'edit'")),
        );
        out.element(
            ui::text("edit_title")
                .parent("edit")
                .variant("h2")
                .text(cel("s.draft.editing ? 'Edit form' : 'New form'")),
        );
        out.element(ui::card("settings").parent("edit").gap("medium"));
        out.element(
            ui::input("title_in")
                .parent("settings")
                .label("Title")
                .bind("draft.title")
                .placeholder("Team offsite survey"),
        );
        out.element(
            ui::input("desc_in")
                .parent("settings")
                .label("Description (optional)")
                .variant("multiline")
                .bind("draft.description"),
        );
        out.element(
            ui::select("lifetime_in")
                .parent("settings")
                .label("Keep it open for")
                .variant("segmented")
                .options(lifetimes)
                .bind("draft.lifetime")
                .visible(cel("!s.draft.editing")),
        );
        out.element(
            ui::text("expires_text")
                .parent("settings")
                .variant("caption")
                .text(cel("s.draft.expires"))
                .visible(cel("s.draft.editing")),
        );
        out.element(
            ui::checkbox("public_in")
                .parent("settings")
                .label("Public results: anyone with the link can see the answers")
                .variant("switch")
                .bind("draft.public"),
        );
        out.element(ui::column("draft_fields").parent("edit"));
        out.element(ui::row("edit_bar").parent("edit").wrap(true));
        out.element(
            ui::button("add_btn")
                .parent("edit_bar")
                .label("+ Add question")
                .on_press(Handler::action("add")),
        );
        out.element(
            ui::button("save_btn")
                .parent("edit_bar")
                .label(cel("s.draft.editing ? 'Save changes' : 'Create form'"))
                .variant("primary")
                .on_press(Handler::action("save")),
        );
        out.element(
            ui::button("cancel_btn")
                .parent("edit_bar")
                .label("Cancel")
                .variant("plain")
                .on_press(Handler::action("cancel")),
        );
        out.element(
            ui::button("delete_btn")
                .parent("edit_bar")
                .label("Delete form")
                .variant("failure")
                .visible(cel("s.draft.editing"))
                .on_press(Handler::action("delete")),
        );

        // What a form's pages share: its title, and the owner's tools.
        out.element(
            ui::column("form_head")
                .parent("main")
                .visible(cel("s.view == 'fill' || s.view == 'results'")),
        );
        out.element(
            ui::text("form_title")
                .parent("form_head")
                .variant("h2")
                .text(cel("s.form.title")),
        );
        out.element(
            ui::text("form_desc")
                .parent("form_head")
                .visible(cel("s.form.description != ''"))
                .text(cel("s.form.description")),
        );
        out.element(ui::row("form_badges").parent("form_head").wrap(true));
        out.element(
            ui::badge("privacy")
                .parent("form_badges")
                .variant(cel("s.form.public ? 'info' : 'neutral'"))
                .text(cel(
                    "s.form.public ? 'Public results: anyone can see the answers' \
                     : 'Private: only the creator sees the answers'",
                )),
        );
        out.element(
            ui::badge("expires")
                .parent("form_badges")
                .text(cel("s.form.expires")),
        );
        out.element(
            ui::card("owner")
                .parent("form_head")
                .title("Your form")
                .visible(cel("s.form.is_owner"))
                .gap("small"),
        );
        out.element(
            ui::text("share_caption")
                .parent("owner")
                .variant("caption")
                .text("Share this link to collect answers:"),
        );
        out.element(
            ui::text("share_link")
                .parent("owner")
                .variant("code")
                .text(cel("s.form.link")),
        );
        out.element(
            ui::text("results_caption")
                .parent("owner")
                .variant("caption")
                .text(cel("'Results: ' + s.form.results_link")),
        );
        out.element(ui::row("owner_bar").parent("owner").wrap(true));
        out.element(
            ui::button("edit_btn")
                .parent("owner_bar")
                .label("Edit")
                .on_press(Handler::action("edit")),
        );

        // Filling in.
        out.element(
            ui::column("fill")
                .parent("main")
                .visible(cel("s.view == 'fill'")),
        );
        out.element(ui::column("fill_fields").parent("fill"));
        out.element(ui::row("fill_bar").parent("fill").wrap(true));
        out.element(
            ui::button("submit_btn")
                .parent("fill_bar")
                .label(cel("s.form.answered ? 'Update my answers' : 'Submit'"))
                .variant("primary")
                .on_press(Handler::action("submit")),
        );
        out.element(
            ui::button("results_btn")
                .parent("fill_bar")
                .label(cel("'See results (' + s.form.count + ')'"))
                .visible(cel("s.form.can_see"))
                .on_press(Handler::action("results")),
        );
        out.element(
            ui::text("fill_caption")
                .parent("fill")
                .variant("caption")
                .text("Your answers are tied to your identity: come back with it to change them."),
        );

        // Results.
        out.element(
            ui::column("results")
                .parent("main")
                .visible(cel("s.view == 'results'")),
        );
        out.element(
            ui::notice("private_notice")
                .parent("results")
                .variant("warning")
                .visible(cel("!s.form.can_see"))
                .text("The results of this form are private: only its creator can see them."),
        );
        out.element(ui::row("results_bar").parent("results").wrap(true));
        out.element(
            ui::badge("count")
                .parent("results_bar")
                .variant("success")
                .visible(cel("s.form.can_see"))
                .text(cel("s.form.count")),
        );
        out.element(
            ui::button("fill_btn")
                .parent("results_bar")
                .label(cel(
                    "s.form.answered ? 'Change my answers' : 'Fill in the form'",
                ))
                .on_press(Handler::action("fill")),
        );
        out.element(ui::column("result_fields").parent("results"));

        match self.route.clone() {
            Route::Home => self.go_home(out),
            Route::Fill(id) => self.open_fill(out, FormId::new(id)),
            Route::Results(id) => self.open_results(out, FormId::new(id)),
            Route::Edit(id) => self.start_edit(out, Some(FormId::new(id))),
            Route::Unknown => self.missing(out),
        }
    }

    fn action(&mut self, out: &mut Outbox, action: &Action) {
        let named = action.payload_as::<String>().map(FormId::new);
        match action.name.as_str() {
            "home" => self.go_home(out),
            "new" => self.start_edit(out, None),
            "open" => self.goto(out, View::Results, named),
            "fill" => self.goto(out, View::Fill, None),
            "results" => self.goto(out, View::Results, None),
            "edit" => self.goto(out, View::Edit, None),
            "cancel" => match self.editing.clone() {
                Some(id) => self.open_fill(out, id),
                None => self.go_home(out),
            },
            "add" | "remove" | "up" | "down" => self.rearrange(out, action),
            "save" => self.save(out, action),
            "delete" => self.delete(out),
            "submit" => self.submit(out, action),
            _ => {}
        }
    }

    fn tick(&mut self, out: &mut Outbox, _: f64) {
        let Some(id) = &self.open else { return };
        let version = load(&self.db, id).map(|f| f.version);
        if version == Some(self.seen) {
            return;
        }
        match self.view {
            View::Fill => self.refresh_fill(out),
            View::Results => self.refresh_results(out),
            _ => {}
        }
        if self.view == View::Missing {
            out.signal(feedback("warning", "This form was deleted, or it expired."));
        }
    }

    fn tick_interval(&self) -> Option<u32> {
        self.open.is_some().then_some(WATCH_MS)
    }
}

#[cfg(test)]
mod tests;
