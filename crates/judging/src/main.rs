//! judging: judged contests for [hyperb](../../hyperb). The organizer types
//! in the entries and the judges, and every judge gets a link of their own
//! that opens on their name. Each judge scores privately; for every entry the
//! highest and the lowest score are dropped and the rest averaged, and a
//! leaderboard appears once the organizer reveals it. Photo contests,
//! hackathons, bake-offs. People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p judging -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Contests are kept in `judging.redb` in the data directory, and the ones
//! left untouched for 180 days are deleted.

mod judging;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often idle contests are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/judging (or
    /// ~/.local/share/judging).
    #[arg(long, env = "JUDGING_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "JUDGING_LISTEN", default_value = "127.0.0.1:8408")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("judging: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("judging.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(PRUNE_EVERY);
        loop {
            timer.tick().await;
            let n = judging::prune(&pruner);
            if n > 0 {
                println!("deleted {n} idle contest(s)");
            }
        }
    });
    let server = http::serve("judging", args.listen, &args.tls, move |session| {
        let app = judging::Contests::new(db.clone(), session.key, &session.origin, &session.path);
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
    Some(base.join("judging"))
}
