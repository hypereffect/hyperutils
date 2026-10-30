//! One caller's session: the screens of the BBS, drawn with hypereffect
//! components the way hypereffect's server guide asks: build once, then
//! change as little as possible.
//!
//! The frame (title, status badges, feedback notice, footer, page dialog) is
//! sent once, at hello. A screen is drawn into `body` when the caller goes
//! there, and from then on [`Bbs::sync`] keeps it current with the fewest
//! messages: what changes (counts, NEW marks, who is doing what) lives in
//! signals that expressions read, and the screen's one list of things
//! (topics, posts, chat lines…) gains, loses and reorders rows one at a time
//! (see [`reconcile`]). Nothing is redrawn to apply a change. `sync` works
//! out everything from the database, so it is idempotent: a session applies
//! its own changes at once, in the reply to the action, and the echo of the
//! same change from the hub finds nothing left to do.
//!
//! Signals:
//! - `f`: form fields, bound to inputs. The only signals the server reads,
//!   so the only ones that are not local.
//! - `_busy`: the indicator of every button that asks the server to change
//!   something; every reply resets it.
//! - `_page`: the page dialog, which its own handlers close.
//! - everything else (`_me`, `_mail`, `_unread`, `_v`…) is display state that
//!   only the server writes, through [`Bbs::put`], which sends only what
//!   changed. `_v` belongs to the screen and goes with it.
//!
//! User text goes into literal attributes or signals, never into CEL.

use crate::db::{Board, Bulletin, Db, Letter, Oneliner, Post, Topic, User};
use crate::hub::{ChatKind, ChatLine, Event, Hub, Node, NodeGuard};
use chrono::{Local, TimeZone};
use hypereffect::signals::merge_patch;
use hypereffect::{cel, json, ui, Action, Element, Handler, Map, Metadata, Outbox, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

const MAX_HANDLE: usize = 20;
const MAX_LINE: usize = 80;
const MAX_CHAT: usize = 300;
const MAX_BODY: usize = 8000;
/// Chat lines kept on screen.
const CHAT_SHOWN: usize = 100;
/// One-liners on the main menu.
const WALL_ON_MENU: usize = 5;
/// Handles nobody can take.
const RESERVED: &[&str] = &["sysop", "all", "everyone", "guest", "<unknown>"];

/// Where the caller is. Navigation buttons send one of these as the payload
/// of a `go` action, so a screen is also something a caller can ask for:
/// [`Bbs::go`] checks it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "screen", rename_all = "snake_case")]
pub enum Screen {
    /// The banner, then the new user application or the welcome back.
    Login,
    Menu,
    Boards,
    Board {
        id: u64,
    },
    Topic {
        id: u64,
    },
    NewTopic {
        board: u64,
    },
    Chat,
    Oneliners,
    Who,
    Users,
    Callers,
    Mail,
    Letter {
        id: u64,
    },
    Compose {
        #[serde(default)]
        to: String,
        #[serde(default)]
        subject: String,
    },
    Bulletins,
    Profile,
    Sysop,
    Goodbye,
}

impl Screen {
    /// What who's online says about a caller here.
    fn doing(&self) -> &'static str {
        match self {
            Screen::Login => "Logging in",
            Screen::Menu => "Main menu",
            Screen::Boards | Screen::Board { .. } | Screen::Topic { .. } => "Reading messages",
            Screen::NewTopic { .. } => "Writing a post",
            Screen::Chat => "In teleconference",
            Screen::Oneliners => "At the one-liner wall",
            Screen::Who => "Who's online",
            Screen::Users => "User list",
            Screen::Callers => "Last callers",
            Screen::Mail | Screen::Letter { .. } => "Reading mail",
            Screen::Compose { .. } => "Writing mail",
            Screen::Bulletins => "Bulletins",
            Screen::Profile => "Profile",
            Screen::Sysop => "Sysop console",
            Screen::Goodbye => "Hanging up",
        }
    }
}

pub struct Bbs {
    hub: Arc<Hub>,
    /// The caller's iroh endpoint key, in hex.
    key: String,
    node: Option<NodeGuard>,
    screen: Screen,
    /// When the session started (after the hello).
    since: Option<i64>,
    registered: bool,
    hangup: bool,
    welcome: Welcome,
    /// The rows of the screen's list, as the renderer has them.
    items: Vec<String>,
    /// Whether the screen says its subject is gone.
    gone: bool,
    /// The display signals (see the module documentation) as the renderer
    /// has them.
    shown: Value,
}

/// What the welcome screen tells about this call.
#[derive(Default)]
struct Welcome {
    /// The caller number.
    number: u64,
    /// When the caller last called before.
    previous: i64,
    /// Who called before them.
    last_caller: Option<String>,
}

impl Bbs {
    pub fn new(hub: Arc<Hub>, key: String) -> Self {
        Bbs {
            hub,
            key,
            node: None,
            screen: Screen::Login,
            since: None,
            registered: false,
            hangup: false,
            welcome: Welcome::default(),
            items: Vec::new(),
            gone: false,
            shown: Value::Null,
        }
    }

    /// Whether the session is over; the transport closes it once the last
    /// screen is sent.
    pub fn hung_up(&self) -> bool {
        self.hangup
    }

    pub fn node(&self) -> Option<u32> {
        self.node.as_ref().map(|n| n.n)
    }

    pub fn hello(&mut self, out: &mut Outbox, _params: &BTreeMap<String, String>) {
        let now = now();
        self.since = Some(now);
        out.metadata(
            Metadata::new()
                .name(&self.hub.config.name)
                .description("A bulletin board system"),
        );
        // The signals the renderer writes too go out directly; `put` keeps
        // track of the others.
        out.signal(json!({
            "f": {},
            "_busy": false,
            "_page": {"open": false, "from": ""},
        }));
        self.put(
            out,
            json!({
                "_flash": {"kind": "info", "text": ""},
                "_me": "",
                "_node": 0,
                "_nodes": self.hub.config.nodes,
                "_clock": "00:00:00",
                "_now": hm(now),
            }),
        );
        self.sync_globals(out);
        self.frame(out);
        let Some(node) = self.hub.take_node(&self.key, now) else {
            self.refuse(
                out,
                "All lines are busy",
                &format!(
                    "All {} nodes are in use. Call back later!",
                    self.hub.config.nodes
                ),
            );
            return;
        };
        self.put(out, json!({"_node": node.n}));
        self.node = Some(node);
        let user = self.hub.store.get().user(&self.key).cloned();
        match user {
            Some(u) if u.banned => {
                self.refuse(out, "Access denied", "This key is banned from the board.");
            }
            Some(u) => {
                self.registered = true;
                println!("node {}: {} logged in", self.node().unwrap_or(0), u.handle);
                self.login(out);
            }
            None => {
                println!("node {}: new caller {}", self.node().unwrap_or(0), self.key);
                self.show(out, Screen::Login);
            }
        }
    }

    pub fn action(&mut self, out: &mut Outbox, a: &Action) {
        if self.since.is_none() || self.hangup {
            return;
        }
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
        if !self.registered {
            match a.name.as_str() {
                "register" => self.register(
                    out,
                    &field("handle"),
                    &field("location"),
                    &field("tagline"),
                ),
                "hangup" => self.show(out, Screen::Goodbye),
                _ => {}
            }
            return;
        }
        let id = || a.payload_as::<u64>();
        match a.name.as_str() {
            "go" => {
                if let Some(s) = a.payload_as::<Screen>() {
                    self.go(out, s);
                }
            }
            "hangup" => self.show(out, Screen::Goodbye),
            "post_topic" => {
                if let Some(board) = id() {
                    self.post_topic(out, board, &field("subject"), &field("body"));
                }
            }
            "reply" => {
                if let Some(topic) = id() {
                    self.reply(out, topic, &field("body"));
                }
            }
            "delete_post" => {
                if let Some(post) = id() {
                    self.delete_post(out, post);
                }
            }
            "chat" => self.say(out, &field("chat")),
            "oneliner" => self.oneliner(out, &field("oneliner")),
            "send_mail" => self.send_mail(out, &field("to"), &field("subject"), &field("body")),
            "delete_mail" => {
                if let Some(letter) = id() {
                    self.delete_mail(out, letter);
                }
            }
            "page" => {
                if let Some(n) = a.payload_as::<u32>() {
                    self.page(out, n);
                }
            }
            "save_profile" => self.save_profile(out, &field("location"), &field("tagline")),
            // The rest is for sysops only.
            _ if !self.hub.is_sysop(&self.key) => {}
            "post_bulletin" => self.post_bulletin(out, &field("title"), &field("body")),
            "delete_bulletin" => {
                if let Some(b) = id() {
                    self.hub.store.edit(|db| db.bulletins.retain(|x| x.id != b));
                    self.changed(out);
                }
            }
            "new_board" => self.new_board(out, &field("name"), &field("desc")),
            "delete_board" => {
                if let Some(b) = id() {
                    self.hub.store.edit(|db| db.delete_board(b));
                    self.changed(out);
                    self.flash(out, "info", "Board deleted.");
                }
            }
            "ban" | "unban" => {
                if let Some(key) = a.payload_as::<String>() {
                    self.ban(out, &key, a.name == "ban");
                }
            }
            _ => {}
        }
    }

    /// What the other sessions did.
    pub fn event(&mut self, out: &mut Outbox, e: &Event) {
        if self.since.is_none() || self.hangup {
            return;
        }
        match e {
            Event::Changed => self.sync(out),
            Event::Nodes => match self.screen {
                Screen::Who | Screen::Chat => self.sync(out),
                _ => self.sync_globals(out),
            },
            Event::Chat if self.screen == Screen::Chat => self.sync(out),
            Event::Mail { to } if *to == self.key => {
                self.flash(out, "info", "New mail has arrived!");
                self.sync(out);
            }
            Event::Page { to, from } if Some(*to) == self.node() && self.registered => {
                out.signal(json!({"_page": {"open": true, "from": from}}));
            }
            Event::Kick { key } if *key == self.key => {
                self.refuse(out, "Access denied", "The sysop has banned this key.")
            }
            _ => {}
        }
    }

    /// Once a second.
    pub fn tick(&mut self, out: &mut Outbox) {
        let Some(since) = self.since else {
            return;
        };
        let now = now();
        let s = (now - since).max(0);
        self.put(
            out,
            json!({
                "_clock": format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
                "_now": hm(now),
            }),
        );
    }

    /// Catches up after this session missed events.
    pub fn refresh(&mut self, out: &mut Outbox) {
        if self.since.is_some() && !self.hangup {
            self.sync(out);
        }
    }

    // Signals.

    /// Writes display signals, sending only what changes. Never for signals
    /// the renderer also writes (`f`, `_busy`, `_page`): `shown` would not
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

    /// The signals of the frame and the menu, current on every screen.
    fn sync_globals(&mut self, out: &mut Outbox) {
        let hub = self.hub.clone();
        let (unread, total, mail) = {
            let db = hub.store.get();
            let mut unread = Map::new();
            let mut total = 0;
            for b in &db.boards {
                let n = if self.registered { db.unread_in(&self.key, b.id) } else { 0 };
                total += n;
                unread.insert(format!("b{}", b.id), n.into());
            }
            (unread, total, db.unread_mail(&self.key))
        };
        let (online, chatters) = {
            let nodes = hub.nodes.lock().unwrap();
            let chatters = nodes.values().filter(|n| in_chat(n)).count();
            (nodes.len(), chatters)
        };
        self.put(
            out,
            json!({
                "_unread": unread,
                "_unread_total": total,
                "_mail": mail,
                "_online": online,
                "_chatters": chatters,
            }),
        );
    }

    /// Brings the screen up to date with the database, the nodes and the
    /// teleconference.
    fn sync(&mut self, out: &mut Outbox) {
        self.sync_globals(out);
        match self.screen.clone() {
            Screen::Menu => self.sync_wall(out, "mwall", WALL_ON_MENU, false),
            Screen::Oneliners => self.sync_wall(out, "wall", usize::MAX, true),
            Screen::Boards => self.sync_boards(out),
            Screen::Board { id } => self.sync_board(out, id),
            Screen::Topic { id } => self.sync_topic(out, id),
            Screen::NewTopic { board } => {
                let gone = self.hub.store.get().board(board).is_none();
                if gone {
                    self.lost(out);
                }
            }
            Screen::Chat => self.sync_chat(out),
            Screen::Who => self.sync_who(out),
            Screen::Users => self.sync_users(out),
            Screen::Mail => self.sync_mail(out),
            Screen::Bulletins => self.sync_bulletins(out),
            Screen::Sysop => self.sync_sysop(out),
            _ => {}
        }
    }

    /// Applies a change this session made, and tells the others.
    fn changed(&mut self, out: &mut Outbox) {
        self.sync(out);
        self.hub.emit(Event::Changed);
    }

    /// The subject of the screen went away: say so, once.
    fn lost(&mut self, out: &mut Outbox) {
        if !self.gone {
            self.render(out);
        }
    }

    // Frame and navigation.

    fn frame(&self, out: &mut Outbox) {
        out.element(ui::column("main").gap("small"));
        out.element(
            ui::text("title")
                .parent("main")
                .variant("h1")
                .text(self.hub.config.name.as_str()),
        );
        out.element(ui::row("status").parent("main").wrap(true));
        out.element(
            ui::badge("me")
                .parent("status")
                .variant("success")
                .visible(cel("s._me != ''"))
                .text(cel("s._me")),
        );
        out.element(
            ui::badge("nodebadge")
                .parent("status")
                .visible(cel("s._node > 0"))
                .text(cel("'Node ' + string(s._node)")),
        );
        out.element(
            ui::badge("mailbadge")
                .parent("status")
                .variant("info")
                .visible(cel("s._mail > 0"))
                .text(cel(
                    "string(s._mail) + (s._mail == 1 ? ' new letter' : ' new letters')",
                )),
        );
        out.element(
            ui::text("clock")
                .parent("status")
                .variant("caption")
                .text(cel("'On for ' + s._clock")),
        );
        out.element(ui::divider("top").parent("main"));
        out.element(
            ui::notice("flash")
                .parent("main")
                .visible(cel("s._flash.text != ''"))
                .variant(cel("s._flash.kind"))
                .text(cel("s._flash.text")),
        );
        out.element(ui::column("body").parent("main"));
        out.element(ui::divider("bottom").parent("main"));
        out.element(
            ui::text("footer")
                .parent("main")
                .variant("caption")
                .text(cel(
                    "s._now + ' · ' + string(s._online) + ' of ' + string(s._nodes) + ' nodes in use'",
                )),
        );
        // Opened by a page from another node (see `event`); closing it is
        // the caller's business, so it needs no round trip.
        let close = json!({"_page": {"open": false}});
        out.element(
            ui::dialog("pagedlg")
                .open(cel("s._page.open"))
                .title(cel("s._page.from + ' is paging you'"))
                .on_dismiss(Handler::patch(close.clone())),
        );
        out.element(
            ui::text("page_text")
                .parent("pagedlg")
                .text(cel("s._page.from + ' would like to chat in the teleconference.'")),
        );
        out.element(ui::row("page_buttons").parent("pagedlg"));
        out.element(
            ui::button("page_yes")
                .parent("page_buttons")
                .label("Join the teleconference")
                .variant("primary")
                .on_press(go(Screen::Chat).with_patch(close.clone())),
        );
        out.element(
            ui::button("page_no")
                .parent("page_buttons")
                .label("Not now")
                .on_press(Handler::patch(close)),
        );
    }

    /// A navigation request from the caller.
    fn go(&mut self, out: &mut Outbox, s: Screen) {
        let allowed = match s {
            Screen::Login | Screen::Goodbye => false,
            Screen::Sysop => self.hub.is_sysop(&self.key),
            _ => true,
        };
        if allowed && s != self.screen {
            self.show(out, s);
        }
    }

    fn show(&mut self, out: &mut Outbox, screen: Screen) {
        let was_chat = self.screen == Screen::Chat;
        self.screen = screen;
        let in_chat = self.screen == Screen::Chat;
        if let Some(n) = self.node() {
            let doing = self.screen.doing();
            self.hub.update_node(n, |node| node.doing = doing);
        }
        self.render(out);
        if self.screen == Screen::Goodbye {
            self.hangup = true;
        }
        let me = self.handle();
        if was_chat && !in_chat {
            self.hub
                .chat(ChatKind::System, &me, "has left the teleconference", now());
        } else if in_chat && !was_chat {
            self.hub
                .chat(ChatKind::System, &me, "has entered the teleconference", now());
            self.sync(out);
        }
    }

    /// The defaults of the screen's form fields.
    fn form(&self) -> Value {
        match &self.screen {
            Screen::Login => json!({"handle": "", "location": "", "tagline": ""}),
            Screen::NewTopic { .. } => json!({"subject": "", "body": ""}),
            Screen::Topic { .. } => json!({"body": ""}),
            Screen::Chat => json!({"chat": ""}),
            Screen::Oneliners => json!({"oneliner": ""}),
            Screen::Compose { to, subject } => json!({"to": to, "subject": subject, "body": ""}),
            Screen::Bulletins => json!({"title": "", "body": ""}),
            Screen::Profile => {
                let db = self.hub.store.get();
                let u = db.user(&self.key);
                json!({
                    "location": u.map_or("", |u| &u.location),
                    "tagline": u.map_or("", |u| &u.tagline),
                })
            }
            Screen::Sysop => json!({"name": "", "desc": ""}),
            _ => json!({}),
        }
    }

    /// Draws the screen: the only place `body` is replaced, because the
    /// caller went somewhere else.
    fn render(&mut self, out: &mut Outbox) {
        self.clear(out);
        out.signal(json!({"f": null}));
        out.signal(json!({"f": self.form()}));
        self.put(out, json!({"_flash": {"kind": "info", "text": ""}}));
        match self.screen.clone() {
            Screen::Login if self.registered => self.draw_welcome(out),
            Screen::Login => self.draw_application(out),
            Screen::Menu => self.draw_menu(out),
            Screen::Boards => self.draw_boards(out),
            Screen::Board { id } => self.draw_board(out, id),
            Screen::Topic { id } => self.draw_topic(out, id),
            Screen::NewTopic { board } => self.draw_new_topic(out, board),
            Screen::Chat => self.draw_chat(out),
            Screen::Oneliners => self.draw_oneliners(out),
            Screen::Who => self.draw_who(out),
            Screen::Users => self.draw_users(out),
            Screen::Callers => self.draw_callers(out),
            Screen::Mail => self.draw_mail(out),
            Screen::Letter { id } => self.draw_letter(out, id),
            Screen::Compose { .. } => self.draw_compose(out),
            Screen::Bulletins => self.draw_bulletins(out),
            Screen::Profile => self.draw_profile(out),
            Screen::Sysop => self.draw_sysop(out),
            Screen::Goodbye => self.draw_goodbye(out),
        }
        self.sync(out);
    }

    /// Empties `body`, and forgets the screen's signals with it.
    fn clear(&mut self, out: &mut Outbox) {
        out.remove("body");
        out.element(ui::column("body").parent("main").before("bottom"));
        self.items.clear();
        self.gone = false;
        self.put(out, json!({"_v": null}));
    }

    /// Instead of a screen whose subject was deleted.
    fn draw_gone(&mut self, out: &mut Outbox, text: &str, to: Screen) {
        self.gone = true;
        out.element(ui::notice("gone").parent("body").variant("warning").text(text));
        back(out, to, "Back");
    }

    /// Shows why the caller cannot stay, and hangs up.
    fn refuse(&mut self, out: &mut Outbox, title: &str, text: &str) {
        self.clear(out);
        out.element(banner(&self.hub.config.banner));
        out.element(
            ui::notice("refused")
                .parent("body")
                .variant("failure")
                .title(title)
                .text(text),
        );
        out.element(code("nocarrier", "body", "NO CARRIER"));
        self.hangup = true;
    }

    fn handle(&self) -> String {
        self.hub.store.get().handle(&self.key)
    }

    // Logging in.

    fn draw_application(&mut self, out: &mut Outbox) {
        let name = &self.hub.config.name;
        out.element(banner(&self.hub.config.banner));
        out.element(
            ui::card("apply")
                .parent("body")
                .title("New user application"),
        );
        out.element(body(
            "apply_intro",
            "apply",
            &format!(
                "Welcome to {name}! Your hyperb key is your account, so nobody \
                 else can log in as you and there is no password to remember. \
                 Pick a handle to be known by."
            ),
        ));
        out.element(
            ui::input("handle")
                .parent("apply")
                .label("Handle")
                .bind("f.handle")
                .placeholder("2 to 20 letters, digits, spaces or _-."),
        );
        out.element(
            ui::input("location")
                .parent("apply")
                .label("Calling from")
                .bind("f.location")
                .placeholder("City, State"),
        );
        out.element(
            ui::input("tagline")
                .parent("apply")
                .label("Tagline (shown under your posts)")
                .bind("f.tagline"),
        );
        out.element(ui::row("apply_buttons").parent("apply"));
        out.element(submit(
            "register",
            "apply_buttons",
            &format!("Join {name}"),
            "primary",
            Handler::action("register"),
        ));
        out.element(
            ui::button("apply_hangup")
                .parent("apply_buttons")
                .label("Hang up")
                .variant("plain")
                .on_press(Handler::action("hangup")),
        );
    }

    fn register(&mut self, out: &mut Outbox, handle: &str, location: &str, tagline: &str) {
        let handle = handle.split_whitespace().collect::<Vec<_>>().join(" ");
        let problem = if handle.chars().count() < 2 || handle.chars().count() > MAX_HANDLE {
            Some(format!("A handle has 2 to {MAX_HANDLE} characters."))
        } else if !handle.chars().next().is_some_and(char::is_alphanumeric)
            || !handle
                .chars()
                .all(|c| c.is_alphanumeric() || " _-.".contains(c))
        {
            Some("A handle starts with a letter or digit and has only letters, digits, spaces and _-.".into())
        } else if location.chars().count() > MAX_LINE || tagline.chars().count() > MAX_LINE {
            Some(format!("Location and tagline have at most {MAX_LINE} characters."))
        } else if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&handle)) {
            Some(format!("{handle} is reserved."))
        } else {
            None
        };
        if let Some(p) = problem {
            return self.flash(out, "failure", &p);
        }
        let now = now();
        let taken = self.hub.store.edit(|db| {
            if db.find_handle(&handle).is_some() {
                return true;
            }
            // The first caller to register runs the place.
            let sysop = db.users.is_empty();
            db.users.insert(
                self.key.clone(),
                User {
                    handle: handle.clone(),
                    location: location.into(),
                    tagline: tagline.into(),
                    sysop,
                    joined: now,
                    ..Default::default()
                },
            );
            false
        });
        if taken {
            return self.flash(out, "failure", &format!("{handle} is taken, pick another."));
        }
        println!("node {}: {} registered as {handle}", self.node().unwrap_or(0), self.key);
        self.registered = true;
        self.login(out);
    }

    /// Logs the call and shows the welcome screen.
    fn login(&mut self, out: &mut Outbox) {
        let node = self.node().unwrap_or(0);
        let handle = self.handle();
        self.hub.update_node(node, |n| n.handle = handle.clone());
        let (previous, last_caller) = {
            let db = self.hub.store.get();
            let previous = db.user(&self.key).map_or(0, |u| u.last_on);
            (previous, db.callers.last().map(|c| caller_line(&db, &c.key)))
        };
        let number = self.hub.store.edit(|db| db.log_call(&self.key, node, now()));
        self.welcome = Welcome {
            number,
            previous,
            last_caller,
        };
        self.put(out, json!({"_me": handle}));
        self.show(out, Screen::Login);
        // The user list shows calls and the last time on.
        self.hub.emit(Event::Changed);
    }

    fn draw_welcome(&mut self, out: &mut Outbox) {
        let node = self.node().unwrap_or(0);
        let Welcome {
            number,
            previous,
            ref last_caller,
        } = self.welcome;
        let db = self.hub.store.get();
        let Some(u) = db.user(&self.key) else {
            return;
        };
        out.element(banner(&self.hub.config.banner));
        out.element(
            ui::card("welcome")
                .parent("body")
                .title(if u.calls > 1 {
                    format!("Welcome back, {}!", u.handle)
                } else {
                    format!("Welcome, {}!", u.handle)
                })
                .gap("small"),
        );
        let mut lines = vec![format!(
            "You are caller #{number}, on node {node}. This is your call #{}.",
            u.calls
        )];
        if u.calls > 1 {
            lines.push(format!("Your last call: {}.", when(previous)));
        }
        if let Some(last) = last_caller {
            lines.push(format!("Last caller: {last}."));
        }
        let unread: usize = db.boards.iter().map(|b| db.unread_in(&self.key, b.id)).sum();
        if unread > 0 {
            lines.push(format!("{unread} topics have new posts for you."));
        }
        if u.sysop {
            lines.push("You are a sysop here.".into());
        }
        for (i, line) in lines.iter().enumerate() {
            out.element(body(&format!("welcome{i}"), "welcome", line));
        }
        let mail = db.unread_mail(&self.key);
        if mail > 0 {
            out.element(
                ui::notice("welcome_mail")
                    .parent("body")
                    .title("You have mail")
                    .text(format!("{mail} unread letters are waiting for you.")),
            );
        }
        if let Some(b) = db.bulletins.last() {
            out.element(
                ui::notice("welcome_bulletin")
                    .parent("body")
                    .variant("warning")
                    .title(format!("Bulletin: {}", b.title))
                    .text(b.body.as_str()),
            );
        }
        out.element(
            ui::button("continue")
                .parent("body")
                .label("Continue to the main menu")
                .variant("primary")
                .on_press(go(Screen::Menu)),
        );
    }

    // The main menu.

    fn draw_menu(&mut self, out: &mut Outbox) {
        let sysop = self.hub.is_sysop(&self.key);
        heading(out, "menu_h", "body", "Main Menu");
        out.element(ui::row("menu").parent("body").wrap(true).align("start"));
        let group = |out: &mut Outbox, id: &str, title: &str| {
            out.element(ui::card(id).parent("menu").title(title).gap("small"));
        };
        group(out, "m_msg", "Messages");
        menu_item(out, "m_boards", "m_msg", "Message boards", Screen::Boards, Some("s._unread_total"));
        menu_item(out, "m_mail", "m_msg", "E-mail", Screen::Mail, Some("s._mail"));
        menu_item(out, "m_bull", "m_msg", "Bulletins", Screen::Bulletins, None);
        group(out, "m_people", "People");
        menu_item(out, "m_chat", "m_people", "Teleconference", Screen::Chat, Some("s._chatters"));
        menu_item(out, "m_who", "m_people", "Who's online", Screen::Who, None);
        menu_item(out, "m_one", "m_people", "One-liner wall", Screen::Oneliners, None);
        menu_item(out, "m_users", "m_people", "User list", Screen::Users, None);
        menu_item(out, "m_last", "m_people", "Last callers", Screen::Callers, None);
        group(out, "m_you", "You");
        menu_item(out, "m_prof", "m_you", "Your profile", Screen::Profile, None);
        if sysop {
            menu_item(out, "m_sysop", "m_you", "Sysop console", Screen::Sysop, None);
        }
        out.element(
            ui::button("m_bye")
                .parent("m_you")
                .label("Goodbye")
                .variant("failure")
                .on_press(Handler::action("hangup")),
        );
        out.element(
            ui::card("m_wall_card")
                .parent("body")
                .title("Latest one-liners")
                .gap("none"),
        );
        out.element(
            ui::list("mwall")
                .parent("m_wall_card")
                .empty("The wall is clean."),
        );
    }

    /// The one-liners in `list`, newest first.
    fn sync_wall(&mut self, out: &mut Outbox, list: &str, most: usize, dated: bool) {
        let hub = self.hub.clone();
        let db = hub.store.get();
        let want: Vec<_> = db
            .oneliners
            .iter()
            .rev()
            .take(most)
            .map(|o| (format!("o{}", o.id), o))
            .collect();
        reconcile(out, &mut self.items, &want, |o| {
            vec![oneliner_item(&db, o, list, dated)]
        });
    }

    // Message boards.

    fn draw_boards(&mut self, out: &mut Outbox) {
        heading(out, "boards_h", "body", "Message Boards");
        out.element(
            ui::list("boards")
                .parent("body")
                .empty("There are no boards yet."),
        );
        back(out, Screen::Menu, "Main menu");
    }

    fn sync_boards(&mut self, out: &mut Outbox) {
        let hub = self.hub.clone();
        let db = hub.store.get();
        let about: Map<String, Value> = db
            .boards
            .iter()
            .map(|b| {
                let topics = db.topics.iter().filter(|t| t.board == b.id).count();
                let text = format!("{} · {topics} topics", b.description);
                (format!("b{}", b.id), text.into())
            })
            .collect();
        self.put(out, json!({"_v": {"about": about}}));
        let want: Vec<_> = db.boards.iter().map(|b| (format!("b{}", b.id), b)).collect();
        reconcile(out, &mut self.items, &want, |b| board_item(b));
    }

    fn draw_board(&mut self, out: &mut Outbox, id: u64) {
        let Some(board) = self.hub.store.get().board(id).cloned() else {
            return self.draw_gone(out, "This board is gone.", Screen::Boards);
        };
        heading(out, "board_h", "body", &board.name);
        out.element(caption("board_about", "body", &board.description));
        out.element(
            ui::button("new_topic")
                .parent("body")
                .label("Start a new topic")
                .variant("primary")
                .on_press(go(Screen::NewTopic { board: id })),
        );
        out.element(
            ui::list("topics")
                .parent("body")
                .empty("No topics yet. Be the first!"),
        );
        back(out, Screen::Boards, "All boards");
    }

    /// The board's topics, most recently active first: a reply moves its
    /// topic to the top.
    fn sync_board(&mut self, out: &mut Outbox, id: u64) {
        let hub = self.hub.clone();
        let db = hub.store.get();
        if db.board(id).is_none() {
            drop(db);
            return self.lost(out);
        }
        let topics = db.topics_of(id);
        let mut new = Map::new();
        let mut about = Map::new();
        for t in &topics {
            let k = format!("t{}", t.id);
            new.insert(k.clone(), db.is_unread(&self.key, t).into());
            let replies = db.posts_of(t.id).count().saturating_sub(1);
            about.insert(
                k,
                format!(
                    "by {} · {replies} {} · last {}",
                    db.handle(&t.author),
                    if replies == 1 { "reply" } else { "replies" },
                    when(t.last_at)
                )
                .into(),
            );
        }
        self.put(out, json!({"_v": {"new": new, "about": about}}));
        let want: Vec<_> = topics.into_iter().map(|t| (format!("t{}", t.id), t)).collect();
        reconcile(out, &mut self.items, &want, |t| topic_item(t));
    }

    fn draw_new_topic(&mut self, out: &mut Outbox, board: u64) {
        let Some(b) = self.hub.store.get().board(board).cloned() else {
            return self.draw_gone(out, "This board is gone.", Screen::Boards);
        };
        heading(out, "nt_h", "body", &format!("New topic in {}", b.name));
        out.element(
            ui::input("nt_subject")
                .parent("body")
                .label("Subject")
                .bind("f.subject"),
        );
        out.element(
            ui::input("nt_body")
                .parent("body")
                .label("Message")
                .variant("multiline")
                .bind("f.body"),
        );
        out.element(ui::row("nt_buttons").parent("body"));
        out.element(submit(
            "nt_post",
            "nt_buttons",
            "Post it",
            "primary",
            Handler::action("post_topic").payload(board),
        ));
        out.element(nav("nt_cancel", "nt_buttons", "Cancel", Screen::Board { id: board }));
    }

    fn post_topic(&mut self, out: &mut Outbox, board: u64, subject: &str, text: &str) {
        if subject.is_empty() || text.is_empty() {
            return self.flash(out, "failure", "A topic needs a subject and a message.");
        }
        if subject.chars().count() > MAX_LINE || text.chars().count() > MAX_BODY {
            return self.flash(
                out,
                "failure",
                &format!("At most {MAX_LINE} characters of subject and {MAX_BODY} of message."),
            );
        }
        let topic = self.hub.store.edit(|db| {
            db.board(board)?;
            Some(db.add_topic(board, &self.key, subject, text, now()))
        });
        let Some(topic) = topic else {
            return self.flash(out, "failure", "That board is gone.");
        };
        self.show(out, Screen::Topic { id: topic });
        self.hub.emit(Event::Changed);
        self.flash(out, "success", "Your topic is up.");
    }

    fn draw_topic(&mut self, out: &mut Outbox, id: u64) {
        let hub = self.hub.clone();
        let db = hub.store.get();
        let Some(t) = db.topic(id) else {
            drop(db);
            return self.draw_gone(out, "This topic is gone.", Screen::Boards);
        };
        let board = db.board(t.board).map_or("", |b| &b.name);
        heading(out, "topic_h", "body", &t.subject);
        out.element(caption("topic_about", "body", &format!("in {board}")));
        out.element(ui::column("posts").parent("body"));
        out.element(ui::card("reply_card").parent("body").title("Reply"));
        out.element(
            ui::input("reply_body")
                .parent("reply_card")
                .variant("multiline")
                .bind("f.body"),
        );
        out.element(submit(
            "reply",
            "reply_card",
            "Post reply",
            "primary",
            Handler::action("reply").payload(id),
        ));
        back(out, Screen::Board { id: t.board }, &format!("Back to {board}"));
    }

    /// The topic's posts, oldest first. What is on screen counts as read.
    fn sync_topic(&mut self, out: &mut Outbox, id: u64) {
        let sysop = self.hub.is_sysop(&self.key);
        let hub = self.hub.clone();
        let db = hub.store.get();
        let Some(last) = db.topic(id).map(|t| t.last_post) else {
            drop(db);
            return self.lost(out);
        };
        let want: Vec<_> = db.posts_of(id).map(|p| (format!("p{}", p.id), p)).collect();
        let key = &self.key;
        reconcile(out, &mut self.items, &want, |p| {
            post_item(&db, p, sysop || p.author == *key)
        });
        let seen = db.user(key).and_then(|u| u.last_read.get(&id).copied());
        drop(db);
        if seen != Some(last) {
            self.hub.store.edit(|db| {
                if let Some(u) = db.users.get_mut(&self.key) {
                    u.last_read.insert(id, last);
                }
            });
            self.sync_globals(out);
        }
    }

    fn reply(&mut self, out: &mut Outbox, topic: u64, text: &str) {
        if text.is_empty() {
            return self.flash(out, "failure", "Type a reply first.");
        }
        if text.chars().count() > MAX_BODY {
            return self.flash(out, "failure", &format!("At most {MAX_BODY} characters."));
        }
        let posted = self.hub.store.edit(|db| {
            db.topic(topic)?;
            Some(db.add_post(topic, &self.key, text, now()))
        });
        if posted.is_none() {
            return self.flash(out, "failure", "That topic is gone.");
        }
        out.signal(json!({"f": {"body": ""}}));
        self.changed(out);
    }

    fn delete_post(&mut self, out: &mut Outbox, post: u64) {
        let sysop = self.hub.is_sysop(&self.key);
        let done = self.hub.store.edit(|db| {
            let p = db.posts.iter().find(|p| p.id == post)?;
            if !sysop && p.author != self.key {
                return None;
            }
            let board = db.topic(p.topic)?.board;
            let (_, topic_gone) = db.delete_post(post)?;
            Some((board, topic_gone))
        });
        let Some((board, topic_gone)) = done else {
            return;
        };
        if topic_gone {
            self.show(out, Screen::Board { id: board });
            self.hub.emit(Event::Changed);
        } else {
            self.changed(out);
        }
        self.flash(out, "info", "Post deleted.");
    }

    // The teleconference.

    fn draw_chat(&mut self, out: &mut Outbox) {
        heading(out, "chat_h", "body", "Teleconference");
        out.element(caption(
            "chat_help",
            "body",
            "Everyone in here hears everything. Type /me to emote.",
        ));
        out.element(
            ui::text("chat_who")
                .parent("body")
                .variant("caption")
                .text(cel("s._v.here")),
        );
        out.element(ui::row("chat_say").parent("body"));
        // The field stays enabled while a line is on its way, so it keeps
        // the focus.
        out.element(
            ui::input("chat_input")
                .parent("chat_say")
                .bind("f.chat")
                .placeholder("Say something…")
                .on_submit(Handler::action("chat").indicator("_busy")),
        );
        out.element(submit(
            "chat_send",
            "chat_say",
            "Say",
            "primary",
            Handler::action("chat"),
        ));
        out.element(ui::list("chatlog").parent("body").empty("It's quiet in here."));
        back(out, Screen::Menu, "Leave the teleconference");
    }

    /// Who is here, and the log, newest first.
    fn sync_chat(&mut self, out: &mut Outbox) {
        let mut here: Vec<String> = self
            .hub
            .nodes
            .lock()
            .unwrap()
            .values()
            .filter(|n| in_chat(n))
            .map(|n| n.handle.clone())
            .collect();
        here.sort();
        here.dedup();
        self.put(out, json!({"_v": {"here": format!("Here: {}", here.join(", "))}}));
        let history = self.hub.chat_history();
        let want: Vec<_> = history
            .iter()
            .rev()
            .take(CHAT_SHOWN)
            .map(|l| (format!("chat{}", l.n), l))
            .collect();
        reconcile(out, &mut self.items, &want, |l| vec![chat_item(l)]);
    }

    fn say(&mut self, out: &mut Outbox, text: &str) {
        if self.screen != Screen::Chat || text.is_empty() {
            return;
        }
        let text: String = text.chars().take(MAX_CHAT).collect();
        let (kind, text) = match text.strip_prefix("/me ") {
            Some(rest) => (ChatKind::Emote, rest.trim().to_owned()),
            None => (ChatKind::Say, text),
        };
        out.signal(json!({"f": {"chat": ""}}));
        self.hub.chat(kind, &self.handle(), &text, now());
        self.sync(out);
    }

    // People.

    fn draw_oneliners(&mut self, out: &mut Outbox) {
        heading(out, "one_h", "body", "One-liner Wall");
        out.element(caption(
            "one_help",
            "body",
            &format!("Leave your mark for the next caller, in {MAX_LINE} characters or less."),
        ));
        out.element(ui::row("one_say").parent("body"));
        out.element(
            ui::input("one_input")
                .parent("one_say")
                .bind("f.oneliner")
                .on_submit(Handler::action("oneliner").indicator("_busy")),
        );
        out.element(submit(
            "one_send",
            "one_say",
            "Scrawl it",
            "primary",
            Handler::action("oneliner"),
        ));
        out.element(ui::list("wall").parent("body").empty("The wall is clean."));
        back(out, Screen::Menu, "Main menu");
    }

    fn oneliner(&mut self, out: &mut Outbox, text: &str) {
        if text.is_empty() {
            return;
        }
        if text.chars().count() > MAX_LINE {
            return self.flash(out, "failure", &format!("One line, {MAX_LINE} characters at most."));
        }
        self.hub.store.edit(|db| db.add_oneliner(&self.key, text, now()));
        out.signal(json!({"f": {"oneliner": ""}}));
        self.changed(out);
    }

    fn draw_who(&mut self, out: &mut Outbox) {
        heading(out, "who_h", "body", "Who's Online");
        out.element(ui::list("who").parent("body"));
        back(out, Screen::Menu, "Main menu");
    }

    /// A row per node; what changes in it is in `_v.who`.
    fn sync_who(&mut self, out: &mut Outbox) {
        let nodes = self.hub.nodes.lock().unwrap().clone();
        let mut who = Map::new();
        {
            let db = self.hub.store.get();
            for (n, node) in &nodes {
                let name = if node.handle.is_empty() {
                    "A new caller".to_owned()
                } else {
                    match db.user(&node.key).map(|u| &u.location).filter(|l| !l.is_empty()) {
                        Some(from) => format!("{} from {from}", node.handle),
                        None => node.handle.clone(),
                    }
                };
                who.insert(
                    format!("n{n}"),
                    json!({
                        "who": name,
                        "doing": format!("{}, on since {}", node.doing, hm(node.since)),
                        "page": Some(*n) != self.node() && !node.handle.is_empty(),
                    }),
                );
            }
        }
        self.put(out, json!({"_v": {"who": who}}));
        let want: Vec<_> = nodes.keys().map(|n| (format!("n{n}"), *n)).collect();
        reconcile(out, &mut self.items, &want, |n| node_item(*n));
    }

    fn page(&mut self, out: &mut Outbox, to: u32) {
        let known = self
            .hub
            .nodes
            .lock()
            .unwrap()
            .get(&to)
            .is_some_and(|n| !n.handle.is_empty());
        if !known || Some(to) == self.node() {
            return self.flash(out, "failure", "Nobody to page there.");
        }
        self.hub.emit(Event::Page {
            to,
            from: self.handle(),
        });
        self.flash(
            out,
            "success",
            &format!("Paged node {to}. Wait for them in the teleconference!"),
        );
    }

    fn draw_users(&mut self, out: &mut Outbox) {
        out.element(
            ui::text("users_h")
                .parent("body")
                .variant("h2")
                .text(cel("'User List (' + string(s._v.count) + ')'")),
        );
        out.element(ui::list("users").parent("body"));
        back(out, Screen::Menu, "Main menu");
    }

    /// Users by handle; what changes about them is in `_v.users`.
    fn sync_users(&mut self, out: &mut Outbox) {
        let sysop = self.hub.is_sysop(&self.key);
        let hub = self.hub.clone();
        let db = hub.store.get();
        let mut users: Vec<_> = db.users.iter().collect();
        users.sort_by_key(|(_, u)| u.handle.to_lowercase());
        let mut view = Map::new();
        for (key, u) in &users {
            let mut about = Vec::new();
            if !u.location.is_empty() {
                about.push(u.location.clone());
            }
            about.push(format!("{} calls, {} posts", u.calls, u.posts));
            about.push(format!("last on {}", when(u.last_on)));
            view.insert(
                format!("u{key}"),
                json!({
                    "about": about.join(" · "),
                    "banned": u.banned,
                    "sysop": u.sysop || hub.config.sysops.contains(key),
                }),
            );
        }
        self.put(out, json!({"_v": {"count": users.len(), "users": view}}));
        let want: Vec<_> = users
            .into_iter()
            .map(|(key, u)| (format!("u{key}"), (key, u)))
            .collect();
        let me = &self.key;
        reconcile(out, &mut self.items, &want, |(key, u)| {
            user_item(key, u, *key == me, sysop)
        });
    }

    /// Pattern A of the server guide: the two buttons only ask, the reply
    /// carries the confirmed state (through `changed`) with the indicator
    /// reset.
    fn ban(&mut self, out: &mut Outbox, key: &str, ban: bool) {
        if key == self.key {
            return self.flash(out, "failure", "You cannot ban yourself.");
        }
        let handle = self.hub.store.edit(|db| {
            let u = db.users.get_mut(key)?;
            u.banned = ban;
            Some(u.handle.clone())
        });
        let Some(handle) = handle else {
            return;
        };
        if ban {
            self.hub.emit(Event::Kick { key: key.into() });
        }
        self.changed(out);
        self.flash(
            out,
            "info",
            &format!("{handle} is {}.", if ban { "banned" } else { "welcome again" }),
        );
    }

    fn draw_callers(&mut self, out: &mut Outbox) {
        let db = self.hub.store.get();
        heading(out, "callers_h", "body", "Last Callers");
        out.element(ui::list("callers").parent("body").empty("Nobody yet."));
        // A log that does not change while it is on screen.
        for (i, c) in db.callers.iter().rev().take(20).enumerate() {
            out.element(code(
                &format!("caller{i}"),
                "callers",
                &format!("{}  node {:>2}  {}", when(c.at), c.node, caller_line(&db, &c.key)),
            ));
        }
        out.element(caption(
            "callers_total",
            "body",
            &format!("{} calls since the board went up.", db.calls),
        ));
        back(out, Screen::Menu, "Main menu");
    }

    // Mail.

    fn draw_mail(&mut self, out: &mut Outbox) {
        heading(out, "mail_h", "body", "E-mail");
        out.element(
            ui::button("mail_new")
                .parent("body")
                .label("Write a letter")
                .variant("primary")
                .on_press(go(Screen::Compose {
                    to: String::new(),
                    subject: String::new(),
                })),
        );
        out.element(ui::list("inbox").parent("body").empty("Your mailbox is empty."));
        back(out, Screen::Menu, "Main menu");
    }

    fn sync_mail(&mut self, out: &mut Outbox) {
        let hub = self.hub.clone();
        let db = hub.store.get();
        let inbox = db.inbox(&self.key);
        let new: Map<String, Value> = inbox
            .iter()
            .map(|l| (format!("l{}", l.id), (!l.read).into()))
            .collect();
        self.put(out, json!({"_v": {"new": new}}));
        let want: Vec<_> = inbox.into_iter().map(|l| (format!("l{}", l.id), l)).collect();
        reconcile(out, &mut self.items, &want, |l| letter_item(&db, l));
    }

    fn draw_letter(&mut self, out: &mut Outbox, id: u64) {
        let letter = {
            let db = self.hub.store.get();
            db.mail
                .iter()
                .find(|l| l.id == id && l.to == self.key)
                .map(|l| (l.clone(), db.handle(&l.from)))
        };
        let Some((l, from)) = letter else {
            return self.draw_gone(out, "This letter is gone.", Screen::Mail);
        };
        heading(out, "letter_h", "body", &l.subject);
        out.element(caption(
            "letter_from",
            "body",
            &format!("From {from}, {}", when(l.at)),
        ));
        out.element(ui::card("letter").parent("body"));
        out.element(body("letter_body", "letter", &l.body));
        out.element(ui::row("letter_buttons").parent("body"));
        let subject = if l.subject.starts_with("Re: ") {
            l.subject.clone()
        } else {
            format!("Re: {}", l.subject)
        };
        out.element(
            ui::button("letter_reply")
                .parent("letter_buttons")
                .label("Reply")
                .variant("primary")
                .on_press(go(Screen::Compose { to: from, subject })),
        );
        out.element(submit(
            "letter_delete",
            "letter_buttons",
            "Delete",
            "failure",
            Handler::action("delete_mail").payload(id),
        ));
        out.element(nav("letter_back", "letter_buttons", "Back to the mailbox", Screen::Mail));
        if !l.read {
            self.hub.store.edit(|db| {
                if let Some(l) = db.mail.iter_mut().find(|l| l.id == id) {
                    l.read = true;
                }
            });
        }
    }

    fn draw_compose(&mut self, out: &mut Outbox) {
        heading(out, "compose_h", "body", "Write a letter");
        out.element(
            ui::input("compose_to")
                .parent("body")
                .label("To (handle)")
                .bind("f.to"),
        );
        out.element(
            ui::input("compose_subject")
                .parent("body")
                .label("Subject")
                .bind("f.subject"),
        );
        out.element(
            ui::input("compose_body")
                .parent("body")
                .label("Message")
                .variant("multiline")
                .bind("f.body"),
        );
        out.element(ui::row("compose_buttons").parent("body"));
        out.element(submit(
            "compose_send",
            "compose_buttons",
            "Send",
            "primary",
            Handler::action("send_mail"),
        ));
        out.element(nav("compose_cancel", "compose_buttons", "Cancel", Screen::Mail));
    }

    fn send_mail(&mut self, out: &mut Outbox, to: &str, subject: &str, text: &str) {
        let Some(to_key) = self.hub.store.get().find_handle(to).map(str::to_owned) else {
            return self.flash(out, "failure", &format!("Nobody here goes by {to:?}."));
        };
        if subject.is_empty() || text.is_empty() {
            return self.flash(out, "failure", "A letter needs a subject and a message.");
        }
        if subject.chars().count() > MAX_LINE || text.chars().count() > MAX_BODY {
            return self.flash(
                out,
                "failure",
                &format!("At most {MAX_LINE} characters of subject and {MAX_BODY} of message."),
            );
        }
        let handle = self.hub.store.edit(|db| {
            let id = db.next_id();
            db.mail.push(Letter {
                id,
                from: self.key.clone(),
                to: to_key.clone(),
                subject: subject.into(),
                body: text.into(),
                at: now(),
                read: false,
            });
            db.handle(&to_key)
        });
        self.hub.emit(Event::Mail { to: to_key });
        self.show(out, Screen::Mail);
        self.flash(out, "success", &format!("Your letter is on its way to {handle}."));
    }

    fn delete_mail(&mut self, out: &mut Outbox, id: u64) {
        self.hub
            .store
            .edit(|db| db.mail.retain(|l| l.id != id || l.to != self.key));
        self.show(out, Screen::Mail);
        self.flash(out, "info", "Letter deleted.");
    }

    // Bulletins.

    fn draw_bulletins(&mut self, out: &mut Outbox) {
        let sysop = self.hub.is_sysop(&self.key);
        heading(out, "bull_h", "body", "Bulletins");
        out.element(ui::list("bulls").parent("body").empty("No bulletins. All is well."));
        if sysop {
            out.element(ui::card("bull_new").parent("body").title("Post a bulletin"));
            out.element(
                ui::input("bull_title")
                    .parent("bull_new")
                    .label("Title")
                    .bind("f.title"),
            );
            out.element(
                ui::input("bull_body")
                    .parent("bull_new")
                    .label("Text")
                    .variant("multiline")
                    .bind("f.body"),
            );
            out.element(submit(
                "bull_post",
                "bull_new",
                "Post",
                "primary",
                Handler::action("post_bulletin"),
            ));
        }
        back(out, Screen::Menu, "Main menu");
    }

    fn sync_bulletins(&mut self, out: &mut Outbox) {
        let sysop = self.hub.is_sysop(&self.key);
        let hub = self.hub.clone();
        let db = hub.store.get();
        let want: Vec<_> = db
            .bulletins
            .iter()
            .rev()
            .map(|b| (format!("bull{}", b.id), b))
            .collect();
        reconcile(out, &mut self.items, &want, |b| bulletin_item(b, sysop));
    }

    fn post_bulletin(&mut self, out: &mut Outbox, title: &str, text: &str) {
        if title.is_empty() || text.is_empty() {
            return self.flash(out, "failure", "A bulletin needs a title and a text.");
        }
        self.hub.store.edit(|db| {
            let id = db.next_id();
            db.bulletins.push(Bulletin {
                id,
                title: title.chars().take(MAX_LINE).collect(),
                body: text.chars().take(MAX_BODY).collect(),
                at: now(),
            });
        });
        out.signal(json!({"f": {"title": "", "body": ""}}));
        self.changed(out);
    }

    // The caller and the sysop.

    fn draw_profile(&mut self, out: &mut Outbox) {
        let db = self.hub.store.get();
        let Some(u) = db.user(&self.key) else {
            return;
        };
        heading(out, "prof_h", "body", "Your Profile");
        out.element(ui::card("prof_stats").parent("body").gap("small"));
        for (i, line) in [
            format!("Handle: {} (handles are forever)", u.handle),
            format!("Member since {}", when(u.joined)),
            format!("{} calls, {} posts", u.calls, u.posts),
        ]
        .iter()
        .enumerate()
        {
            out.element(body(&format!("prof{i}"), "prof_stats", line));
        }
        out.element(caption("prof_key", "prof_stats", &format!("Key: {}", self.key)));
        out.element(
            ui::input("prof_location")
                .parent("body")
                .label("Calling from")
                .bind("f.location"),
        );
        out.element(
            ui::input("prof_tagline")
                .parent("body")
                .label("Tagline")
                .bind("f.tagline"),
        );
        out.element(ui::row("prof_buttons").parent("body"));
        out.element(submit(
            "prof_save",
            "prof_buttons",
            "Save",
            "primary",
            Handler::action("save_profile"),
        ));
        out.element(nav("prof_back", "prof_buttons", "Main menu", Screen::Menu));
    }

    fn save_profile(&mut self, out: &mut Outbox, location: &str, tagline: &str) {
        if location.chars().count() > MAX_LINE || tagline.chars().count() > MAX_LINE {
            return self.flash(out, "failure", &format!("At most {MAX_LINE} characters each."));
        }
        self.hub.store.edit(|db| {
            if let Some(u) = db.users.get_mut(&self.key) {
                u.location = location.into();
                u.tagline = tagline.into();
            }
        });
        // The confirmed values, normalized (trimmed).
        out.signal(json!({"f": {"location": location, "tagline": tagline}}));
        self.changed(out);
        self.flash(out, "success", "Profile saved.");
    }

    fn draw_sysop(&mut self, out: &mut Outbox) {
        heading(out, "sys_h", "body", "Sysop Console");
        out.element(
            ui::text("sys_stats")
                .parent("body")
                .variant("code")
                .text(cel("s._v.stats")),
        );
        out.element(caption(
            "sys_help",
            "body",
            "Ban callers from the user list, delete posts from their topics, \
             post bulletins from the bulletins screen.",
        ));
        out.element(ui::card("sys_boards").parent("body").title("Boards"));
        out.element(ui::list("sys_board_list").parent("sys_boards"));
        out.element(
            ui::input("sys_name")
                .parent("sys_boards")
                .label("New board")
                .bind("f.name"),
        );
        out.element(
            ui::input("sys_desc")
                .parent("sys_boards")
                .label("Description")
                .bind("f.desc"),
        );
        out.element(submit(
            "sys_add",
            "sys_boards",
            "Add the board",
            "primary",
            Handler::action("new_board"),
        ));
        back(out, Screen::Menu, "Main menu");
    }

    fn sync_sysop(&mut self, out: &mut Outbox) {
        let online = self.hub.online();
        let hub = self.hub.clone();
        let db = hub.store.get();
        let stats = format!(
            "users {}   topics {}   posts {}   letters {}   calls {}   online {online}",
            db.users.len(),
            db.topics.len(),
            db.posts.len(),
            db.mail.len(),
            db.calls,
        );
        self.put(out, json!({"_v": {"stats": stats}}));
        let want: Vec<_> = db.boards.iter().map(|b| (format!("sb{}", b.id), b)).collect();
        reconcile(out, &mut self.items, &want, |b| sysop_board_item(b));
    }

    fn new_board(&mut self, out: &mut Outbox, name: &str, desc: &str) {
        if name.is_empty() || name.chars().count() > MAX_LINE || desc.chars().count() > MAX_LINE {
            return self.flash(
                out,
                "failure",
                &format!("A board needs a name; both fields have at most {MAX_LINE} characters."),
            );
        }
        self.hub.store.edit(|db| db.add_board(name, desc));
        out.signal(json!({"f": {"name": "", "desc": ""}}));
        self.changed(out);
        self.flash(out, "success", &format!("{name} is open."));
    }

    fn draw_goodbye(&mut self, out: &mut Outbox) {
        println!("node {}: {} hung up", self.node().unwrap_or(0), self.handle());
        out.element(code(
            "bye",
            "body",
            &format!(
                "Thanks for calling {}!\nCome back soon.\n\n+++ATH0\nNO CARRIER",
                self.hub.config.name
            ),
        ));
    }
}

impl Drop for Bbs {
    fn drop(&mut self) {
        if self.screen == Screen::Chat && self.registered {
            self.hub.chat(
                ChatKind::System,
                &self.handle(),
                "dropped carrier",
                now(),
            );
        }
    }
}

/// Brings the rows of a list on screen to `want`, in order, with the fewest
/// messages: removes the rows that went away (with their children), inserts
/// the new ones where they belong and moves those out of place, by re-sending
/// just the row with `before`. Rows already in place get nothing.
///
/// `items` is the list's rows as the renderer has them. `build` makes a
/// row and its children, the row first, and is also the row's only
/// builder when it moves.
fn reconcile<T>(
    out: &mut Outbox,
    items: &mut Vec<String>,
    want: &[(String, T)],
    build: impl Fn(&T) -> Vec<Element>,
) {
    items.retain(|id| {
        let keep = want.iter().any(|(w, _)| w == id);
        if !keep {
            out.remove(id.as_str());
        }
        keep
    });
    // From the end, so the row each one goes before is already in place.
    for i in (0..want.len()).rev() {
        let (id, item) = &want[i];
        let next = want.get(i + 1).map(|(n, _)| n);
        match items.iter().position(|x| x == id) {
            None => {
                let mut elements = build(item).into_iter();
                let Some(row) = elements.next() else { continue };
                let at = next
                    .and_then(|n| items.iter().position(|x| x == n))
                    .unwrap_or(items.len());
                out.element(match next {
                    Some(n) => row.before(n.as_str()),
                    None => row,
                });
                for e in elements {
                    out.element(e);
                }
                items.insert(at, id.clone());
            }
            Some(p) => {
                let Some(n) = next else { continue };
                if items.get(p + 1) == Some(n) {
                    continue;
                }
                let Some(row) = build(item).into_iter().next() else { continue };
                out.element(row.before(n.as_str()));
                items.remove(p);
                let at = items.iter().position(|x| x == n).unwrap_or(items.len());
                items.insert(at, id.clone());
            }
        }
    }
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

// The rows of the lists, one builder each.

fn oneliner_item(db: &Db, o: &Oneliner, list: &str, dated: bool) -> Element {
    let line = format!("{:>width$}: {}", db.handle(&o.author), o.text, width = MAX_HANDLE);
    let text = if dated {
        format!("{} {line}", short_date(o.at))
    } else {
        line
    };
    code(&format!("o{}", o.id), list, &text)
}

fn board_item(b: &Board) -> Vec<Element> {
    let row = format!("b{}", b.id);
    vec![
        ui::row(&row).parent("boards").wrap(true),
        nav(&format!("{row}_go"), &row, &b.name, Screen::Board { id: b.id }),
        ui::badge(format!("{row}_new"))
            .parent(&row)
            .variant("info")
            .visible(cel(format!("s._unread.{row} > 0")))
            .text(cel(format!("string(s._unread.{row}) + ' new'"))),
        ui::text(format!("{row}_about"))
            .parent(&row)
            .variant("caption")
            .text(cel(format!("s._v.about.{row}"))),
    ]
}

fn topic_item(t: &Topic) -> Vec<Element> {
    let row = format!("t{}", t.id);
    vec![
        ui::row(&row).parent("topics").wrap(true),
        ui::badge(format!("{row}_new"))
            .parent(&row)
            .variant("info")
            .visible(cel(format!("s._v.new.{row}")))
            .text("NEW"),
        nav(&format!("{row}_go"), &row, &t.subject, Screen::Topic { id: t.id }),
        ui::text(format!("{row}_about"))
            .parent(&row)
            .variant("caption")
            .text(cel(format!("s._v.about.{row}"))),
    ]
}

/// A post never changes, so it is all literals.
fn post_item(db: &Db, p: &Post, can_delete: bool) -> Vec<Element> {
    let card = format!("p{}", p.id);
    let author = db.user(&p.author);
    let mut els = vec![
        ui::card(&card)
            .parent("posts")
            .title(format!(
                "Msg #{} · {} · {}",
                p.id,
                author.map_or("<unknown>", |u| &u.handle),
                when(p.at)
            ))
            .gap("small"),
        body(&format!("{card}_body"), &card, &p.body),
    ];
    if let Some(tag) = author.map(|u| &u.tagline).filter(|t| !t.is_empty()) {
        els.push(caption(&format!("{card}_tag"), &card, &format!("-- {tag}")));
    }
    if can_delete {
        els.push(
            submit(
                &format!("{card}_del"),
                &card,
                "Delete",
                "plain",
                Handler::action("delete_post").payload(p.id),
            ),
        );
    }
    els
}

fn chat_item(line: &ChatLine) -> Element {
    let text = match line.kind {
        ChatKind::Say => format!("[{}] <{}> {}", hm(line.at), line.who, line.text),
        ChatKind::Emote => format!("[{}] * {} {}", hm(line.at), line.who, line.text),
        ChatKind::System => format!("[{}] *** {} {}", hm(line.at), line.who, line.text),
    };
    ui::text(format!("chat{}", line.n))
        .parent("chatlog")
        .variant(if line.kind == ChatKind::System { "caption" } else { "code" })
        .text(text)
}

fn node_item(n: u32) -> Vec<Element> {
    let row = format!("n{n}");
    let v = format!("s._v.who.{row}");
    vec![
        ui::row(&row).parent("who").wrap(true),
        ui::badge(format!("{row}_n")).parent(&row).text(format!("Node {n}")),
        ui::text(format!("{row}_who")).parent(&row).text(cel(format!("{v}.who"))),
        ui::text(format!("{row}_doing"))
            .parent(&row)
            .variant("caption")
            .text(cel(format!("{v}.doing"))),
        submit(
            &format!("{row}_page"),
            &row,
            "Page",
            "plain",
            Handler::action("page").payload(n),
        )
        .visible(cel(format!("{v}.page"))),
    ]
}

fn user_item(key: &str, u: &User, me: bool, sysop: bool) -> Vec<Element> {
    let row = format!("u{key}");
    let v = format!("s._v.users.{row}");
    let mut els = vec![
        ui::row(&row).parent("users").wrap(true),
        // Handles are forever.
        body(&format!("{row}_name"), &row, &u.handle),
        ui::badge(format!("{row}_sysop"))
            .parent(&row)
            .variant("success")
            .visible(cel(format!("{v}.sysop")))
            .text("SYSOP"),
        ui::badge(format!("{row}_banned"))
            .parent(&row)
            .variant("failure")
            .visible(cel(format!("{v}.banned")))
            .text("BANNED"),
        ui::text(format!("{row}_about"))
            .parent(&row)
            .variant("caption")
            .text(cel(format!("{v}.about"))),
    ];
    if !me {
        els.push(nav(
            &format!("{row}_mail"),
            &row,
            "Write",
            Screen::Compose {
                to: u.handle.clone(),
                subject: String::new(),
            },
        ));
    }
    if !me && sysop {
        // A payload is a literal, so the two requests are two buttons, and
        // the confirmed state picks the one to show.
        for (name, label, show) in [("ban", "Ban", "!"), ("unban", "Unban", "")] {
            els.push(
                submit(
                    &format!("{row}_{name}"),
                    &row,
                    label,
                    "failure",
                    Handler::action(name).payload(key),
                )
                .visible(cel(format!("{show}{v}.banned"))),
            );
        }
    }
    els
}

fn letter_item(db: &Db, l: &Letter) -> Vec<Element> {
    let row = format!("l{}", l.id);
    vec![
        ui::row(&row).parent("inbox").wrap(true),
        ui::badge(format!("{row}_new"))
            .parent(&row)
            .variant("info")
            .visible(cel(format!("s._v.new.{row}")))
            .text("NEW"),
        nav(&format!("{row}_go"), &row, &l.subject, Screen::Letter { id: l.id }),
        caption(
            &format!("{row}_from"),
            &row,
            &format!("from {} · {}", db.handle(&l.from), when(l.at)),
        ),
    ]
}

fn bulletin_item(b: &Bulletin, sysop: bool) -> Vec<Element> {
    let card = format!("bull{}", b.id);
    let mut els = vec![
        ui::card(&card).parent("bulls").title(b.title.as_str()).gap("small"),
        body(&format!("{card}_body"), &card, &b.body),
        caption(&format!("{card}_at"), &card, &when(b.at)),
    ];
    if sysop {
        els.push(submit(
            &format!("{card}_del"),
            &card,
            "Delete",
            "plain",
            Handler::action("delete_bulletin").payload(b.id),
        ));
    }
    els
}

fn sysop_board_item(b: &Board) -> Vec<Element> {
    let row = format!("sb{}", b.id);
    vec![
        ui::row(&row).parent("sys_board_list"),
        body(&format!("{row}_name"), &row, &b.name),
        submit(
            &format!("{row}_del"),
            &row,
            "Delete with all its posts",
            "failure",
            Handler::action("delete_board").payload(b.id),
        ),
    ]
}

/// The default login art: the board's name in a box.
pub fn default_banner(name: &str) -> String {
    let title: String = name
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, ' '])
        .collect::<String>()
        .trim_end()
        .to_owned();
    let tagline = "a bulletin board system on hyperb";
    let inner = title.chars().count().max(tagline.len()) + 8;
    let center = |s: &str| {
        let pad = inner - s.chars().count();
        format!("║{}{s}{}║", " ".repeat(pad / 2), " ".repeat(pad - pad / 2))
    };
    [
        format!("╔{}╗", "═".repeat(inner)),
        center(""),
        center(&title),
        center(""),
        center(tagline),
        format!("╚{}╝", "═".repeat(inner)),
    ]
    .join("\n")
}

fn in_chat(n: &Node) -> bool {
    n.doing == Screen::Chat.doing()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn local(ts: i64) -> chrono::DateTime<Local> {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(Local::now)
}

fn when(ts: i64) -> String {
    local(ts).format("%Y-%m-%d %H:%M").to_string()
}

fn short_date(ts: i64) -> String {
    local(ts).format("%m/%d").to_string()
}

fn hm(ts: i64) -> String {
    local(ts).format("%H:%M").to_string()
}

/// "handle from location".
fn caller_line(db: &Db, key: &str) -> String {
    match db.user(key) {
        Some(u) if !u.location.is_empty() => format!("{} from {}", u.handle, u.location),
        Some(u) => u.handle.clone(),
        None => "<unknown>".into(),
    }
}

fn go(s: Screen) -> Handler {
    Handler::action("go").payload(serde_json::to_value(s).expect("screens serialize"))
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

fn heading(out: &mut Outbox, id: &str, parent: &str, text: &str) {
    out.element(ui::text(id).parent(parent).variant("h2").text(text));
}

fn body(id: &str, parent: &str, text: &str) -> Element {
    ui::text(id).parent(parent).text(text)
}

fn caption(id: &str, parent: &str, text: &str) -> Element {
    ui::text(id).parent(parent).variant("caption").text(text)
}

fn code(id: &str, parent: &str, text: &str) -> Element {
    ui::text(id).parent(parent).variant("code").text(text)
}

fn banner(art: &str) -> Element {
    code("banner", "body", art)
}

/// A button that goes to a screen.
fn nav(id: &str, parent: &str, label: &str, to: Screen) -> Element {
    ui::button(id)
        .parent(parent)
        .label(label)
        .variant("plain")
        .on_press(go(to))
}

/// A menu entry, with a count (an expression) of what is new there.
fn menu_item(out: &mut Outbox, id: &str, parent: &str, label: &str, to: Screen, count: Option<&str>) {
    let row = format!("{id}_row");
    out.element(ui::row(&row).parent(parent));
    out.element(nav(id, &row, label, to));
    if let Some(count) = count {
        out.element(
            ui::badge(format!("{id}_new"))
                .parent(&row)
                .variant("info")
                .visible(cel(format!("{count} > 0")))
                .text(cel(format!("string({count})"))),
        );
    }
}

fn back(out: &mut Outbox, to: Screen, label: &str) {
    out.element(
        ui::button("back")
            .parent("body")
            .label(format!("« {label}"))
            .on_press(go(to)),
    );
}

#[cfg(test)]
mod tests;
