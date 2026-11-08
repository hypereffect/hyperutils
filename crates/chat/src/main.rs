//! chat: chat rooms for [hyperb](../../hyperb), served over HTTPS and iroh.
//!
//! ```text
//! cargo run --release -p chat -- [--data DIR] [--name NAME] [--invite-only]
//!     [--listen ADDR] [--tls-cert PEM --tls-key PEM] [--no-iroh]
//! ```
//!
//! Callers open `hyperb://<host>` or the `hyperb://<endpoint id>` address it
//! prints. Their key is their account: new keys pick a name (and, if they
//! like, a picture) to join, known ones go straight to the rooms. On an
//! invite-only server joining takes an invite, which members make; while
//! nobody has joined, the server prints one for the first member.
//!
//! The data directory holds the server's key (`identity`) and the database
//! (`chat.redb`).

mod avatar;
mod chat;
mod db;
mod hub;
mod session;

use clap::Parser;
use hyperb_core::identity::Identity;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the server key and the database live. Defaults to
    /// $XDG_DATA_HOME/chat (or ~/.local/share/chat).
    #[arg(long, env = "CHAT_DATA")]
    data: Option<PathBuf>,
    /// The server's name.
    #[arg(long, default_value = "hyperchat")]
    name: String,
    /// Let in only those who type an invite.
    #[arg(long)]
    invite_only: bool,
    /// The address to answer HTTP(S) callers on.
    #[arg(long, env = "CHAT_LISTEN", default_value = "127.0.0.1:8409")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
    /// Do not answer over iroh.
    #[arg(long)]
    no_iroh: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("chat: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let identity = Identity::load_or_create(&data.join("identity")).map_err(|e| e.to_string())?;
    let store = db::Store::open(&data.join("chat.redb"))?;
    println!("data in {}", data.display());
    if args.invite_only && store.members().is_empty() {
        let code = store
            .usable_invite()
            .unwrap_or_else(|| store.add_invite(None, Some(1), chrono::Utc::now().timestamp()));
        println!("nobody has joined yet; the first member can use the invite {code}");
    }
    let hub = hub::Hub::new(
        hub::Config {
            name: args.name,
            invite_only: args.invite_only,
        },
        store,
    );

    let service = {
        let hub = hub.clone();
        move |s| session::run(hub.clone(), s)
    };
    let iroh = async {
        if args.no_iroh {
            std::future::pending().await
        } else {
            hyperserve::iroh::serve(&hub.config.name, identity, service.clone()).await
        }
    };
    tokio::select! {
        r = http::serve(&hub.config.name, args.listen, &args.tls, service.clone()) => r?,
        r = iroh => r?,
        _ = tokio::signal::ctrl_c() => println!("shutting down"),
    }
    Ok(())
}

fn default_data() -> Option<PathBuf> {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")))?;
    Some(base.join("chat"))
}
