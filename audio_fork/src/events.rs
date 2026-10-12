//! Events of all forks fired by one task, in the order the forks queue them.
//!
//! Neither the bug's CLOSE nor the media thread waits for an event being created. A fork queues
//! nothing after its bug's CLOSE (`Shared::fire`), so none of its events can come after those of a
//! new fork with the same bugname.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::error;

use crate::fork::{Event, ForkId, Host, panic_message};

/// FreeSWITCH blocks a fire once its own dispatch queue is full (`switch_queue_push`, 10000 events
/// per dispatch thread); a server that sends events faster is slowed down much earlier here.
const CAPACITY: usize = 1024;

/// The sending end; cloned into every fork.
#[derive(Debug, Clone)]
pub struct EventQueue {
    tx: Sender<(ForkId, Event)>,
}

impl EventQueue {
    /// Spawns the task that fires the events on `runtime`; it ends with the runtime or once every
    /// clone of the queue is dropped.
    pub fn start(host: Arc<dyn Host>, runtime: &Handle) -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);
        runtime.spawn(fire_in_order(host, rx));
        EventQueue { tx }
    }

    /// Waits for room, so the socket task of a server that floods events stops reading from it.
    pub(crate) async fn fire(&self, fork: ForkId, event: Event) {
        // fails only once the runtime is shut down at unload, when there is nobody left to tell
        let _ = self.tx.send((fork, event)).await;
    }
}

async fn fire_in_order(host: Arc<dyn Host>, mut rx: Receiver<(ForkId, Event)>) {
    while let Some((fork, event)) = rx.recv().await {
        let subclass = event.subclass();
        // a panic would end this task and with it the events of every fork, so it costs only this one
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| host.fire(&fork, event))) {
            let message = panic_message(payload);
            error!(target: "crit", session = %fork.session, "{fork}: firing {} panicked: {message}", subclass.to_string_lossy());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::time;

    use super::*;

    #[tokio::test]
    async fn a_panicking_fire_loses_only_that_event() {
        let host = Arc::new(FirstFirePanics::default());
        let queue = EventQueue::start(host.clone(), &Handle::current());
        let fork = ForkId {
            session: "session".to_string(),
            bugname: "bug".to_string(),
        };
        queue.fire(fork.clone(), Event::Connect).await;
        queue.fire(fork, Event::KillAudio).await;
        for _ in 0..100 {
            if host.fired.lock().unwrap().len() == 2 {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            *host.fired.lock().unwrap(),
            vec![None, Some(Event::KillAudio)]
        );
    }

    /// Records every event it is asked to fire as `None` when it panicked on it.
    #[derive(Debug, Default)]
    struct FirstFirePanics {
        fired: Mutex<Vec<Option<Event>>>,
    }

    impl Host for FirstFirePanics {
        fn fire(&self, _fork: &ForkId, event: Event) {
            let mut fired = self.fired.lock().unwrap();
            if fired.is_empty() {
                fired.push(None);
                drop(fired);
                panic!("injected in fire");
            }
            fired.push(Some(event));
        }

        fn break_playback(&self, _session: &str) {
            unreachable!()
        }

        fn end(&self, _fork: &ForkId) {
            unreachable!()
        }
    }
}
