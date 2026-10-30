//! meet: a Doodle-style scheduler for [hyperb](../../hyperb). Someone creates
//! a meeting with candidate times and gets a short code to share; others open
//! meet, type the code and mark which times suit them, and everyone sees each
//! other's answers live. People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p meet -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Meetings and the people who answered them are kept in `meet.redb` in the
//! data directory.

mod meet;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/meet (or
    /// ~/.local/share/meet).
    #[arg(long, env = "MEET_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "MEET_LISTEN", default_value = "127.0.0.1:8402")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("meet: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("meet.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(60 * 60));
        loop {
            timer.tick().await;
            meet::prune(&pruner);
        }
    });
    let server = http::serve("meet", args.listen, &args.tls, move |session| {
        let app = meet::Meet::new(db.clone(), session.key);
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
    Some(base.join("meet"))
}
