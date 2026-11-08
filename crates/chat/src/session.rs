//! One caller, over whichever transport brought them: their key (proven by
//! the transport) is their account, and the lines of the session drive a
//! [`Chat`].

use crate::chat::Chat;
use crate::db::MemberKey;
use crate::hub::Hub;
use hypereffect::{Outbox, RendererMessage};
use hyperserve::Session;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub async fn run(hub: Arc<Hub>, mut session: Session) {
    // Subscribe first, so nothing that happens after the hello is missed.
    let mut events = hub.subscribe();
    let mut chat = Chat::new(hub, MemberKey::new(session.key.to_string()));
    let mut out = Outbox::new();
    loop {
        tokio::select! {
            line = session.input.recv() => match line.as_deref().map(RendererMessage::parse) {
                Some(Ok(RendererMessage::Hello { params })) => chat.hello(&mut out, &params),
                Some(Ok(RendererMessage::Action(a))) => chat.action(&mut out, &a),
                Some(Err(_)) => {}
                None => break,
            },
            event = events.recv() => match event {
                Ok(e) => chat.event(&mut out, &e),
                Err(RecvError::Lagged(_)) => chat.refresh(&mut out),
                Err(RecvError::Closed) => break,
            },
        }
        let chunk = out.take();
        if !chunk.is_empty() && session.output.send(chunk).is_err() {
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
    use std::time::Duration;

    /// Two callers over HTTP: one says something, the other sees it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_reaches_another_caller() {
        let hub = Hub::new(
            Config {
                name: "Test chat".into(),
                invite_only: false,
            },
            Store::memory(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            http::serve_on("Test chat", listener, &Tls::default(), move |s| {
                run(hub.clone(), s)
            })
            .await
        });
        let url = address::normalize(&format!("hyperb://127.0.0.1:{port}")).unwrap();
        async fn next(conn: &Connection) -> Event {
            tokio::time::timeout(Duration::from_secs(10), conn.events().recv())
                .await
                .expect("timeout")
                .unwrap()
        }
        async fn wait_for(conn: &Connection, text: &str) {
            let mut seen = String::new();
            while !seen.contains(text) {
                match next(conn).await {
                    Event::Data(lines) => seen.push_str(&lines),
                    e => panic!("{e:?}"),
                }
            }
        }
        let call = async |name: &str| {
            let conn = Connection::open(&url, &Identity::generate());
            assert_eq!(next(&conn).await, Event::Open);
            conn.send("hello {}\n".into());
            conn.send(format!(
                "action {{\"name\":\"join\",\"signals\":{{\"f\":{{\"name\":\"{name}\"}}}}}}\n"
            ));
            conn
        };
        let ann = call("Ann").await;
        let bob = call("Bob").await;
        wait_for(&ann, "No messages yet").await;
        wait_for(&bob, "No messages yet").await;
        ann.send(
            "action {\"name\":\"send\",\"signals\":{\"f\":{\"msg\":\"hello there\"}}}\n".into(),
        );
        wait_for(&bob, "hello there").await;
    }
}
