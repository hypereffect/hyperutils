//! signup: sign-up sheets for [hyperb](../../hyperb). Anyone can make a sheet
//! listing the roles to fill or the things to bring, and share its link,
//! `hyperb://<host>/<id>`. Whoever opens the link puts a name down for an
//! item. Every name, and every change in the sheet's history, shows a short
//! tag of the key that made it, so everyone can see who wrote what. People
//! are recognized by their public key.
//!
//! ```text
//! cargo run --release -p signup -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Sheets are kept in `signup.redb` in the data directory, and the ones left
//! untouched for 180 days are deleted.

mod signup;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often idle sheets are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/signup (or
    /// ~/.local/share/signup).
    #[arg(long, env = "SIGNUP_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "SIGNUP_LISTEN", default_value = "127.0.0.1:8405")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("signup: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("signup.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(PRUNE_EVERY);
        loop {
            timer.tick().await;
            let n = signup::prune(&pruner);
            if n > 0 {
                println!("deleted {n} idle sheet(s)");
            }
        }
    });
    let server = http::serve("signup", args.listen, &args.tls, move |session| {
        let app = signup::Sheets::new(db.clone(), session.key, &session.origin, &session.path);
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
    Some(base.join("signup"))
}
