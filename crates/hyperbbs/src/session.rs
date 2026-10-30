//! One caller on the line, over whichever transport brought them: their key
//! (proven by the transport) is their account, and the lines of the session
//! drive a [`Bbs`].

use crate::bbs::Bbs;
use crate::hub::Hub;
use hypereffect::{Outbox, RendererMessage};
use hyperserve::Session;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// How long the last screen stays up before the line drops.
const HANGUP_DELAY: Duration = Duration::from_secs(2);

pub async fn run(hub: Arc<Hub>, mut session: Session) {
    // Subscribe first, so nothing that happens after the hello is missed.
    let mut events = hub.subscribe();
    let mut bbs = Bbs::new(hub, session.key.to_string());
    let mut out = Outbox::new();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            line = session.input.recv() => match line.as_deref().map(RendererMessage::parse) {
                Some(Ok(RendererMessage::Hello { params })) => bbs.hello(&mut out, &params),
                Some(Ok(RendererMessage::Action(a))) => bbs.action(&mut out, &a),
                Some(Err(_)) => {}
                None => break,
            },
            event = events.recv() => match event {
                Ok(e) => bbs.event(&mut out, &e),
                Err(RecvError::Lagged(_)) => bbs.refresh(&mut out),
                Err(RecvError::Closed) => break,
            },
            _ = clock.tick() => bbs.tick(&mut out),
        }
        let chunk = out.take();
        if !chunk.is_empty() && session.output.send(chunk).is_err() {
            break;
        }
        if bbs.hung_up() {
            tokio::time::sleep(HANGUP_DELAY).await;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use crate::hub::Config;
    use hyperb_core::address;
    use hyperb_core::identity::Identity;
    use hyperb_core::net::{Connection, Event};
    use hyperserve::http::{self, Tls};

    fn hub() -> Arc<Hub> {
        Hub::new(
            Config {
                name: "Test BBS".into(),
                nodes: 4,
                sysops: vec![],
                banner: crate::bbs::default_banner("Test BBS"),
            },
            Store::memory(),
        )
    }

    /// Calls `address` as a renderer would and registers as Neo.
    async fn call(address: &str) {
        let url = address::normalize(address).unwrap();
        let conn = Connection::open(&url, &Identity::generate());
        let events = conn.events();
        let next = || async {
            tokio::time::timeout(Duration::from_secs(60), events.recv())
                .await
                .expect("timeout")
                .unwrap()
        };
        assert_eq!(next().await, Event::Open);
        conn.send("hello {}\n".into());
        let mut seen = String::new();
        while !seen.contains("New user application") {
            match next().await {
                Event::Data(lines) => seen.push_str(&lines),
                e => panic!("{e:?}"),
            }
        }
        conn.send(
            "action {\"name\":\"register\",\"signals\":{\"f\":{\"handle\":\"Neo\"}}}\n".into(),
        );
        while !seen.contains("Welcome, Neo!") {
            match next().await {
                Event::Data(lines) => seen.push_str(&lines),
                e => panic!("{e:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn caller_reaches_the_board_over_http() {
        let hub = hub();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            http::serve_on("Test BBS", listener, &Tls::default(), move |s| {
                run(hub.clone(), s)
            })
            .await
        });
        call(&format!("hyperb://127.0.0.1:{port}")).await;
    }

    /// Dials through iroh's discovery, so it needs internet access:
    /// `cargo test -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn caller_reaches_the_board_over_iroh() {
        let hub = hub();
        let server = Identity::generate();
        let public = server.public();
        tokio::spawn(hyperserve::iroh::serve("Test BBS", server, move |s| {
            run(hub.clone(), s)
        }));
        // Let it publish its address.
        tokio::time::sleep(Duration::from_secs(10)).await;
        call(&format!("hyperb://{public}")).await;
    }
}
