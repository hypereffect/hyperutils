//! forms: a form builder for [hyperb](../../hyperb). Anyone can make a form
//! with any number of questions, pick how long it lives, and share its link,
//! `hyperb://<host>/<id>`. Whoever opens the link fills it in; the creator
//! (or anyone, if the form is public) sees the answers at `/<id>/results`.
//! People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p forms -- [--data DIR] [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Forms and their responses are kept in `forms.redb` in the data directory,
//! and expired forms are deleted every six hours.

mod forms;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often expired forms are deleted.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the database lives. Defaults to $XDG_DATA_HOME/forms (or
    /// ~/.local/share/forms).
    #[arg(long, env = "FORMS_DATA")]
    data: Option<PathBuf>,
    /// The address to answer on.
    #[arg(long, env = "FORMS_LISTEN", default_value = "127.0.0.1:8403")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("forms: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let path = data.join("forms.redb");
    let db = store::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let db = Arc::new(db);
    println!("data in {}", data.display());

    let pruner = db.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(PRUNE_EVERY);
        loop {
            timer.tick().await;
            let n = forms::prune(&pruner);
            if n > 0 {
                println!("deleted {n} expired form(s)");
            }
        }
    });
    let server = http::serve("forms", args.listen, &args.tls, move |session| {
        let app = forms::Forms::new(db.clone(), session.key, &session.origin, &session.path);
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
    Some(base.join("forms"))
}
