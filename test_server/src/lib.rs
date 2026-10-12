//! Scripted WebSocket server (testing T6): each connection is handed to the test before the
//! handshake, so it can accept, delay or refuse it, and then records what the client sends and
//! does what the test tells it (send, stall, close, drop the TCP connection).

use std::{net::SocketAddr, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
    http::HeaderValue,
    protocol::WebSocketConfig,
};

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
        tokio::time::timeout(wait, self.pending.recv())
            .await
            .is_ok()
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

    /// Closes the TCP connection without a handshake.
    pub fn refuse(self) {}
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
        tokio::select! {
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
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out waiting for the client")
}
