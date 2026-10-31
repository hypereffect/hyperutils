//! tournament: round-robin tournaments for [hyperb](../../hyperb). Anyone can
//! type in the teams and get the fixtures, every team meeting every other
//! once (or twice, home and away). The tournament has two links: one where
//! anyone can enter the results, and one that only shows the fixtures and
//! the standings. People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p tournament -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Tournaments are kept in `tournament.redb` in the data directory, and the
//! ones left untouched for 180 days are deleted.

mod store;
mod tournament;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often idle tournaments are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/tournament (or
    /// ~/.local/share/tournament).
    #[arg(long, env = "TOURNAMENT_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "TOURNAMENT_LISTEN", default_value = "127.0.0.1:8404")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("tournament: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("tournament.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(PRUNE_EVERY);
        loop {
            timer.tick().await;
            let n = tournament::prune(&pruner);
            if n > 0 {
                println!("deleted {n} idle tournament(s)");
            }
        }
    });
    let server = http::serve("tournament", args.listen, &args.tls, move |session| {
        let app =
            tournament::Tournaments::new(db.clone(), session.key, &session.origin, &session.path);
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
    Some(base.join("tournament"))
}
