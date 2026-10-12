//! Scripted WebSocket server (testing T6): each connection is handed to the test before the
//! handshake, so it can accept, delay or refuse it, and then records what the client sends and
//! does what the test tells it (send, stall, close, drop the TCP connection).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::select;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

#[derive(Debug)]
pub struct Server {
    addr: SocketAddr,
    pending: mpsc::UnboundedReceiver<Pending>,
}

impl Server {
    pub async fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, pending) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if tx.send(Pending { stream }).is_err() {
                    break;
                }
            }
        });
        Server { addr, pending }
    }

    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// The next TCP connection, before the WebSocket handshake.
    pub async fn pending(&mut self) -> Pending {
        within(self.pending.recv()).await.expect("listener ended")
    }

    pub async fn accept(&mut self) -> Conn {
        self.pending().await.accept().await
    }

    /// Whether a connection arrives within `wait`.
    pub async fn connects_within(&mut self, wait: Duration) -> bool {
        time::timeout(wait, self.pending.recv()).await.is_ok()
    }
}

#[derive(Debug)]
pub struct Pending {
    stream: TcpStream,
}

impl Pending {
    /// Completes the handshake, selecting the subprotocol the client asked for.
    // the callback's error type is tungstenite's
    #[allow(clippy::result_large_err)]
    pub async fn accept(self) -> Conn {
        let mut protocol = None;
        let mut path = String::new();
        let callback = |request: &Request, mut response: Response| {
            path = request.uri().path().to_string();
            protocol = request
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|p| p.to_str().ok())
                .map(str::to_string);
            if let Some(p) = &protocol {
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_str(p).unwrap());
            }
            Ok(response)
        };
        // large enough for the oversized messages the receive-limit tests send
        let config = WebSocketConfig::default()
            .max_message_size(Some(16 << 20))
            .max_frame_size(Some(16 << 20));
        let ws =
            tokio_tungstenite::accept_hdr_async_with_config(self.stream, callback, Some(config))
                .await
                .expect("websocket handshake");
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(ws, frames_tx, commands_rx));
        Conn {
            protocol,
            path,
            frames,
            commands,
        }
    }

    /// Like [`Pending::accept`], but `None` when the client gave up during the handshake.
    #[allow(clippy::result_large_err)]
    pub async fn try_accept(self) -> Option<Conn> {
        let callback = |_: &Request, response: Response| Ok(response);
        let ws = tokio_tungstenite::accept_hdr_async(self.stream, callback)
            .await
            .ok()?;
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(ws, frames_tx, commands_rx));
        Some(Conn {
            protocol: None,
            path: String::new(),
            frames,
            commands,
        })
    }

    /// Closes the TCP connection without a handshake.
    pub fn refuse(self) {}
}

/// A server that accepts every connection (after `handshake_delay`), discards what it receives
/// and counts the connections still open, for the hangup races and the load tier.
#[derive(Debug, Clone)]
pub struct Sink {
    addr: SocketAddr,
    open: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
}

impl Sink {
    pub async fn start(handshake_delay: Duration) -> Sink {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink = Sink {
            addr: listener.local_addr().unwrap(),
            open: Arc::default(),
            accepted: Arc::default(),
        };
        let counters = sink.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(counters.clone().drain(stream, handshake_delay));
            }
        });
        sink
    }

    // the callback's error type is tungstenite's
    #[allow(clippy::result_large_err)]
    async fn drain(self, stream: TcpStream, handshake_delay: Duration) {
        time::sleep(handshake_delay).await;
        let callback = |request: &Request, mut response: Response| {
            if let Some(p) = request.headers().get("Sec-WebSocket-Protocol") {
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", p.clone());
            }
            Ok(response)
        };
        // the client may have given up during the delay
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
            return;
        };
        self.accepted.fetch_add(1, Ordering::SeqCst);
        self.open.fetch_add(1, Ordering::SeqCst);
        while let Some(Ok(_)) = ws.next().await {}
        self.open.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// Connections whose handshake completed and that the client has not closed yet.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// A server that accepts every connection, discards what it receives and streams back a running
/// counter (from 1) in real time, 20 ms per frame: playout for the load tier. Remembers when each
/// path started streaming.
#[derive(Debug, Clone)]
pub struct Streamer {
    addr: SocketAddr,
    started: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Streamer {
    pub async fn start(rate: u32) -> Streamer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let streamer = Streamer {
            addr: listener.local_addr().unwrap(),
            started: Arc::default(),
        };
        let started = streamer.started.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(stream_counter(stream, rate, started.clone()));
            }
        });
        streamer
    }

    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// When the connection to `path` sent its first frame.
    pub fn started(&self, path: &str) -> Option<Instant> {
        self.started.lock().unwrap().get(path).copied()
    }
}

// the callback's error type is tungstenite's
#[allow(clippy::result_large_err)]
async fn stream_counter(
    stream: TcpStream,
    rate: u32,
    started: Arc<Mutex<HashMap<String, Instant>>>,
) {
    let mut path = String::new();
    let callback = |request: &Request, mut response: Response| {
        path = request.uri().path().to_string();
        if let Some(p) = request.headers().get("Sec-WebSocket-Protocol") {
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", p.clone());
        }
        Ok(response)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let (mut tx, mut rx) = ws.split();
    tokio::spawn(async move { while let Some(Ok(_)) = rx.next().await {} });
    let per_frame = rate as usize / 50;
    let mut next: u16 = 1;
    let mut tick = time::interval(Duration::from_millis(20));
    started.lock().unwrap().insert(path, Instant::now());
    loop {
        tick.tick().await;
        let frame: Vec<u8> = (0..per_frame)
            .flat_map(|_| {
                let s = next;
                next = next.wrapping_add(1);
                s.to_le_bytes()
            })
            .collect();
        if tx.send(Message::binary(frame)).await.is_err() {
            return;
        }
    }
}

/// What the client sent, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

#[derive(Debug)]
enum Command {
    Send(Message),
    Close,
    Drop,
    Stall,
}

#[derive(Debug)]
pub struct Conn {
    pub protocol: Option<String>,
    pub path: String,
    frames: mpsc::UnboundedReceiver<Frame>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Conn {
    /// The next frame the client sent; `None` once the connection is gone.
    pub async fn next(&mut self) -> Option<Frame> {
        within(self.frames.recv()).await
    }

    /// The next text frame, skipping audio.
    pub async fn next_text(&mut self) -> Option<String> {
        loop {
            match self.next().await? {
                Frame::Text(t) => return Some(t),
                Frame::Binary(_) => {}
                Frame::Close => return None,
            }
        }
    }

    /// Waits for at least `bytes` of audio and returns all audio received meanwhile.
    pub async fn audio(&mut self, bytes: usize) -> Vec<u8> {
        let mut audio = Vec::new();
        while audio.len() < bytes {
            match self.next().await {
                Some(Frame::Binary(b)) => audio.extend(b),
                Some(Frame::Text(_)) => {}
                other => panic!(
                    "connection ended after {} audio bytes: {other:?}",
                    audio.len()
                ),
            }
        }
        audio
    }

    pub fn send_text(&self, text: impl Into<String>) {
        self.command(Command::Send(Message::text(text.into())));
    }

    pub fn send_binary(&self, bytes: Vec<u8>) {
        self.command(Command::Send(Message::binary(bytes)));
    }

    /// Sends a close frame.
    pub fn close(&self) {
        self.command(Command::Close);
    }

    /// Drops the TCP connection without a close frame.
    pub fn drop_tcp(&self) {
        self.command(Command::Drop);
    }

    /// Stops reading, so the client's sends eventually block.
    pub fn stall(&self) {
        self.command(Command::Stall);
    }

    fn command(&self, command: Command) {
        // the connection task has ended when the client went away; that is what the test then sees
        let _ = self.commands.send(command);
    }
}

async fn serve(
    mut ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    frames: mpsc::UnboundedSender<Frame>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut stalled = false;
    loop {
        select! {
            command = commands.recv() => match command {
                Some(Command::Send(m)) => {
                    if ws.send(m).await.is_err() {
                        return;
                    }
                }
                Some(Command::Close) => {
                    let _ = ws.close(None).await;
                }
                Some(Command::Drop) | None => return,
                Some(Command::Stall) => stalled = true,
            },
            message = ws.next(), if !stalled => {
                let frame = match message {
                    Some(Ok(Message::Text(t))) => Frame::Text(t.to_string()),
                    Some(Ok(Message::Binary(b))) => Frame::Binary(b.to_vec()),
                    Some(Ok(Message::Close(_))) => Frame::Close,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return,
                };
                let _ = frames.send(frame);
            }
        }
    }
}

/// Every wait in a test is bounded, so a missing frame fails the test instead of hanging it.
async fn within<T>(f: impl Future<Output = T>) -> T {
    time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out waiting for the client")
}
