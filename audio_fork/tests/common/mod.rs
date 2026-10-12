//! The fake host and fork setup shared by the simulation tests.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio_fork::api::{Capture, CaptureRate, Mix};
use audio_fork::events::EventQueue;
use audio_fork::fork::{
    Config, Event, Fork, ForkHandle, ForkId, Host, MediaSide, Resamplers, Timeouts,
};
use tokio::{runtime::Handle, sync::Notify, time};

/// 20 ms at 8 kHz.
pub const FRAME: [i16; 160] = [7; 160];

/// Records what the core asks of the host.
#[derive(Debug, Default)]
pub struct FakeHost {
    events: Mutex<Vec<Event>>,
    pub breaks: Mutex<Vec<String>>,
    pub ends: Mutex<Vec<ForkId>>,
    fired: Notify,
}

impl Host for FakeHost {
    fn fire(&self, _fork: &ForkId, event: Event) {
        self.events.lock().unwrap().push(event);
        self.fired.notify_waiters();
    }

    fn break_playback(&self, session: &str) {
        self.breaks.lock().unwrap().push(session.to_string());
    }

    fn end(&self, fork: &ForkId) {
        self.ends.lock().unwrap().push(fork.clone());
    }
}

impl FakeHost {
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub async fn wait_for(&self, wanted: impl Fn(&Event) -> bool) -> Event {
        let wait = async {
            loop {
                let fired = self.fired.notified();
                if let Some(e) = self.events().into_iter().find(&wanted) {
                    return e;
                }
                fired.await;
            }
        };
        time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("event not fired; got {:?}", self.events()))
    }
}

pub fn start(
    url: &str,
    metadata: Option<&str>,
    timeouts: Timeouts,
) -> (MediaSide, ForkHandle, Arc<FakeHost>) {
    start_with(url, metadata, None, timeouts)
}

pub fn start_with(
    url: &str,
    metadata: Option<&str>,
    playout_rate: Option<u32>,
    timeouts: Timeouts,
) -> (MediaSide, ForkHandle, Arc<FakeHost>) {
    let host = Arc::new(FakeHost::default());
    let config = Config {
        id: ForkId {
            session: "session".to_string(),
            bugname: "bug".to_string(),
        },
        url: url.parse().unwrap(),
        capture: Some(Capture {
            mix: Mix::Mono,
            rate: CaptureRate::try_from(8000).unwrap(),
        }),
        codec_rate: 8000,
        playout_rate,
        timeouts,
    };
    let events = EventQueue::start(host.clone(), &Handle::current());
    let (media, handle) = Fork::start(
        config,
        metadata.map(str::to_string),
        host.clone(),
        &events,
        Resamplers::default(),
        &Handle::current(),
    )
    .unwrap();
    (media, handle, host)
}
