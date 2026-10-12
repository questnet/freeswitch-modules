//! Events of all forks fired by one task, in the order the forks queue them.
//!
//! Queueing never waits, so neither CLOSE nor the media thread waits for an event being created. A
//! fork queues nothing after its CLOSE (`Shared::fire`), so none of its events can come after those
//! of a new fork with the same bugname.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::fork::{Event, ForkId, Host};

/// The sending end; cloned into every fork.
#[derive(Debug, Clone)]
pub struct EventQueue {
    tx: UnboundedSender<(ForkId, Event)>,
}

impl EventQueue {
    /// Spawns the task that fires the events on `runtime`; it ends with the runtime or once every
    /// clone of the queue is dropped.
    pub fn start(host: Arc<dyn Host>, runtime: &Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        runtime.spawn(fire_in_order(host, rx));
        EventQueue { tx }
    }

    pub(crate) fn fire(&self, fork: ForkId, event: Event) {
        // fails only once the runtime is shut down at unload, when there is nobody left to tell
        let _ = self.tx.send((fork, event));
    }
}

async fn fire_in_order(host: Arc<dyn Host>, mut rx: UnboundedReceiver<(ForkId, Event)>) {
    while let Some((fork, event)) = rx.recv().await {
        host.fire(&fork, event);
    }
}
