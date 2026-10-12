//! Events of all forks fired by one task, in the order the forks queue them.
//!
//! Neither the bug's CLOSE nor the media thread waits for an event being created. A fork queues
//! nothing after its bug's CLOSE (`Shared::fire`), so none of its events can come after those of a new fork with the
//! same bugname.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::fork::{Event, ForkId, Host};

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
        host.fire(&fork, event);
    }
}
