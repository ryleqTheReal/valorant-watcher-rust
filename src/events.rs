use tokio::sync::broadcast;

use crate::lockfile::Lockfile;

// every event that flows between the watchers and downstream services.
// grows as more services are added.
#[derive(Debug, Clone)]
pub enum Event {
    Startup,
    RsoLogin(Lockfile),
    RsoLogout,
    ValorantOpened(Lockfile),
    ValorantClosed,
    Shutdown,
}

// thin wrapper over a broadcast channel
// clone to hand out to tasks
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(128);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    // send fails only when there are no subscribers
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}
