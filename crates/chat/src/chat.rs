//! One member's session: the rooms on the left, the messages of the one
//! picked on the right, and a line to write in, drawn the way hypereffect's
//! server guide asks: build once, then change as little as possible.
//!
//! The frame is sent once, when the caller is in. From then on the two
//! lists (rooms and messages) gain and lose rows through [`reconcile`], and
//! what can change about a row that is already there (whose turn it is to be
//! highlighted, a member's name and picture) lives in signals that its
//! expressions read. Renaming yourself changes one signal, not every message
//! you wrote. Every `sync_*` works out what to show from the database, so
//! it is idempotent: a session applies its own changes in the reply to the
//! action, and the echo of the same change from the hub finds nothing left
//! to do.
//!
//! A room opens on its latest [`PAGE`] messages. "Show older" moves the
//! start of the window back by another [`PAGE`]; new messages are added at
//! the end, so the window only grows while the room is open.
//!
//! Signals:
//! - `f`: form fields, bound to inputs. The only signals the server reads.
//! - `_busy`: the indicator of every button that asks the server to change
//!   something; every reply resets it.
//! - `_dlg`: the dialog that is open (`profile`, `invite` or none), which
//!   the dialogs close by themselves.
//! - everything else (`_who`, `_room`, `_older`…) is display state that only
//!   the server writes, through [`Chat::put`], which sends only what changed.
//!   `_who.k<key>` is a member's `name` and `pic`.
//!
//! User text goes into literal attributes or signals, never into CEL.

use crate::avatar::default_picture;
use crate::db::{JoinError, Member, MemberKey, Message, MessageId, Room, RoomError, RoomId};
use crate::hub::{Event, Hub};
use chrono::{Local, TimeZone};
use hypereffect::signals::merge_patch;
use hypereffect::vtree::{self, Node as Tree};
use hypereffect::{Action, Element, Handler, Map, Metadata, Outbox, Value, cel, json, ui};
use std::collections::BTreeMap;
use std::sync::Arc;

/// How many messages a room opens on, and how many more "Show older" adds.
pub const PAGE: usize = 100;
const MAX_NAME: usize = 32;
const MAX_ROOM: usize = 32;
const MAX_MESSAGE: usize = 2000;
/// The longest `data:` picture taken, in bytes. Every session gets every
/// member's picture, so they are kept small.
const MAX_DATA_PICTURE: usize = 48 * 1024;
const MAX_LINK_PICTURE: usize = 1000;

pub struct Chat {
    hub: Arc<Hub>,
    /// The caller's hyperb key, in hex.
    key: MemberKey,
    started: bool,
    /// Whether the caller has joined, and so sees the rooms.
    member: bool,
    /// The room open, and the first of its messages on screen (0: from the
    /// first message of the room).
    room: Option<RoomId>,
    from: MessageId,
    /// The rows of the lists, as the renderer has them.
    rooms: Vec<Tree>,
    messages: Vec<Tree>,
    /// The display signals (see the module documentation) as the renderer
    /// has them.
    shown: Value,
}

impl Chat {
    pub fn new(hub: Arc<Hub>, key: MemberKey) -> Self {
        Chat {
            hub,
            key,
            started: false,
            member: false,
            room: None,
            from: MessageId(0),
            rooms: Vec::new(),
            messages: Vec::new(),
            shown: Value::Null,
        }
    }

    pub fn hello(&mut self, out: &mut Outbox, _params: &BTreeMap<String, String>) {
        if self.started {
            return;
        }
        self.started = true;
        let name = &self.hub.config.name;
        out.metadata(
            Metadata::new()
                .name(name)
                .description("Chat rooms, live"),
        );
        // The signals the renderer writes too go out directly; `put` keeps
        // track of the others.
        out.signal(json!({"f": {}, "_busy": false, "_dlg": ""}));
        self.put(
            out,
            json!({
                "_flash": {"kind": "info", "text": ""},
                "_who": {},
                "_room": 0,
                "_room_name": "",
                "_older": false,
                "_invite": "",
            }),
        );
        self.member = self.hub.store.member(&self.key).is_some();
        if self.member {
            self.enter(out);
        } else {
            self.draw_join(out);
        }
    }

    pub fn action(&mut self, out: &mut Outbox, a: &Action) {
        if !self.started {
            return;
        }
        self.flash(out, "info", "");
        self.dispatch(out, a);
        // Whatever happened, the request is over.
        out.signal(json!({"_busy": false}));
    }

    fn dispatch(&mut self, out: &mut Outbox, a: &Action) {
        let field = |path: &str| {
            a.signal(&format!("f.{path}"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if !self.member {
            if a.name == "join" {
                self.join(out, &field("name"), &field("picture"), &field("invite"));
            }
            return;
        }
        match a.name.as_str() {
            "open" => {
                if let Some(id) = a.payload_as::<u64>() {
                    self.open(out, RoomId(id));
                }
            }
            "older" => self.older(out),
            "send" => self.send(out, &field("msg")),
            "create_room" => self.create_room(out, &field("room")),
            "profile" => self.edit_profile(out),
            "save_profile" => self.save_profile(out, &field("name"), &field("picture")),
            "make_invite" => self.make_invite(out, &field("uses")),
            _ => {}
        }
    }

    /// What the other sessions did.
    pub fn event(&mut self, out: &mut Outbox, e: &Event) {
        if !self.member {
            return;
        }
        match e {
            Event::Message { room } if Some(*room) == self.room => self.sync_messages(out),
            Event::Message { .. } => {}
            Event::Rooms => self.sync_rooms(out),
            Event::Members => self.sync_who(out),
        }
    }

    /// Catches up after this session missed events.
    pub fn refresh(&mut self, out: &mut Outbox) {
        if self.member {
            self.sync_who(out);
            self.sync_rooms(out);
            self.sync_messages(out);
        }
    }

    // Signals.

    /// Writes display signals, sending only what changes. Never for signals
    /// the renderer also writes (`f`, `_busy`, `_dlg`): `shown` would not
    /// know their value.
    fn put(&mut self, out: &mut Outbox, patch: Value) {
        let mut next = self.shown.clone();
        merge_patch(&mut next, &patch);
        if let Some(d) = diff(&self.shown, &next) {
            out.signal(d);
        }
        self.shown = next;
    }

    fn flash(&mut self, out: &mut Outbox, kind: &str, text: &str) {
        self.put(out, json!({"_flash": {"kind": kind, "text": text}}));
    }

    /// Every member's name and picture.
    fn sync_who(&mut self, out: &mut Outbox) {
        let who: Map<String, Value> = self
            .hub
            .store
            .members()
            .iter()
            .map(|m| (who_key(&m.key), json!({"name": m.name, "pic": picture_of(m)})))
            .collect();
        self.put(out, json!({"_who": who}));
    }

    fn sync_rooms(&mut self, out: &mut Outbox) {
        let want = self.hub.store.rooms().iter().map(room_item).collect();
        reconcile(out, &mut self.rooms, "rooms", want);
    }

    /// The open room's messages from the start of the window on.
    fn sync_messages(&mut self, out: &mut Outbox) {
        let Some(room) = self.room else {
            return;
        };
        let (messages, older) = self.hub.store.messages_from(room, self.from);
        let want = messages.iter().map(message_item).collect();
        reconcile(out, &mut self.messages, "msgs", want);
        self.put(out, json!({"_older": older}));
    }

    // Joining.

    fn draw_join(&self, out: &mut Outbox) {
        let config = &self.hub.config;
        out.element(ui::column("main"));
        out.element(
            ui::text("title")
                .parent("main")
                .variant("h1")
                .text(config.name.as_str()),
        );
        out.element(flash());
        out.element(
            ui::card("join_card")
                .parent("main")
                .title(format!("Join {}", config.name)),
        );
        let intro = if config.invite_only {
            "This server is invite-only: type the invite you were given. \
             Your hyperb key becomes your account, so there is no password, \
             and the invite is only needed once."
        } else {
            "Your hyperb key is your account, so there is no password. \
             Pick a name to be known by."
        };
        out.element(ui::text("join_intro").parent("join_card").text(intro));
        if config.invite_only {
            out.element(
                ui::input("join_invite")
                    .parent("join_card")
                    .label("Invite")
                    .bind("f.invite"),
            );
        }
        out.element(
            ui::input("join_name")
                .parent("join_card")
                .label("Name")
                .bind("f.name")
                .placeholder(format!("Up to {MAX_NAME} characters")),
        );
        out.element(picture_input("join_picture", "join_card"));
        out.element(picture_help("join_picture_help", "join_card"));
        out.element(submit(
            "join",
            "join_card",
            "Join",
            "primary",
            Handler::action("join"),
        ));
    }

    fn join(&mut self, out: &mut Outbox, name: &str, picture: &str, invite: &str) {
        let invite_only = self.hub.config.invite_only;
        if invite_only && invite.is_empty() {
            return self.flash(out, "failure", "Type your invite first.");
        }
        if let Err(why) = check_name(name).and_then(|_| check_picture(picture)) {
            return self.flash(out, "failure", &why);
        }
        let invite = invite_only.then_some(invite);
        match self.hub.store.join(&self.key, name, picture, invite, now()) {
            Ok(()) => {}
            Err(JoinError::BadInvite) => {
                return self.flash(out, "failure", "That invite is unknown or used up.");
            }
        }
        println!("{name} joined ({})", self.key.0);
        self.member = true;
        out.remove("main");
        self.hub.emit(Event::Members);
        self.enter(out);
    }

    // The rooms.

    /// Draws the frame and opens the first room.
    fn enter(&mut self, out: &mut Outbox) {
        out.signal(json!({"f": null}));
        out.signal(json!({"f": {"msg": "", "room": "", "name": "", "picture": "", "uses": "1"}}));
        self.draw_frame(out);
        self.sync_who(out);
        self.sync_rooms(out);
        if let Some(first) = self.hub.store.rooms().first() {
            self.open(out, first.id);
        }
    }

    fn draw_frame(&self, out: &mut Outbox) {
        let me = format!("s._who.{}", who_key(&self.key));
        out.element(ui::column("main").gap("small"));
        out.element(ui::row("top").parent("main").justify("between").wrap(true));
        out.element(
            ui::text("title")
                .parent("top")
                .variant("h2")
                .text(self.hub.config.name.as_str()),
        );
        out.element(ui::row("me").parent("top"));
        out.element(
            ui::image("me_pic")
                .parent("me")
                .variant("avatar")
                .src(cel(format!("{me}.pic")))
                .alt(cel(format!("{me}.name"))),
        );
        out.element(
            ui::button("profile")
                .parent("me")
                .label(cel(format!("{me}.name")))
                .variant("plain")
                .on_press(Handler::action("profile")),
        );
        if self.hub.config.invite_only {
            out.element(
                ui::button("invite")
                    .parent("me")
                    .label("Invite")
                    .on_press(Handler::patch(json!({"_dlg": "invite"}))),
            );
        }
        out.element(flash());
        out.element(ui::row("layout").parent("main").align("start").gap("large"));

        // The left column: the rooms, and a line to make one.
        out.element(ui::column("side").parent("layout").gap("small"));
        out.element(ui::text("rooms_h").parent("side").variant("h3").text("Rooms"));
        out.element(ui::list("rooms").parent("side").empty("No rooms yet."));
        out.element(ui::divider("side_div").parent("side"));
        out.element(
            ui::input("new_room")
                .parent("side")
                .bind("f.room")
                .placeholder("New room")
                .on_submit(Handler::action("create_room").indicator("_busy")),
        );
        out.element(submit(
            "create_room",
            "side",
            "Create room",
            "secondary",
            Handler::action("create_room"),
        ));

        // The right column: the open room.
        out.element(
            ui::column("pane")
                .parent("layout")
                .gap("small")
                .visible(cel("s._room > 0")),
        );
        out.element(
            ui::text("room_h")
                .parent("pane")
                .variant("h3")
                .text(cel("'# ' + s._room_name")),
        );
        out.element(
            ui::button("older")
                .parent("pane")
                .label("Show older messages")
                .variant("plain")
                .visible(cel("s._older"))
                .disabled(cel("s._busy"))
                .on_press(Handler::action("older").indicator("_busy")),
        );
        out.element(ui::list("msgs").parent("pane").empty("No messages yet. Say hi!"));
        out.element(ui::row("compose").parent("pane"));
        // The field stays enabled while a message is on its way, so it keeps
        // the focus.
        out.element(
            ui::input("msg")
                .parent("compose")
                .bind("f.msg")
                .placeholder(cel("'Message #' + s._room_name"))
                .on_submit(Handler::action("send").indicator("_busy")),
        );
        out.element(submit("send", "compose", "Send", "primary", Handler::action("send")));

        self.draw_profile_dialog(out, &me);
        self.draw_invite_dialog(out);
    }

    fn draw_profile_dialog(&self, out: &mut Outbox, me: &str) {
        let close = Handler::patch(json!({"_dlg": ""}));
        out.element(
            ui::dialog("profile_dlg")
                .open(cel("s._dlg == 'profile'"))
                .title("Your profile")
                .on_dismiss(close.clone()),
        );
        out.element(
            ui::image("profile_pic")
                .parent("profile_dlg")
                .variant("avatar")
                .src(cel(format!("{me}.pic")))
                .alt(cel(format!("{me}.name"))),
        );
        out.element(
            ui::input("profile_name")
                .parent("profile_dlg")
                .label("Name")
                .bind("f.name"),
        );
        out.element(picture_input("profile_picture", "profile_dlg"));
        out.element(picture_help("profile_picture_help", "profile_dlg"));
        out.element(ui::row("profile_buttons").parent("profile_dlg"));
        out.element(submit(
            "save_profile",
            "profile_buttons",
            "Save",
            "primary",
            Handler::action("save_profile"),
        ));
        out.element(
            ui::button("profile_cancel")
                .parent("profile_buttons")
                .label("Cancel")
                .on_press(close),
        );
    }

    fn draw_invite_dialog(&self, out: &mut Outbox) {
        if !self.hub.config.invite_only {
            return;
        }
        let close = Handler::patch(json!({"_dlg": ""}));
        out.element(
            ui::dialog("invite_dlg")
                .open(cel("s._dlg == 'invite'"))
                .title("Invite people")
                .on_dismiss(close.clone()),
        );
        out.element(
            ui::text("invite_intro")
                .parent("invite_dlg")
                .text("Make an invite and share the code. Whoever types it when they join gets in."),
        );
        out.element(
            ui::select("invite_uses")
                .parent("invite_dlg")
                .label("Good for")
                .variant("segmented")
                .bind("f.uses")
                .options(json!([
                    {"value": "1", "label": "1 person"},
                    {"value": "10", "label": "10 people"},
                    {"value": "0", "label": "Anyone"},
                ])),
        );
        out.element(
            ui::text("invite_code")
                .parent("invite_dlg")
                .variant("code")
                .visible(cel("s._invite != ''"))
                .text(cel("s._invite")),
        );
        out.element(ui::row("invite_buttons").parent("invite_dlg"));
        out.element(submit(
            "make_invite",
            "invite_buttons",
            "Make an invite",
            "primary",
            Handler::action("make_invite"),
        ));
        out.element(
            ui::button("invite_close")
                .parent("invite_buttons")
                .label("Close")
                .on_press(close),
        );
    }

    fn open(&mut self, out: &mut Outbox, id: RoomId) {
        let Some(room) = self.hub.store.room(id) else {
            return self.flash(out, "failure", "That room is gone.");
        };
        if self.room != Some(id) {
            self.room = Some(id);
            self.from = self.hub.store.start(id, None, PAGE);
            out.signal(json!({"f": {"msg": ""}}));
        }
        self.put(out, json!({"_room": id.0, "_room_name": room.name}));
        self.sync_messages(out);
    }

    fn older(&mut self, out: &mut Outbox) {
        let Some(room) = self.room else {
            return;
        };
        if self.from.0 > 0 {
            self.from = self.hub.store.start(room, Some(self.from), PAGE);
        }
        self.sync_messages(out);
    }

    fn send(&mut self, out: &mut Outbox, text: &str) {
        let Some(room) = self.room else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if text.chars().count() > MAX_MESSAGE {
            return self.flash(out, "failure", &format!("At most {MAX_MESSAGE} characters."));
        }
        if self.hub.store.post(room, &self.key, text, now()).is_none() {
            return self.flash(out, "failure", "That room is gone.");
        }
        out.signal(json!({"f": {"msg": ""}}));
        self.sync_messages(out);
        self.hub.emit(Event::Message { room });
    }

    fn create_room(&mut self, out: &mut Outbox, name: &str) {
        let name = name.trim_start_matches('#').trim();
        if name.is_empty() {
            return self.flash(out, "failure", "Name the room first.");
        }
        if name.chars().count() > MAX_ROOM || name.chars().any(char::is_control) {
            return self.flash(
                out,
                "failure",
                &format!("A room name is up to {MAX_ROOM} characters on one line."),
            );
        }
        match self.hub.store.add_room(name, now()) {
            Ok(id) => {
                out.signal(json!({"f": {"room": ""}}));
                self.sync_rooms(out);
                self.hub.emit(Event::Rooms);
                self.open(out, id);
            }
            Err(RoomError::Taken) => self.flash(out, "failure", "There is a room by that name already."),
        }
    }

    // You.

    fn edit_profile(&mut self, out: &mut Outbox) {
        let Some(m) = self.hub.store.member(&self.key) else {
            return;
        };
        out.signal(json!({"f": {"name": m.name, "picture": m.picture}, "_dlg": "profile"}));
    }

    fn save_profile(&mut self, out: &mut Outbox, name: &str, picture: &str) {
        if let Err(why) = check_name(name).and_then(|_| check_picture(picture)) {
            return self.flash(out, "failure", &why);
        }
        self.hub.store.save_profile(&self.key, name, picture);
        out.signal(json!({"_dlg": ""}));
        self.sync_who(out);
        self.hub.emit(Event::Members);
    }

    fn make_invite(&mut self, out: &mut Outbox, uses: &str) {
        if !self.hub.config.invite_only {
            return;
        }
        let uses = match uses.parse::<u32>() {
            Ok(0) => None,
            Ok(n) => Some(n.min(1000)),
            Err(_) => Some(1),
        };
        let code = self.hub.store.add_invite(Some(&self.key), uses, now());
        self.put(out, json!({"_invite": code}));
    }
}

/// Brings the rows of `list` on screen to `want`, in order, with the fewest
/// messages. `items` is the list's rows as the renderer has them.
fn reconcile(out: &mut Outbox, items: &mut Vec<Tree>, list: &str, want: Vec<Tree>) {
    out.extend(vtree::diff_into(Some(list), items, &want));
    *items = want;
}

/// The merge patch that turns `old` into `new`, if they differ.
fn diff(old: &Value, new: &Value) -> Option<Value> {
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let mut patch = Map::new();
            for (k, v) in n {
                match o.get(k) {
                    Some(ov) => {
                        if let Some(d) = diff(ov, v) {
                            patch.insert(k.clone(), d);
                        }
                    }
                    None => {
                        patch.insert(k.clone(), v.clone());
                    }
                }
            }
            for k in o.keys().filter(|k| !n.contains_key(*k)) {
                patch.insert(k.clone(), Value::Null);
            }
            (!patch.is_empty()).then_some(Value::Object(patch))
        }
        _ if old == new => None,
        _ => Some(new.clone()),
    }
}

// The rows of the lists.

/// The open room's button is the highlighted one.
fn room_item(r: &Room) -> Tree {
    let id = r.id.0;
    vtree::button(format!("r{id}"))
        .label(format!("# {}", r.name))
        .variant(cel(format!("s._room == {id} ? 'primary' : 'plain'")))
        .on_press(Handler::action("open").payload(id))
}

/// A message never changes; its author's name and picture are signals.
fn message_item(m: &Message) -> Tree {
    let row = format!("m{}", m.id.0);
    let who = format!("s._who.{}", who_key(&m.author));
    vtree::row(&row).align("start").children([
        vtree::image(format!("{row}_pic"))
            .variant("avatar")
            .src(cel(format!("{who}.pic")))
            .alt(cel(format!("{who}.name"))),
        vtree::column(format!("{row}_c")).gap("none").children([
            vtree::row(format!("{row}_h")).children([
                vtree::text(format!("{row}_who")).variant("h3").text(cel(format!("{who}.name"))),
                vtree::text(format!("{row}_at")).variant("caption").text(when(m.at)),
            ]),
            vtree::text(format!("{row}_text")).text(m.text.as_str()),
        ]),
    ])
}

/// Where a member is in `_who`: their key is hex, so it makes a field name.
fn who_key(key: &MemberKey) -> String {
    format!("k{}", key.0)
}

fn picture_of(m: &Member) -> String {
    if m.picture.is_empty() {
        default_picture(&m.key.0)
    } else {
        m.picture.clone()
    }
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Pick a name first.".into());
    }
    if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("A name is up to {MAX_NAME} characters on one line."));
    }
    Ok(())
}

fn check_picture(picture: &str) -> Result<(), String> {
    let ok = picture.is_empty()
        || picture.starts_with("data:image/") && picture.len() <= MAX_DATA_PICTURE
        || (picture.starts_with("https://") || picture.starts_with("http://"))
            && picture.len() <= MAX_LINK_PICTURE
            && !picture.contains(char::is_whitespace);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "A picture is a data:image/… URI of up to {} KB, or a link.",
            MAX_DATA_PICTURE / 1024
        ))
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The time of day for today's messages, with the date for older ones.
fn when(ts: i64) -> String {
    let t = Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now);
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn flash() -> Element {
    ui::notice("flash")
        .parent("main")
        .visible(cel("s._flash.text != ''"))
        .variant(cel("s._flash.kind"))
        .text(cel("s._flash.text"))
}

fn picture_input(id: &str, parent: &str) -> Element {
    ui::input(id)
        .parent(parent)
        .label("Picture (optional)")
        .bind("f.picture")
        .placeholder("data:image/… or https://…")
}

fn picture_help(id: &str, parent: &str) -> Element {
    ui::text(id).parent(parent).variant("caption").text(
        "Leave it empty for a square in a color of your own. \
         hyperb shows data: images; for a link it shows your name.",
    )
}

/// A button that asks the server to change something: disabled from the
/// press until the reply, which always resets `_busy`.
fn submit(id: &str, parent: &str, label: &str, variant: &str, h: Handler) -> Element {
    ui::button(id)
        .parent(parent)
        .label(label)
        .variant(variant)
        .disabled(cel("s._busy"))
        .on_press(h.indicator("_busy"))
}

#[cfg(test)]
mod tests;
