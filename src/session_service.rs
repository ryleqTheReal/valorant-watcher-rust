use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::interval;
use tracing::{info, warn};
use uuid::Uuid;

use crate::backend::Backend;
use crate::events::{Bus, Event};

const PING_INTERVAL: Duration = Duration::from_secs(30);

// maintains an app-scoped backend session: opens on the first reported state,
// pings every 30s, reports each state transition, reopens on 404/410, and ends
// on shutdown.
pub async fn run(bus: Bus, backend: Backend) {
    let mut rx = bus.subscribe();
    let mut service = SessionService::new(backend);
    let mut ping = interval(PING_INTERVAL);
    ping.tick().await; // consume the immediate first tick

    // report startup directly; the bus Startup event may fire before we subscribe
    service.send_state("STARTUP").await;

    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(Event::Shutdown) => {
                    service.end_session().await;
                    break;
                }
                Ok(event) => {
                    if let Some(state) = map_state(&event) {
                        service.send_state(state).await;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            _ = ping.tick() => service.ping().await,
        }
    }
    info!("session service stopped");
}

struct SessionService {
    backend: Backend,
    client: reqwest::Client,
    session_id: Option<String>,
    last_state: Option<String>,
    pending: Vec<String>,
}

impl SessionService {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            client: reqwest::Client::new(),
            session_id: None,
            last_state: None,
            pending: Vec::new(),
        }
    }

    async fn send_state(&mut self, state: &str) {
        if self.session_id.is_none() && !self.ensure_session().await {
            self.pending.push(state.to_string());
            return;
        }
        self.post_state(state).await;
    }

    // open the session if needed, then flush any states queued while offline
    async fn ensure_session(&mut self) -> bool {
        if self.session_id.is_some() {
            return true;
        }
        if !self.open_session().await {
            return false;
        }
        for state in std::mem::take(&mut self.pending) {
            self.post_state(&state).await;
        }
        true
    }

    async fn post_state(&mut self, state: &str) {
        match self.post_event(state).await {
            EventResult::Sent => self.last_state = Some(state.to_string()),
            EventResult::Lost => {
                info!("backend session lost, reopening");
                self.session_id = None;
                if self.open_session().await && self.post_event(state).await == EventResult::Sent {
                    self.last_state = Some(state.to_string());
                }
            }
            EventResult::Failed => {}
        }
    }

    async fn post_event(&self, state: &str) -> EventResult {
        let (Some(token), Some(session_id)) =
            (self.backend.app_access_token().await, self.session_id.clone())
        else {
            return EventResult::Failed;
        };

        let body = json!({
            "state": state,
            "timestamp_unix_ms": now_ms(),
            "event_id": Uuid::new_v4().to_string(),
        });

        let url = format!("{}/v1/sessions/{session_id}/event", self.backend.base_url());
        match self.client.post(url).bearer_auth(token).json(&body).send().await {
            Ok(response) if response.status() == StatusCode::NO_CONTENT => EventResult::Sent,
            Ok(response) if is_session_lost(response.status()) => EventResult::Lost,
            Ok(response) => {
                warn!("session event returned {}", response.status());
                EventResult::Failed
            }
            Err(e) => {
                warn!("session event failed: {e}");
                EventResult::Failed
            }
        }
    }

    async fn open_session(&mut self) -> bool {
        let Some(token) = self.backend.app_access_token().await else {
            return false;
        };

        let url = format!("{}/v1/sessions", self.backend.base_url());
        let response = match self.client.post(url).bearer_auth(token).send().await {
            Ok(response) => response,
            Err(e) => {
                warn!("failed to open session: {e}");
                return false;
            }
        };

        if response.status() != StatusCode::CREATED {
            warn!("POST /v1/sessions returned {}", response.status());
            return false;
        }

        let body = response.text().await.unwrap_or_default();
        match extract_session_id(&body) {
            Some(session_id) => {
                info!("backend session opened: {session_id}");
                self.session_id = Some(session_id);
                true
            }
            None => {
                warn!("session response missing session_id");
                false
            }
        }
    }

    async fn ping(&mut self) {
        let (Some(token), Some(session_id)) =
            (self.backend.app_access_token().await, self.session_id.clone())
        else {
            return;
        };

        let url = format!("{}/v1/sessions/{session_id}/ping", self.backend.base_url());
        match self.client.post(url).bearer_auth(token).send().await {
            Ok(response) if is_session_lost(response.status()) => {
                info!("session lost during ping, reopening");
                self.session_id = None;
                if self.open_session().await {
                    if let Some(last) = self.last_state.clone() {
                        self.post_state(&last).await;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => warn!("session ping failed: {e}"),
        }
    }

    async fn end_session(&mut self) {
        let Some(session_id) = self.session_id.take() else {
            return;
        };
        if let Some(token) = self.backend.app_access_token().await {
            let url = format!("{}/v1/sessions/{session_id}/end", self.backend.base_url());
            let _ = self.client.post(url).bearer_auth(token).send().await;
        }
        self.last_state = None;
        info!("backend session ended: {session_id}");
    }
}

#[derive(PartialEq, Eq)]
enum EventResult {
    Sent,
    Lost,
    Failed,
}

// startup is reported explicitly on launch, so it is not mapped here
fn map_state(event: &Event) -> Option<&'static str> {
    match event {
        Event::RsoLogin(_) => Some("RSO_LOGIN"),
        Event::RsoLogout => Some("RSO_LOGOUT"),
        Event::ValorantOpened(_) => Some("VALORANT_OPENED"),
        Event::ValorantClosed => Some("VALORANT_CLOSED"),
        Event::PregameStarted => Some("PREGAME_STARTED"),
        Event::PregameEnded => Some("PREGAME_ENDED"),
        Event::MatchStarted => Some("MATCH_STARTED"),
        Event::MatchEnded => Some("MATCH_ENDED"),
        Event::Startup | Event::Shutdown => None,
    }
}

fn is_session_lost(status: StatusCode) -> bool {
    status == StatusCode::NOT_FOUND || status == StatusCode::GONE
}

fn extract_session_id(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("session_id")? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
