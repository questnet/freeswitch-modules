//! The socket task of one fork: connect, send the metadata, queued texts and capture audio,
//! receive, and close at CLOSE.

use std::sync::{Arc, atomic::Ordering};

use futures_util::{SinkExt, StreamExt};
use rtrb::Consumer;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        self, Message,
        client::IntoClientRequest,
        http::{HeaderValue, Uri},
        protocol::WebSocketConfig,
    },
};

use crate::fork::{Config, Event, Host, MAX_TEXT_LEN, Phase, Shared};

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

pub(crate) async fn run(
    config: Config,
    shared: Arc<Shared>,
    host: Arc<dyn Host>,
    mut capture: Consumer<i16>,
) {
    let id = &shared.id;
    let connected = tokio::time::timeout(config.timeouts.connect, connect(&config.url)).await;
    let mut socket = match connected {
        Ok(Ok(socket)) => socket,
        Ok(Err(e)) => return connect_failed(&shared, &*host, e.to_string()),
        Err(_) => return connect_failed(&shared, &*host, "connect timed out".to_string()),
    };
    if shared.closed.is_cancelled() {
        // stopped or hung up while connecting: nothing is sent, not even the queued texts
        tracing::debug!(session = %id.session, "{id}: closed while connecting");
        shared.outbox.lock().unwrap().phase = Phase::Ended;
        let _ = tokio::time::timeout(config.timeouts.close, socket.close(None)).await;
        return;
    }
    shared.outbox.lock().unwrap().phase = Phase::Open;
    shared.streaming.store(true, Ordering::Release);
    tracing::info!(session = %id.session, "{id}: connected to {}", config.url);
    host.fire(id, Event::Connect);

    let end = stream(&mut socket, &shared, &*host, &mut capture).await;
    shared.streaming.store(false, Ordering::Release);
    match end {
        End::Closed => {
            let flushed =
                tokio::time::timeout(config.timeouts.close, close(&mut socket, &shared)).await;
            shared.outbox.lock().unwrap().phase = Phase::Ended;
            match flushed {
                Ok(Ok(())) => tracing::debug!(session = %id.session, "{id}: closed"),
                Ok(Err(e)) => {
                    tracing::info!(session = %id.session, "{id}: error while closing: {e}")
                }
                Err(_) => tracing::info!(session = %id.session, "{id}: close timed out"),
            }
        }
        End::Dropped(reason) => {
            shared.outbox.lock().unwrap().phase = Phase::Ended;
            tracing::info!(session = %id.session, "{id}: connection dropped: {reason}");
            if !shared.closed.is_cancelled() {
                host.fire(id, Event::Disconnect(None));
            }
        }
    }
}

async fn connect(url: &Uri) -> Result<Socket, tungstenite::Error> {
    let mut request = url.clone().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_TEXT_LEN))
        .max_frame_size(Some(MAX_TEXT_LEN));
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(request, Some(config), true).await?;
    Ok(socket)
}

fn connect_failed(shared: &Shared, host: &dyn Host, reason: String) {
    let id = &shared.id;
    shared.outbox.lock().unwrap().phase = Phase::Ended;
    tracing::info!(session = %id.session, "{id}: connection failed: {reason}");
    if !shared.closed.is_cancelled() {
        host.fire(id, Event::ConnectFailed { reason });
    }
}

/// Sends texts (in order, before any audio queued after them) and capture audio until CLOSE or the connection ends.
async fn stream(
    socket: &mut Socket,
    shared: &Shared,
    host: &dyn Host,
    capture: &mut Consumer<i16>,
) -> End {
    loop {
        loop {
            let Some(text) = shared.outbox.lock().unwrap().pop() else {
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
            _ = shared.wake.notified() => {}
            message = socket.next() => match message {
                Some(Ok(Message::Close(frame))) => return End::Dropped(format!("closed by the server: {frame:?}")),
                Some(Ok(message)) => receive(shared, host, message),
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

fn receive(shared: &Shared, host: &dyn Host, message: Message) {
    let id = &shared.id;
    match message {
        Message::Text(text) => match inbound(&text) {
            Inbound::Fire(event) => host.fire(id, event),
            Inbound::KillAudio => {
                host.fire(id, Event::KillAudio);
                host.break_playback(&id.session);
            }
            Inbound::Ignored(why) => {
                tracing::info!(session = %id.session, "{id}: ignoring message: {why}")
            }
        },
        Message::Binary(bytes) => {
            tracing::debug!(session = %id.session, "{id}: ignoring {} bytes of audio without playout", bytes.len())
        }
        // pings are answered by tungstenite; close frames end the stream before they get here
        _ => {}
    }
}

/// What a server text message asks for.
#[derive(Debug, PartialEq, Eq)]
enum Inbound {
    Fire(Event),
    KillAudio,
    Ignored(String),
}

/// JSON with a `type`; one without counts as `json`, as in the C module.
fn inbound(text: &str) -> Inbound {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return Inbound::Ignored("not JSON".to_string());
    };
    let data = || message.get("data").map(|d| d.to_string());
    match message
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("json")
    {
        "killAudio" => Inbound::KillAudio,
        "disconnect" => Inbound::Fire(Event::Disconnect(data())),
        "error" => Inbound::Fire(Event::Error(data())),
        "json" => Inbound::Fire(Event::Json(message.to_string())),
        other => Inbound::Ignored(format!("unsupported type {other}")),
    }
}

/// After CLOSE: the texts still queued (a final `stop` text among them), then the close frame.
async fn close(socket: &mut Socket, shared: &Shared) -> Result<(), tungstenite::Error> {
    loop {
        let Some(text) = shared.outbox.lock().unwrap().pop() else {
            break;
        };
        socket.send(Message::text(text)).await?;
    }
    socket.close(None).await
}
