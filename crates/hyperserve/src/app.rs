//! Drives a [`hypereffect::App`] over a [`Session`]: the same job as
//! [`hypereffect::Server`], but the app is built by the server (so it can be
//! given the caller's key) and its ticks run on tokio's clock.

use crate::Session;
use hypereffect::{App, Outbox, RendererMessage};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, Interval, interval_at};

/// Runs `app` until the caller hangs up. Lines before the `hello`, a second
/// `hello` and lines that do not parse are ignored.
pub async fn run_app<A: App>(mut session: Session, mut app: A) {
    let mut out = Outbox::new();
    let mut started = false;
    let mut ticker: Option<(u32, Interval)> = None;
    loop {
        let chunk = out.take();
        if !chunk.is_empty() && session.output.send(chunk).is_err() {
            break;
        }
        let want = if started { app.tick_interval() } else { None };
        if want != ticker.as_ref().map(|t| t.0) {
            ticker = want.map(|ms| {
                let every = Duration::from_millis(ms.max(1).into());
                (ms, interval_at(Instant::now() + every, every))
            });
        }
        tokio::select! {
            line = session.input.recv() => match line.as_deref().map(RendererMessage::parse) {
                None => break,
                Some(Ok(RendererMessage::Hello { params })) if !started => {
                    started = true;
                    app.hello(&mut out, &params);
                }
                Some(Ok(RendererMessage::Action(a))) if started => app.action(&mut out, &a),
                Some(_) => {}
            },
            _ = async { ticker.as_mut().unwrap().1.tick().await }, if ticker.is_some() => {
                app.tick(&mut out, now_ms());
            }
        }
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{self, Tls};
    use hyperb_core::address;
    use hyperb_core::identity::{Identity, PublicKey};
    use hyperb_core::net::{Connection, Event};
    use hypereffect::{Action, json};
    use std::collections::BTreeMap;

    /// Greets the caller by key and counts their presses.
    struct Counter {
        key: PublicKey,
        n: u32,
    }

    impl App for Counter {
        fn hello(&mut self, out: &mut Outbox, _: &BTreeMap<String, String>) {
            out.signal(json!({"key": self.key.to_string(), "n": self.n}));
        }

        fn action(&mut self, out: &mut Outbox, _: &Action) {
            self.n += 1;
            out.signal(json!({"n": self.n}));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_app_over_http() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            http::serve_on("counter", listener, &Tls::default(), |s| {
                let app = Counter { key: s.key, n: 0 };
                run_app(s, app)
            })
            .await
        });
        let me = Identity::generate();
        let url = address::normalize(&format!("hyperb://127.0.0.1:{port}/any/path")).unwrap();
        let conn = Connection::open(&url, &me);
        let events = conn.events();
        let next = || async {
            tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("timeout")
                .unwrap()
        };
        assert_eq!(next().await, Event::Open);
        // Actions before the hello are ignored.
        conn.send("action {\"name\":\"press\"}\n".into());
        conn.send("hello {}\n".into());
        let key = me.public().to_string();
        assert_eq!(
            next().await,
            Event::Data(format!("signal {{\"key\":\"{key}\",\"n\":0}}\n"))
        );
        conn.send("action {\"name\":\"press\"}\n".into());
        assert_eq!(next().await, Event::Data("signal {\"n\":1}\n".into()));
    }
}
