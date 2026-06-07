use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, info, warn};

use crate::events::{Bus, Event};
use crate::lockfile::Lockfile;
use crate::paths;

const VALORANT_PROCESS_NAMES: &[&str] = &[
    "VALORANT-Win64-Shipping.exe",
    "VALORANT-Win64-Shipping",
    "VALORANT",
];

const RIOT_CLIENT_PROCESS_NAMES: &[&str] = &["RiotClientServices.exe", "RiotClientServices"];

// watches the lockfile and the rso-auth endpoint to detect login/logout
// the requests can be sent as soon as the RSO login succeeds
pub fn spawn_riot_client(bus: Bus, poll_interval: u64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut watcher = RiotClientWatcher::new();
        let mut rx = bus.subscribe();
        let mut ticker = interval(Duration::from_secs(poll_interval));
        loop {
            tokio::select! {
                _ = ticker.tick() => watcher.poll_once(&bus).await,
                ev = rx.recv() => {
                    if matches!(ev, Ok(Event::Shutdown)) {
                        break;
                    }
                }
            }
        }
        info!("riot client watcher stopped");
    })
}

// watches for the valorant process to detect game open/close
pub fn spawn_process(bus: Bus, poll_interval: u64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut watcher = ProcessWatcher::new();
        let mut rx = bus.subscribe();
        let mut ticker = interval(Duration::from_secs(poll_interval));
        loop {
            tokio::select! {
                _ = ticker.tick() => watcher.poll_once(&bus).await,
                ev = rx.recv() => {
                    if matches!(ev, Ok(Event::Shutdown)) {
                        break;
                    }
                }
            }
        }
        info!("process watcher stopped");
    })
}

struct RiotClientWatcher {
    logged_in: bool,
    last_content: String,
    lockfile: Option<Lockfile>,
    client: Option<reqwest::Client>,
}

impl RiotClientWatcher {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| warn!("failed to build local http client: {e}"))
            .ok();
        Self {
            logged_in: false,
            last_content: String::new(),
            lockfile: None,
            client,
        }
    }

    async fn poll_once(&mut self, bus: &Bus) {
        let path = match paths::lockfile_path() {
            Ok(p) => p,
            Err(e) => {
                warn!("cannot resolve lockfile path: {e}");
                return;
            }
        };

        // no lockfile means the riot client is not running
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c.trim().to_string(),
            Err(_) => {
                if self.logged_in {
                    self.transition_logout(bus);
                }
                self.lockfile = None;
                self.last_content.clear();
                return;
            }
        };

        if content != self.last_content {
            if !self.last_content.is_empty() {
                info!("lockfile changed, re-reading credentials");
            }
            self.last_content = content.clone();
            match Lockfile::parse(&content) {
                Ok(lf) => self.lockfile = Some(lf),
                Err(e) => {
                    debug!("lockfile not ready: {e}");
                    return;
                }
            }
            // credentials changed, force a fresh login check
            if self.logged_in {
                self.transition_logout(bus);
            }
        }

        let (Some(lockfile), Some(client)) = (self.lockfile.clone(), self.client.as_ref()) else {
            return;
        };

        let url = format!("{}/rso-auth/v1/authorization", lockfile.base_url());
        let response = client
            .get(&url)
            .header("Authorization", lockfile.auth_header())
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                if !self.logged_in {
                    self.transition_login(bus, lockfile);
                }
            }
            Ok(_) => {
                if self.logged_in {
                    self.transition_logout(bus);
                }
            }
            Err(_) => {
                // client shutting down or not ready yet
                if self.logged_in {
                    self.transition_logout(bus);
                }
            }
        }
    }

    fn transition_login(&mut self, bus: &Bus, lockfile: Lockfile) {
        self.logged_in = true;
        info!("rso login detected");
        bus.emit(Event::RsoLogin(lockfile));
    }

    fn transition_logout(&mut self, bus: &Bus) {
        self.logged_in = false;
        info!("rso logout detected");
        bus.emit(Event::RsoLogout);
    }
}

struct ProcessWatcher {
    system: System,
    valorant_was_running: bool,
}

impl ProcessWatcher {
    fn new() -> Self {
        Self {
            system: System::new(),
            valorant_was_running: false,
        }
    }

    async fn poll_once(&mut self, bus: &Bus) {
        let running = self.is_running(VALORANT_PROCESS_NAMES);

        if running && !self.valorant_was_running {
            info!("valorant process detected");
            match read_lockfile_with_retry(10, Duration::from_secs(1)).await {
                Some(lockfile) => bus.emit(Event::ValorantOpened(lockfile)),
                None => warn!("valorant is running but lockfile is unreadable, skipping"),
            }
        } else if !running && self.valorant_was_running {
            info!("valorant process terminated");
            bus.emit(Event::ValorantClosed);
        }

        self.valorant_was_running = running;
    }

    fn is_running(&mut self, names: &[&str]) -> bool {
        self.system
            .refresh_processes(ProcessesToUpdate::All, true);
        self.system.processes().values().any(|proc| {
            let name = proc.name().to_string_lossy();
            names.iter().any(|n| name == *n)
        })
    }
}

async fn read_lockfile_with_retry(retries: u32, delay: Duration) -> Option<Lockfile> {
    let path = paths::lockfile_path().ok()?;
    for attempt in 1..=retries {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 0 {
                match Lockfile::read(&path) {
                    Ok(lf) => {
                        info!("lockfile read successfully (attempt {attempt})");
                        return Some(lf);
                    }
                    Err(e) => debug!("lockfile not ready yet: {e}"),
                }
            }
        }
        if attempt < retries {
            sleep(delay).await;
        }
    }
    warn!("lockfile unreadable after {retries} attempts");
    None
}

#[allow(dead_code)]
fn is_riot_client_running(system: &mut System) -> bool {
    system.refresh_processes(ProcessesToUpdate::All, true);
    system.processes().values().any(|proc| {
        let name = proc.name().to_string_lossy();
        RIOT_CLIENT_PROCESS_NAMES.iter().any(|n| name == *n)
    })
}
