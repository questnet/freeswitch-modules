//! The socket task of one fork: connect, send the metadata, queued texts and capture audio,
//! receive, and close at CLOSE.

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use futures_util::{SinkExt, StreamExt};
use rtrb::Consumer;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::api::WsUrl;
use crate::fork::{Config, Event, Host, MAX_TEXT_LEN, Phase, Resample, Shared};
use crate::playout::Prebuffer;

const SUBPROTOCOL: &str = "audio.drachtio.org";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why the open connection ended.
#[derive(Debug)]
enum End {
    /// CLOSE ran: we close.
    Closed,
    /// The server closed or the connection broke.
    Dropped(String),
}

/// The task's half of playout: inbound audio is collected, resampled to the codec rate and handed to [`Shared::playout`].
pub(crate) struct Inbound {
    pub prebuffer: Prebuffer,
    pub resampler: Option<Box<dyn Resample>>,
}

pub(crate) async fn run(
    config: Config,
    shared: Arc<Shared>,
    host: Arc<dyn Host>,
    mut capture: Consumer<i16>,
    mut inbound: Option<Inbound>,
) {
    let id = &shared.id;
    let connected = tokio::select! {
        biased;
        _ = shared.closed.cancelled() => {
            // hung up or stopped while connecting: the connection attempt is dropped, nothing is sent
            tracing::debug!(session = %id.session, "{id}: closed while connecting");
            shared.state.lock().phase = Phase::Ended;
            return;
        }
        connected = tokio::time::timeout(config.timeouts.connect, connect(&config.url)) => connected,
    };
    let mut socket = match connected {
        Ok(Ok(socket)) => socket,
        Ok(Err(e)) => return connect_failed(&shared, &*host, e.to_string()),
        Err(_) => return connect_failed(&shared, &*host, "connect timed out".to_string()),
    };
    if shared.closed.is_cancelled() {
        // closed just as the handshake completed: nothing is sent, not even the queued texts
        tracing::debug!(session = %id.session, "{id}: closed while connecting");
        shared.state.lock().phase = Phase::Ended;
        let _ = tokio::time::timeout(config.timeouts.close, socket.close(None)).await;
        return;
    }
    shared.state.lock().phase = Phase::Open;
    shared.streaming.store(true, Ordering::Release);
    tracing::info!(session = %id.session, "{id}: connected to {}", config.url);
    shared.fire(&*host, Event::Connect);

    let end = stream(&mut socket, &shared, &*host, &mut capture, &mut inbound).await;
    shared.streaming.store(false, Ordering::Release);
    match end {
        End::Closed => {
            let flushed =
                tokio::time::timeout(config.timeouts.close, close(&mut socket, &shared)).await;
            shared.state.lock().phase = Phase::Ended;
            match flushed {
                Ok(Ok(())) => tracing::debug!(session = %id.session, "{id}: closed"),
                Ok(Err(e)) => {
                    tracing::info!(session = %id.session, "{id}: error while closing: {e}")
                }
                Err(_) => tracing::info!(session = %id.session, "{id}: close timed out"),
            }
        }
        End::Dropped(reason) => {
            shared.state.lock().phase = Phase::Ended;
            tracing::info!(session = %id.session, "{id}: connection dropped: {reason}");
            shared.fire(&*host, Event::Disconnect(None));
        }
    }
}

async fn connect(url: &WsUrl) -> Result<Socket, tungstenite::Error> {
    let mut request = url.uri().clone().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_TEXT_LEN))
        .max_frame_size(Some(MAX_TEXT_LEN));
    // the roots are only read for the first wss:// fork, never for ws://
    let tls = url.tls().then(tls);
    let (socket, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, Some(config), true, tls).await?;
    Ok(socket)
}

/// One TLS configuration for all `wss://` forks: the system's roots are read once, at the first
/// connect, instead of from disk on a runtime thread for every one.
fn tls() -> Connector {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let native = rustls_native_certs::load_native_certs();
        for e in &native.errors {
            tracing::warn!("loading the system's root certificates: {e}");
        }
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(native.certs);
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    Connector::Rustls(config.clone())
}

fn connect_failed(shared: &Shared, host: &dyn Host, reason: String) {
    let id = &shared.id;
    shared.state.lock().phase = Phase::Ended;
    tracing::info!(session = %id.session, "{id}: connection failed: {reason}");
    shared.fire(host, Event::ConnectFailed { reason });
}

/// Sends texts (in order, before any audio queued after them) and capture audio until CLOSE or the connection ends.
async fn stream(
    socket: &mut Socket,
    shared: &Shared,
    host: &dyn Host,
    capture: &mut Consumer<i16>,
    inbound: &mut Option<Inbound>,
) -> End {
    loop {
        loop {
            let Some(text) = shared.state.lock().pop() else {
                break;
            };
            if let Err(end) = send(socket, shared, Message::text(text)).await {
                return end;
            }
        }
        let available = capture.slots();
        if available > 0 {
            let chunk = capture.read_chunk(available).unwrap();
            let (a, b) = chunk.as_slices();
            let bytes: Vec<u8> = a.iter().chain(b).flat_map(|s| s.to_le_bytes()).collect();
            chunk.commit_all();
            if let Err(end) = send(socket, shared, Message::binary(bytes)).await {
                return end;
            }
        }
        tokio::select! {
            biased;
            _ = shared.closed.cancelled() => return End::Closed,
            _ = shared.wake.notified() => {
                #[cfg(feature = "test-hooks")]
                if shared.inject.task.swap(false, Ordering::AcqRel) {
                    panic!("injected in the socket task");
                }
            }
            message = socket.next() => match message {
                Some(Ok(Message::Close(frame))) => return End::Dropped(format!("closed by the server: {frame:?}")),
                Some(Ok(message)) => receive(shared, host, inbound, message),
                Some(Err(e)) => return End::Dropped(e.to_string()),
                None => return End::Dropped("connection closed".to_string()),
            },
        }
    }
}

/// A send that gives up at CLOSE, so a stalled server cannot keep the fork alive.
async fn send(socket: &mut Socket, shared: &Shared, message: Message) -> Result<(), End> {
    tokio::select! {
        biased;
        _ = shared.closed.cancelled() => Err(End::Closed),
        sent = socket.send(message) => sent.map_err(|e| End::Dropped(e.to_string())),
    }
}

fn receive(shared: &Shared, host: &dyn Host, inbound: &mut Option<Inbound>, message: Message) {
    let id = &shared.id;
    match message {
        Message::Text(text) => match parse(&text) {
            Request::Fire(event) => shared.fire(host, event),
            Request::KillAudio => {
                shared.fire(host, Event::KillAudio);
                host.break_playback(&id.session);
                if let (Some(inbound), Some(playout)) = (inbound, &shared.playout) {
                    inbound.prebuffer.reset();
                    playout.clear();
                }
            }
            Request::Ignored(why) => {
                tracing::info!(session = %id.session, "{id}: ignoring message: {why}")
            }
        },
        Message::Binary(bytes) => match (inbound, &shared.playout) {
            (Some(inbound), Some(playout)) => {
                if let Some(samples) = inbound.prebuffer.push(&bytes) {
                    match &mut inbound.resampler {
                        Some(resampler) => {
                            let mut resampled = Vec::with_capacity(samples.len());
                            resampler.process(&samples, &mut resampled);
                            playout.append(&resampled);
                        }
                        None => playout.append(&samples),
                    }
                }
            }
            _ => {
                tracing::debug!(session = %id.session, "{id}: ignoring {} bytes of audio without playout", bytes.len())
            }
        },
        // pings are answered by tungstenite; close frames end the stream before they get here
        _ => {}
    }
}

/// What a server text message asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Fire(Event),
    KillAudio,
    Ignored(String),
}

/// JSON with a `type`; one without counts as `json`, as in the C module.
fn parse(text: &str) -> Request {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return Request::Ignored("not JSON".to_string());
    };
    let data = || message.get("data").map(|d| d.to_string());
    match message
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("json")
    {
        "killAudio" => Request::KillAudio,
        "disconnect" => Request::Fire(Event::Disconnect(data())),
        "error" => Request::Fire(Event::Error(data())),
        "json" => Request::Fire(Event::Json(message.to_string())),
        other => Request::Ignored(format!("unsupported type {other}")),
    }
}

/// After CLOSE: the texts still queued (a final `stop` text among them), then the close frame.
async fn close(socket: &mut Socket, shared: &Shared) -> Result<(), tungstenite::Error> {
    loop {
        let Some(text) = shared.state.lock().pop() else {
            break;
        };
        socket.send(Message::text(text)).await?;
    }
    socket.close(None).await
}
