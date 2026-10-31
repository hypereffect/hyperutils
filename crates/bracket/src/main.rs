//! bracket: knockout brackets for [hyperb](../../hyperb). Anyone can type in
//! the teams, place the ones they want in the first round and let the draw
//! place the rest, and get a bracket: a round of 16, quarterfinals,
//! semifinals and a final, or as many rounds as the teams need. The bracket
//! has two links: one where anyone can enter the results, and one that only
//! shows them. People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p bracket -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Brackets are kept in `bracket.redb` in the data directory, and the ones
//! left untouched for 180 days are deleted.

mod bracket;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often idle brackets are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/bracket (or
    /// ~/.local/share/bracket).
    #[arg(long, env = "BRACKET_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "BRACKET_LISTEN", default_value = "127.0.0.1:8407")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("bracket: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("bracket.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(PRUNE_EVERY);
        loop {
            timer.tick().await;
            let n = bracket::prune(&pruner);
            if n > 0 {
                println!("deleted {n} idle bracket(s)");
            }
        }
    });
    let server = http::serve("bracket", args.listen, &args.tls, move |session| {
        let app = bracket::Brackets::new(db.clone(), session.key, &session.origin, &session.path);
        hyperserve::run_app(session, app)
    });
    tokio::select! {
        r = server => r?,
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
    Some(base.join("bracket"))
}
