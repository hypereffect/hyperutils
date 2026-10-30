//! The hyperb transports the hyperutils servers share. Each server is one
//! hypereffect app, and sees its callers as [`Session`]s whatever carried
//! them:
//!
//! - [`http`]: HTTPS (or plain HTTP behind a TLS-terminating proxy) with
//!   Server-Sent Events, as `hyperb_core::http` describes, at
//!   `hyperb://<host>[:port]`.
//! - [`iroh`] (feature `iroh`): `hyperb://<endpoint id>`, on the root ALPN.
//!
//! Either way the caller's public key is proven by the transport, so it can
//! stand for their account. [`run_app`] drives a [`hypereffect::App`] over a
//! session; servers with richer needs read and write the lines themselves.

pub mod app;
pub mod http;
#[cfg(feature = "iroh")]
pub mod iroh;

pub use app::run_app;
pub use hyperb_core::identity::PublicKey;

use tokio::sync::mpsc;

/// One caller, connected.
pub struct Session {
    /// The caller's key, proven by the transport.
    pub key: PublicKey,
    /// The address the caller dialed, without its path:
    /// `hyperb://<host>[:port]` or `hyperb://<endpoint id>`.
    pub origin: String,
    /// The path of the address, starting with `/`. Over iroh, which serves
    /// the root ALPN only, it is always `/`.
    pub path: String,
    /// The lines from the renderer, without their line feeds, starting with
    /// the `hello`. It ends when the caller hangs up.
    pub input: mpsc::UnboundedReceiver<String>,
    /// Lines for the renderer, each ending with a line feed, as
    /// [`hypereffect::Outbox::take`] gives them. Dropping it hangs up.
    pub output: mpsc::UnboundedSender<String>,
}
