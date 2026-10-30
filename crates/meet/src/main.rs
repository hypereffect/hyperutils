//! meet: a Doodle-style scheduler for [hyperb](../../hyperb). Someone creates
//! a meeting with candidate times and gets a short code to share; others open
//! meet, type the code and mark which times suit them, and everyone sees each
//! other's answers live. People are recognized by their public key.
//!
//! ```text
//! cargo run --release -p meet -- [--listen ADDR] [--tls-cert PEM --tls-key PEM]
//! ```
//!
//! Meetings are kept in memory only, so they do not survive a restart.

mod meet;
mod store;

use clap::Parser;
use hyperserve::http::{self, Tls};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The address to answer on.
    #[arg(long, env = "MEET_LISTEN", default_value = "127.0.0.1:8402")]
    listen: SocketAddr,
    #[command(flatten)]
    tls: Tls,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    tokio::spawn(async {
        let mut timer = tokio::time::interval(Duration::from_secs(60 * 60));
        loop {
            timer.tick().await;
            meet::prune();
        }
    });
    let server = http::serve("meet", args.listen, &args.tls, |session| {
        let app = meet::Meet::new(session.key);
        hyperserve::run_app(session, app)
    });
    tokio::select! {
        r = server => if let Err(e) = r {
            eprintln!("meet: {e}");
            std::process::exit(1);
        },
        _ = tokio::signal::ctrl_c() => println!("shutting down"),
    }
}
