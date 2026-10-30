//! The iroh session of hyperb's `SPEC.md` (section 3). Callers dial
//! `hyperb://<endpoint id>`, which offers the root ALPN `hyperb/0`. The
//! handshake proves the caller's endpoint key, and one bidirectional stream
//! carries the lines of the session.

use crate::Session;
use hyperb_core::address::IROH_ALPN;
use hyperb_core::identity::{Identity, PublicKey};
use hypereffect::LineBuffer;
use iroh::endpoint::{Incoming, presets};
use iroh::{Endpoint, SecretKey};
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long the caller gets to close the connection after the last line.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Serves the app named `name` with the endpoint key of `identity`, running
/// `service` for each session.
pub async fn serve<F, Fut>(name: &str, identity: Identity, service: F) -> Result<(), String>
where
    F: Fn(Session) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&identity.secret()))
        .alpns(vec![IROH_ALPN.as_bytes().to_vec()])
        .bind()
        .await
        .map_err(|e| e.to_string())?;
    println!("{name} answers at hyperb://{}", identity.public());
    endpoint.online().await;
    println!("{name} is online over iroh");
    while let Some(incoming) = endpoint.accept().await {
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(e) = session(incoming, service).await {
                eprintln!("iroh session: {e}");
            }
        });
    }
    Ok(())
}

async fn session<F, Fut>(incoming: Incoming, service: F) -> Result<(), String>
where
    F: Fn(Session) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let conn = incoming
        .accept()
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())?;
    let key: PublicKey = conn
        .remote_id()
        .to_string()
        .parse()
        .map_err(|_| "bad key")?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
    let (input, input_rx) = mpsc::unbounded_channel();
    let (output, mut output_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(service(Session {
        key,
        input: input_rx,
        output,
    }));

    // Reading stops by itself when the caller hangs up, which ends the input.
    let reader = tokio::spawn(async move {
        let mut lines = LineBuffer::new();
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(Some(n)) = recv.read(&mut buf).await {
            for line in lines.push(&buf[..n]) {
                if input.send(line).is_err() {
                    return;
                }
            }
        }
    });
    let mut hung_up = false;
    while let Some(chunk) = output_rx.recv().await {
        if send.write_all(chunk.as_bytes()).await.is_err() {
            hung_up = true;
            break;
        }
    }
    reader.abort();
    if !hung_up {
        // The service ended the session: let the caller see the stream end.
        let _ = send.finish();
        let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    }
    conn.close(0u32.into(), b"bye");
    Ok(())
}
