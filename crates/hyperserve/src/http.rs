//! The HTTP session of hyperb's `SPEC.md` (section 2, detailed in
//! `hyperb_core::http`): the renderer POSTs its `hello` with its key, gets a
//! session id, opens an event stream signed with its key, and POSTs every
//! later line. Each server line is one SSE event named by its tag.
//!
//! Renderers use `https` for any host that is not `localhost` or an IP
//! address, so a public server either terminates TLS itself (`--tls-cert`
//! and `--tls-key`) or sits behind a proxy that does.
//!
//! A GET from a web browser gets a page that points to the app in hyperb.

use crate::Session;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::stream::{self, Stream, StreamExt};
use hyperb_core::identity::PublicKey;
use hypereffect::LineBuffer;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

// The headers of `hyperb_core::http`, which is only built with the renderers'
// HTTP client.
/// The renderer's public key, as hex.
const KEY: &str = "hyperb-key";
/// The session id the server gave in answer to the hello.
const SESSION: &str = "hyperb-session";
/// The renderer's signature of the session id, as hex.
const SIGNATURE: &str = "hyperb-signature";

/// How long a session waits for its event stream after the hello.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the certificate is read again, to pick up renewals.
const TLS_RELOAD: Duration = Duration::from_secs(12 * 60 * 60);

/// Where the certificate is, to serve HTTPS. Without one the server speaks
/// plain HTTP, which renderers only use for `localhost` and IP addresses.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct Tls {
    /// The certificate chain, PEM. Read again every 12 hours.
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// The certificate's private key, PEM.
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
}

/// Serves the app named `name` at every path of `listen`, running `service`
/// for each session.
pub async fn serve<F, Fut>(
    name: &str,
    listen: SocketAddr,
    tls: &Tls,
    service: F,
) -> Result<(), String>
where
    F: Fn(Session) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(listen).map_err(|e| format!("{listen}: {e}"))?;
    serve_on(name, listener, tls, service).await
}

/// [`serve`] on a socket that is already bound.
pub async fn serve_on<F, Fut>(
    name: &str,
    listener: TcpListener,
    tls: &Tls,
    service: F,
) -> Result<(), String>
where
    F: Fn(Session) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listen = listener.local_addr().map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let headers = [KEY, SESSION, SIGNATURE].map(HeaderName::from_static);
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([
            headers[0].clone(),
            headers[1].clone(),
            headers[2].clone(),
            header::CONTENT_TYPE,
        ])
        .expose_headers([headers[1].clone()]);
    let start: Start = Arc::new(move |s| Box::pin(service(s)));
    let state = Arc::new(Shared {
        name: name.to_owned(),
        start,
        sessions: Mutex::default(),
    });
    let app = Router::new()
        .route("/", get(stream).post(post))
        .route("/{*path}", get(stream).post(post))
        .layer(cors)
        .with_state(state)
        .into_make_service();
    match (&tls.tls_cert, &tls.tls_key) {
        (Some(cert), Some(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let config = RustlsConfig::from_pem_file(cert, key)
                .await
                .map_err(|e| format!("{}: {e}", cert.display()))?;
            let reload = config.clone();
            let (cert, key) = (cert.clone(), key.clone());
            tokio::spawn(async move {
                let mut timer = tokio::time::interval(TLS_RELOAD);
                timer.tick().await;
                loop {
                    timer.tick().await;
                    if let Err(e) = reload.reload_from_pem_file(&cert, &key).await {
                        eprintln!("reloading {}: {e}", cert.display());
                    }
                }
            });
            println!("{name} answers on https://{listen}");
            axum_server::tls_rustls::from_tcp_rustls(listener, config)
                .map_err(|e| e.to_string())?
                .serve(app)
                .await
        }
        _ => {
            println!("{name} answers on http://{listen}");
            axum_server::from_tcp(listener)
                .map_err(|e| e.to_string())?
                .serve(app)
                .await
        }
    }
    .map_err(|e| format!("{listen}: {e}"))
}

type Start =
    Arc<dyn Fn(Session) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct Shared {
    name: String,
    start: Start,
    sessions: Mutex<HashMap<String, Pending>>,
}

/// A session, as the requests that name it see it.
struct Pending {
    key: PublicKey,
    cmds: mpsc::UnboundedSender<Cmd>,
    streaming: bool,
}

enum Cmd {
    /// Lines from the renderer.
    Input(String),
    /// The event stream opened; send everything there.
    Stream(mpsc::UnboundedSender<String>),
}

/// A hello (no session yet) or a line for a session.
async fn post(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    if headers.contains_key(SESSION) {
        let mut map = shared.sessions.lock().unwrap();
        return match authorize(&mut map, &headers) {
            Ok(s) if s.cmds.send(Cmd::Input(body)).is_ok() => {
                StatusCode::NO_CONTENT.into_response()
            }
            Ok(_) => (StatusCode::GONE, "session ended").into_response(),
            Err(r) => r.into_response(),
        };
    }
    let Some(key) = header_str(&headers, KEY).and_then(|k| k.parse::<PublicKey>().ok()) else {
        return (StatusCode::BAD_REQUEST, format!("missing or invalid {KEY}")).into_response();
    };
    let id = session_id();
    let (cmds, cmds_rx) = mpsc::unbounded_channel();
    let (input, input_rx) = mpsc::unbounded_channel();
    let (output, output_rx) = mpsc::unbounded_channel();
    shared.sessions.lock().unwrap().insert(
        id.clone(),
        Pending {
            key,
            cmds: cmds.clone(),
            streaming: false,
        },
    );
    let _ = cmds.send(Cmd::Input(body));
    tokio::spawn((shared.start)(Session {
        key,
        origin: format!("hyperb://{}", host(&uri, &headers)),
        path: uri.path().to_owned(),
        input: input_rx,
        output,
    }));
    let (shared, sid) = (shared.clone(), id.clone());
    tokio::spawn(async move {
        pump(cmds_rx, input, output_rx).await;
        shared.sessions.lock().unwrap().remove(&sid);
    });
    ([(SESSION, id)], StatusCode::OK).into_response()
}

/// Carries a session's lines between its requests and its service, until
/// either side is gone. Output from before the stream opened waits for it.
async fn pump(
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    input: mpsc::UnboundedSender<String>,
    mut output: mpsc::UnboundedReceiver<String>,
) {
    let mut lines = LineBuffer::new();
    let mut backlog = String::new();
    let mut stream: Option<mpsc::UnboundedSender<String>> = None;
    let deadline = tokio::time::sleep(STREAM_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Input(mut body)) => {
                    if !body.ends_with('\n') {
                        body.push('\n');
                    }
                    for line in lines.push(body.as_bytes()) {
                        let _ = input.send(line);
                    }
                }
                Some(Cmd::Stream(tx)) => {
                    if !backlog.is_empty() && tx.send(std::mem::take(&mut backlog)).is_err() {
                        break;
                    }
                    stream = Some(tx);
                }
                None => break,
            },
            chunk = output.recv() => match (chunk, &stream) {
                (None, _) => break,
                (Some(c), Some(tx)) => if tx.send(c).is_err() {
                    break;
                },
                (Some(c), None) => backlog.push_str(&c),
            },
            _ = &mut deadline, if stream.is_none() => break,
            _ = async { stream.as_ref().unwrap().closed().await }, if stream.is_some() => break,
        }
    }
}

/// Opens a session's event stream, or shows a web browser where to go.
async fn stream(State(shared): State<Arc<Shared>>, uri: Uri, headers: HeaderMap) -> Response {
    let wants_events = header_str(&headers, header::ACCEPT.as_str())
        .is_some_and(|a| a.contains("text/event-stream"));
    if !wants_events {
        return Html(page(&shared.name, host(&uri, &headers), uri.path())).into_response();
    }
    let mut map = shared.sessions.lock().unwrap();
    let s = match authorize(&mut map, &headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    if s.streaming {
        return (StatusCode::CONFLICT, "the session already has a stream").into_response();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    if s.cmds.send(Cmd::Stream(tx)).is_err() {
        return (StatusCode::GONE, "session ended").into_response();
    }
    s.streaming = true;
    Sse::new(events(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// One SSE event per line, named by its tag.
fn events(
    rx: mpsc::UnboundedReceiver<String>,
) -> impl Stream<Item = Result<sse::Event, Infallible>> {
    stream::unfold(rx, |mut rx| async move { Some((rx.recv().await?, rx)) }).flat_map(|chunk| {
        let events: Vec<_> = chunk
            .lines()
            .filter_map(|line| line.split_once(' ').filter(|(tag, _)| !tag.is_empty()))
            .map(|(tag, json)| Ok(sse::Event::default().event(tag).data(json)))
            .collect();
        stream::iter(events)
    })
}

/// The session the auth headers name, if they are valid for it.
fn authorize<'a>(
    map: &'a mut HashMap<String, Pending>,
    headers: &HeaderMap,
) -> Result<&'a mut Pending, (StatusCode, &'static str)> {
    let deny = |why| Err((StatusCode::UNAUTHORIZED, why));
    let id = header_str(headers, SESSION).unwrap_or_default();
    let Some(s) = map.get_mut(id) else {
        return deny("unknown session");
    };
    let key = header_str(headers, KEY).and_then(|k| k.parse::<PublicKey>().ok());
    if key != Some(s.key) {
        return deny("the session belongs to another key");
    }
    if !s.key.verify(
        id.as_bytes(),
        header_str(headers, SIGNATURE).unwrap_or_default(),
    ) {
        return deny("bad signature");
    }
    Ok(s)
}

/// The page a web browser gets: where to open the app.
fn page(name: &str, host: &str, path: &str) -> String {
    let address = format!("hyperb://{host}{}", if path == "/" { "" } else { path });
    let web = format!(
        "https://hypereffect.github.io/hyperb/?link={}",
        percent(&address)
    );
    let (name, address) = (escape(name), escape(&address));
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>{name}</title>\
         <body style=\"font-family:system-ui,sans-serif;max-width:36em;margin:3em auto;padding:0 1em\">\
         <h1>{name}</h1>\
         <p>This is a <a href=\"https://github.com/hypereffect/hyperb\">hyperb</a> app. \
         Open <a href=\"{address}\"><code>{address}</code></a> in hyperb, \
         or <a href=\"{web}\">in the web version</a>.</p>"
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The host the request was sent to, with its port if it had one.
fn host<'a>(uri: &'a Uri, headers: &'a HeaderMap) -> &'a str {
    // HTTP/2 has no Host header, only the authority of the URI.
    header_str(headers, header::HOST.as_str())
        .or(uri.authority().map(|a| a.as_str()))
        .unwrap_or("localhost")
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn session_id() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system random number generator");
    hex::encode(bytes)
}
