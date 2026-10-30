//! hyperbbs: an old-school bulletin board system whose screens are
//! [hypereffect](https://crates.io/crates/hypereffect) components, served
//! to [hyperb](../../hyperb) renderers over HTTPS and iroh.
//!
//! ```text
//! cargo run --release -p hyperbbs -- [--data DIR] [--name NAME] [--nodes N] [--sysop KEY]...
//!     [--listen ADDR] [--tls-cert PEM --tls-key PEM] [--no-iroh]
//! ```
//!
//! Callers open `hyperb://<host>` or the `hyperb://<endpoint id>` address it
//! prints. Their key is their account: new keys fill in a new user
//! application, known ones are welcomed back. The first caller to register
//! becomes the sysop.
//!
//! The data directory holds the server's key (`identity`), the database
//! (`bbs.json`) and, if the sysop wants their own login art, `banner.txt`.

mod bbs;
mod db;
mod hub;
mod session;

use clap::Parser;
use hyperb_core::identity::{Identity, PublicKey};
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Where the server key, the database and banner.txt live. Defaults to
    /// $XDG_DATA_HOME/hyperbbs (or ~/.local/share/hyperbbs).
    #[arg(long, env = "HYPERBBS_DATA")]
    data: Option<PathBuf>,
    /// The board's name.
    #[arg(long, default_value = "hyperbbs")]
    name: String,
    /// How many callers can be on at once.
    #[arg(long, default_value_t = 16)]
    nodes: u32,
    /// A key (64 hex characters) that is always a sysop. Repeatable.
    #[arg(long = "sysop", value_name = "KEY")]
    sysops: Vec<String>,
    /// The address to answer HTTP(S) callers on.
    #[arg(long, env = "HYPERBBS_LISTEN", default_value = "127.0.0.1:8401")]
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
        eprintln!("hyperbbs: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data = args
        .data
        .or_else(default_data)
        .ok_or("no data directory, pass --data")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let sysops = args
        .sysops
        .iter()
        .map(|k| k.parse::<PublicKey>().map(|k| k.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("--sysop: {e}"))?;
    let identity = Identity::load_or_create(&data.join("identity")).map_err(|e| e.to_string())?;
    let store = db::Store::open(&data.join("bbs.json"))?;
    let banner = std::fs::read_to_string(data.join("banner.txt"))
        .map(|b| b.trim_end().to_owned())
        .unwrap_or_else(|_| bbs::default_banner(&args.name));
    let hub = hub::Hub::new(
        hub::Config {
            name: args.name,
            nodes: args.nodes.max(1),
            sysops,
            banner,
        },
        store,
    );
    println!("data in {}", data.display());

    let saver = hub.clone();
    tokio::spawn(async move { saver.store.keep_saved().await });
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
    hub.store.save().map_err(|e| format!("saving: {e}"))
}

fn default_data() -> Option<PathBuf> {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")))?;
    Some(base.join("hyperbbs"))
}
