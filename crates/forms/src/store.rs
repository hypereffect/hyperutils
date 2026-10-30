//! The forms, their responses and who owns what, in an [orangedb] database.
//! Owners and respondents are keyed by the hex of the renderer's public key.
//!
//! A storage error is nothing a visitor can fix, so [`read`] and [`write`]
//! panic on one: that ends the session it happened in, which runs in its own
//! task, and nothing else.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub use schema::*;

#[orangedb::schema]
pub mod schema {
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Form {
        /// A cuid2, which is also the path of the form's link.
        #[id(String)]
        pub id: FormId,
        /// The creator's key.
        pub owner: String,
        pub title: String,
        pub description: String,
        pub fields: Vec<crate::store::Field>,
        /// Whether anyone may see the results, not only the owner.
        pub public: bool,
        /// When it is deleted, in seconds since the epoch.
        pub expires: u64,
        /// The id the next new field gets, so that ids are never reused and
        /// answers to a deleted question never land on a new one.
        pub next_field: u32,
        /// Bumped on every change and response, so that watchers know to
        /// refresh.
        pub version: u64,
    }

    /// One person's answers to a form. Each key has at most one, which they
    /// can change.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Response {
        #[id(auto)]
        pub id: ResponseId,
        #[references(Form, on_delete = cascade)]
        pub form: FormId,
        pub key: String,
        pub answers: Vec<crate::store::Answer>,
        /// When it was last saved, in seconds since the epoch.
        pub at: u64,
    }

    /// The forms a key created, newest first. A deleted form leaves the list
    /// by itself.
    #[entity]
    #[derive(Clone, Debug)]
    pub struct Owner {
        #[id(String)]
        pub key: OwnerKey,
        #[references(Form, on_delete = set_null)]
        pub forms: Vec<FormId>,
    }
}

/// A question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    /// Stable across edits; answers refer to it.
    pub id: u32,
    pub label: String,
    pub kind: Kind,
    /// The choices, for the kinds that have them.
    pub options: Vec<String>,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Text,
    Paragraph,
    Number,
    /// One of the options, as radio buttons.
    Choice,
    /// One of the options, from a dropdown.
    Dropdown,
    /// Any of the options.
    Checkboxes,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Text,
        Kind::Paragraph,
        Kind::Number,
        Kind::Choice,
        Kind::Dropdown,
        Kind::Checkboxes,
    ];

    /// Its name in signals.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Paragraph => "paragraph",
            Kind::Number => "number",
            Kind::Choice => "choice",
            Kind::Dropdown => "dropdown",
            Kind::Checkboxes => "checkboxes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "Short answer",
            Kind::Paragraph => "Paragraph",
            Kind::Number => "Number",
            Kind::Choice => "Multiple choice",
            Kind::Dropdown => "Dropdown",
            Kind::Checkboxes => "Checkboxes",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    pub fn has_options(self) -> bool {
        matches!(self, Kind::Choice | Kind::Dropdown | Kind::Checkboxes)
    }
}

/// The answer to one field: the text, or the options picked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub field: u32,
    pub values: Vec<String>,
}

pub fn read<R>(db: &Db, f: impl FnOnce(&ReadTx) -> orangedb::Result<R>) -> R {
    db.read(f).unwrap_or_else(|e| panic!("database: {e}"))
}

pub fn write<R>(db: &Db, f: impl FnOnce(&WriteTx) -> orangedb::Result<R>) -> R {
    db.write(f).unwrap_or_else(|e| panic!("database: {e}"))
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
