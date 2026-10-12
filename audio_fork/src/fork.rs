//! A fork of one call's audio to one WebSocket server, and the ports to the host.
//!
//! [`Fork::start`] spawns the socket task and returns the two sides the host holds: the
//! [`MediaSide`], owned by the media thread (the bug's user data), and the [`ForkHandle`] for
//! control from API calls. Neither holds a session; the task reaches the host only through
//! [`Host`], keyed by session UUID, so a hung-up call simply makes those calls no-ops.

use std::{
    any::Any,
    collections::VecDeque,
    ffi::CStr,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use rtrb::{Producer, RingBuffer};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_util::sync::CancellationToken;

use crate::{
    playout::{Playout, Prebuffer},
    socket,
};

/// The custom events a fork fires (all `SWITCH_EVENT_CUSTOM`, reserved on load).
pub const SUBCLASSES: [&CStr; 7] = [
    c"mod_audio_fork::connect",
    c"mod_audio_fork::connect_failed",
    c"mod_audio_fork::disconnect",
    c"mod_audio_fork::error",
    c"mod_audio_fork::json",
    c"mod_audio_fork::kill_audio",
    c"mod_audio_fork::buffer_overrun",
];

/// Longest text frame accepted, for the start metadata and each `send_text`; longer is an error, never truncated.
pub const MAX_TEXT_LEN: usize = 1024 * 1024;
const MAX_QUEUED_TEXTS: usize = 1024;
const MAX_QUEUED_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Capture buffered while the server is slow, in seconds of audio at the target rate.
const CAPTURE_BUFFER_SECS: usize = 2;

/// One fork: the call it is attached to and the bug's name, which tells several forks on one call apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForkId {
    pub session: String,
    pub bugname: String,
}

impl fmt::Display for ForkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.session, self.bugname)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connect,
    ConnectFailed {
        reason: String,
    },
    /// From the server's `disconnect` message (its `data`), or `None` when the connection dropped.
    Disconnect(Option<String>),
    /// From the server's `error` message (its `data`), or from a panic in the fork.
    Error(Option<String>),
    /// A server `json` message, whole.
    Json(String),
    KillAudio,
    BufferOverrun,
}

impl Event {
    pub fn subclass(&self) -> &'static CStr {
        match self {
            Event::Connect => SUBCLASSES[0],
            Event::ConnectFailed { .. } => SUBCLASSES[1],
            Event::Disconnect(_) => SUBCLASSES[2],
            Event::Error(_) => SUBCLASSES[3],
            Event::Json(_) => SUBCLASSES[4],
            Event::KillAudio => SUBCLASSES[5],
            Event::BufferOverrun => SUBCLASSES[6],
        }
    }

    pub fn body(&self) -> Option<String> {
        match self {
            Event::ConnectFailed { reason } => {
                Some(serde_json::json!({ "reason": reason }).to_string())
            }
            Event::Disconnect(body) | Event::Error(body) => body.clone(),
            Event::Json(body) => Some(body.clone()),
            Event::Connect | Event::KillAudio | Event::BufferOverrun => None,
        }
    }
}

/// What the core needs from the host, keyed by session UUID so a hung-up call makes it a no-op.
pub trait Host: Send + Sync + 'static {
    fn fire(&self, fork: &ForkId, event: Event);
    /// Interrupts whatever the channel is playing (`CF_BREAK`).
    fn break_playback(&self, session: &str);
    /// Removes the fork's bug after its task died; CLOSE follows as for a `stop`.
    fn end(&self, fork: &ForkId);
}

/// Converts interleaved samples between two rates; implemented by the host (testing T13).
pub trait Resample: Send {
    fn process(&mut self, input: &[i16], output: &mut Vec<i16>);
}

#[derive(Debug, Clone)]
pub struct Config {
    pub id: ForkId,
    pub url: Uri,
    pub channels: usize,
    /// The rate frames arrive at from the media thread.
    pub codec_rate: u32,
    /// The rate audio is sent to the server at.
    pub rate: u32,
    pub metadata: Option<String>,
    /// The rate the server sends audio at, when it is played to the caller (write-replace).
    pub playout_rate: Option<u32>,
    pub timeouts: Timeouts,
}

/// Resamplers from the host, each `None` where the rates match.
#[derive(Default)]
pub struct Resamplers {
    /// Codec rate to the rate sent to the server, interleaved for stereo.
    pub capture: Option<Box<dyn Resample>>,
    /// Server rate to the codec rate, mono.
    pub playout: Option<Box<dyn Resample>>,
}

impl fmt::Debug for Resamplers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resamplers")
            .field("capture", &self.capture.is_some())
            .field("playout", &self.playout.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For the TCP connect, TLS and WebSocket handshake together.
    pub connect: Duration,
    /// For sending the remaining texts and the close frame after CLOSE.
    pub close: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(10),
            close: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextError {
    TooLong,
    QueueFull,
    /// The connection failed, ended, or the fork is stopping.
    NotOpen,
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TextError::TooLong => "text exceeds 1 MB",
            TextError::QueueFull => "too many texts waiting to be sent",
            TextError::NotOpen => "connection is not open",
        })
    }
}

/// State shared by the media side, the control handle and the socket task.
#[derive(Debug)]
pub(crate) struct Shared {
    pub id: ForkId,
    pub outbox: Mutex<Outbox>,
    /// Wakes the task: a frame was pushed, a text queued, or the fork is closing.
    pub wake: Notify,
    /// Set by the task while frames can be sent; the media side drops frames otherwise, as there is nobody to send them to.
    pub streaming: AtomicBool,
    /// Cancelled at CLOSE; the task then flushes texts, sends the close frame and ends.
    pub closed: CancellationToken,
    /// Present when server audio is played to the caller.
    pub playout: Option<Playout>,
    #[cfg(feature = "test-hooks")]
    pub inject: Inject,
}

impl Shared {
    /// Fires unless CLOSE ran. Under the outbox lock, which CLOSE takes too, so no event can follow it.
    pub fn fire(&self, host: &dyn Host, event: Event) {
        let outbox = self.outbox.lock();
        if !outbox.closed {
            host.fire(&self.id, event);
        }
        drop(outbox);
    }

    pub fn is_closed(&self) -> bool {
        self.outbox.lock().closed
    }
}

/// Panics to inject for the containment tests (testing T9).
#[cfg(feature = "test-hooks")]
#[derive(Debug, Default)]
pub(crate) struct Inject {
    pub media: AtomicBool,
    pub task: AtomicBool,
}

#[cfg(feature = "test-hooks")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicIn {
    /// The next captured frame.
    Media,
    /// The socket task, when it next wakes.
    Task,
}

#[derive(Debug, Default)]
pub(crate) struct Outbox {
    pub phase: Phase,
    pub texts: VecDeque<String>,
    pub bytes: usize,
    /// No more texts are accepted: `stop` was called or CLOSE ran.
    pub closing: bool,
    /// CLOSE ran: the fork fires no more events.
    pub closed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    #[default]
    Connecting,
    Open,
    Ended,
}

impl Outbox {
    fn push(&mut self, text: String) -> Result<(), TextError> {
        if text.len() > MAX_TEXT_LEN {
            return Err(TextError::TooLong);
        }
        if self.texts.len() >= MAX_QUEUED_TEXTS || self.bytes + text.len() > MAX_QUEUED_TEXT_BYTES {
            return Err(TextError::QueueFull);
        }
        self.bytes += text.len();
        self.texts.push_back(text);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<String> {
        let text = self.texts.pop_front()?;
        self.bytes -= text.len();
        Some(text)
    }
}

#[derive(Debug)]
pub struct Fork;

impl Fork {
    /// Spawns the socket task on `runtime`. Fails only when the metadata is longer than [`MAX_TEXT_LEN`].
    pub fn start(
        config: Config,
        host: Arc<dyn Host>,
        resamplers: Resamplers,
        runtime: &Handle,
    ) -> Result<(MediaSide, ForkHandle), TextError> {
        let mut outbox = Outbox::default();
        if let Some(metadata) = config.metadata.clone() {
            outbox.push(metadata)?;
        }
        let shared = Arc::new(Shared {
            id: config.id.clone(),
            outbox: Mutex::new(outbox),
            wake: Notify::new(),
            streaming: AtomicBool::new(false),
            closed: CancellationToken::new(),
            playout: config.playout_rate.map(|_| Playout::default()),
            #[cfg(feature = "test-hooks")]
            inject: Inject::default(),
        });
        let inbound = config.playout_rate.map(|rate| socket::Inbound {
            prebuffer: Prebuffer::new(rate, config.codec_rate),
            resampler: resamplers.playout,
        });
        let capacity = config.rate as usize * config.channels * CAPTURE_BUFFER_SECS;
        let (producer, consumer) = RingBuffer::new(capacity);
        let task = runtime.spawn(socket::run(
            config,
            shared.clone(),
            host.clone(),
            consumer,
            inbound,
        ));
        runtime.spawn(supervise(task, shared.clone(), host.clone()));
        let media = MediaSide {
            shared: shared.clone(),
            host,
            runtime: runtime.clone(),
            capture: producer,
            resampler: resamplers.capture,
            resampled: Vec::new(),
            overrun_notified: false,
        };
        Ok((media, ForkHandle { shared }))
    }
}

/// A panic in the socket task ends that fork only (D4): logged at CRIT, `error` fired, bug removed.
async fn supervise(task: JoinHandle<()>, shared: Arc<Shared>, host: Arc<dyn Host>) {
    // the other way a task ends early is the runtime shutting down at unload
    let Err(e) = task.await else { return };
    if !e.is_panic() {
        return;
    }
    let message = panic_message(e.into_panic());
    let id = &shared.id;
    tracing::error!(target: "panic", session = %id.session, "{id}: socket task panicked: {message}");
    shared.fire(&*host, panic_event(&message));
    if !shared.is_closed() {
        host.end(id);
    }
}

pub fn panic_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map_or("(non-string panic payload)".to_string(), |s| s.to_string()),
    }
}

fn panic_event(message: &str) -> Event {
    Event::Error(Some(
        serde_json::json!({ "reason": format!("panic: {message}") }).to_string(),
    ))
}

/// The media thread's side of a fork; never blocks.
pub struct MediaSide {
    shared: Arc<Shared>,
    host: Arc<dyn Host>,
    runtime: Handle,
    capture: Producer<i16>,
    resampler: Option<Box<dyn Resample>>,
    resampled: Vec<i16>,
    overrun_notified: bool,
}

impl fmt::Debug for MediaSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaSide")
            .field("id", &self.shared.id)
            .finish_non_exhaustive()
    }
}

impl MediaSide {
    /// A captured frame at the codec rate, interleaved for stereo.
    pub fn on_read(&mut self, samples: &[i16]) {
        #[cfg(feature = "test-hooks")]
        if self.shared.inject.media.swap(false, Ordering::AcqRel) {
            panic!("injected in the media callback");
        }
        if !self.shared.streaming.load(Ordering::Acquire) {
            return;
        }
        let samples = match &mut self.resampler {
            Some(resampler) => {
                self.resampled.clear();
                resampler.process(samples, &mut self.resampled);
                &self.resampled[..]
            }
            None => samples,
        };
        // whole frames only: a partial one would shift the stereo channels
        match self.capture.write_chunk_uninit(samples.len()) {
            Ok(chunk) => {
                chunk.fill_from_iter(samples.iter().copied());
                self.shared.wake.notify_one();
            }
            Err(_) => self.overrun(),
        }
    }

    fn overrun(&mut self) {
        if self.overrun_notified {
            return;
        }
        self.overrun_notified = true;
        tracing::error!(session = %self.shared.id.session, "{}: capture buffer full, dropping frames", self.shared.id);
        // fired from the runtime: the media thread does not call into the host
        let (host, shared) = (self.host.clone(), self.shared.clone());
        self.runtime
            .spawn(async move { shared.fire(&*host, Event::BufferOverrun) });
    }

    /// After a panic in the media callback, which then has FreeSWITCH remove the bug: fires `error`.
    /// From the media thread itself, as the CLOSE that follows right away silences the fork.
    pub fn panicked(&self, message: &str) {
        self.shared.fire(&*self.host, panic_event(message));
    }

    /// Mixes server audio into a frame going to the caller; returns whether it changed the frame.
    /// Only with playout, whose write-replace bug is the only caller.
    pub fn on_write_replace(&mut self, frame: &mut [i16]) -> bool {
        let playout = self
            .shared
            .playout
            .as_ref()
            .expect("write-replace without playout");
        playout.mix_into(frame)
    }

    /// The bug's CLOSE: the task sends what is queued, closes the socket and ends.
    pub fn close(self) {
        let mut outbox = self.shared.outbox.lock();
        outbox.closing = true;
        outbox.closed = true;
        drop(outbox);
        self.shared.closed.cancel();
    }
}

/// Control of a fork from API calls; cloneable, usable from any thread.
#[derive(Debug, Clone)]
pub struct ForkHandle {
    shared: Arc<Shared>,
}

impl ForkHandle {
    #[cfg(feature = "test-hooks")]
    pub fn inject_panic(&self, place: PanicIn) {
        let flag = match place {
            PanicIn::Media => &self.shared.inject.media,
            PanicIn::Task => &self.shared.inject.task,
        };
        flag.store(true, Ordering::Release);
        self.shared.wake.notify_one();
    }

    /// Queues one text frame; texts sent before the connection is open go out right after the metadata.
    pub fn send_text(&self, text: String) -> Result<(), TextError> {
        let mut outbox = self.shared.outbox.lock();
        if outbox.closing || outbox.phase == Phase::Ended {
            return Err(TextError::NotOpen);
        }
        outbox.push(text)?;
        drop(outbox);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Accepts no more texts; `text` is the last frame before the close, if the connection is open.
    /// The fork ends when the host removes the bug (CLOSE).
    pub fn stop(&self, text: Option<String>) {
        let mut outbox = self.shared.outbox.lock();
        if let Some(text) = text.filter(|_| outbox.phase == Phase::Open && !outbox.closing) {
            // the connection closes right after; a final text over the limits is dropped like one sent too late
            if let Err(e) = outbox.push(text) {
                tracing::warn!(session = %self.shared.id.session, "{}: final text not sent: {e}", self.shared.id);
            }
        }
        outbox.closing = true;
        drop(outbox);
        self.shared.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_failed_body_is_escaped_json() {
        let reason = "HTTP error: 400 \"Bad\"\nRequest\\".to_string();
        let body = Event::ConnectFailed {
            reason: reason.clone(),
        }
        .body()
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["reason"], reason);
    }
}
