use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{debug, info, warn};

use crate::backend::Backend;
use crate::config::Config;
use crate::dedup::{self, Marker};
use crate::events::{Bus, Event};
use crate::lockfile::Lockfile;
use crate::session::{Api, Session};

// wamp v1 subscribe to the presence topic, [5, topic]
const SUBSCRIBE_PRESENCE: &str = r#"[5,"OnJsonApiEvent_chat_v4_presences"]"#;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// listen to the local riot client websocket and drive pregame/ingame collection
// off the player's own sessionLoopState. reconnects until the task is aborted.
pub async fn run(lockfile: Lockfile, session: Arc<Session>, backend: Backend, bus: Bus, cfg: Config) {
    loop {
        if let Err(e) = connect_and_listen(&lockfile, &session, &backend, &bus, &cfg).await {
            debug!("websocket not ready ({e}), reconnecting in 3s");
        }
        sleep(Duration::from_secs(3)).await;
    }
}

async fn connect_and_listen(
    lockfile: &Lockfile,
    session: &Arc<Session>,
    backend: &Backend,
    bus: &Bus,
    cfg: &Config,
) -> Result<(), BoxError> {
    let connector = Connector::NativeTls(
        native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()?,
    );

    let mut request = lockfile.wss_url().into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, HeaderValue::from_str(&lockfile.auth_header())?);

    let (mut ws, _) =
        connect_async_tls_with_config(request, None, false, Some(connector)).await?;
    ws.send(Message::Text(SUBSCRIBE_PRESENCE.into())).await?;
    info!("connected to riot client websocket on port {}", lockfile.port);

    let mut tracker = Tracker::new(session.clone(), backend.clone(), bus.clone(), cfg.clone());

    while let Some(message) = ws.next().await {
        match message? {
            Message::Text(text) => tracker.on_message(text.as_str()).await,
            Message::Ping(payload) => ws.send(Message::Pong(payload)).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

// tracks the player's sessionLoopState and starts/stops collection on transitions
struct Tracker {
    session: Arc<Session>,
    backend: Backend,
    bus: Bus,
    cfg: Config,
    state: Option<String>,
    pregame: Option<JoinHandle<()>>,
    ingame_loadout: Option<JoinHandle<()>>,
}

impl Tracker {
    fn new(session: Arc<Session>, backend: Backend, bus: Bus, cfg: Config) -> Self {
        Self {
            session,
            backend,
            bus,
            cfg,
            state: None,
            pregame: None,
            ingame_loadout: None,
        }
    }

    async fn on_message(&mut self, text: &str) {
        let puuid = self.session.puuid().await;
        let Some(new_state) = extract_state(text, &puuid) else {
            return;
        };
        if self.state.as_deref() == Some(new_state.as_str()) {
            return;
        }

        // announce the end of the previous state, stopping the pregame spam first
        match self.state.as_deref() {
            Some("PREGAME") => {
                self.stop_pregame();
                self.bus.emit(Event::PregameEnded);
            }
            Some("INGAME") => {
                self.stop_ingame_loadout();
                self.bus.emit(Event::MatchEnded);
            }
            _ => {}
        }

        info!("session state -> {new_state}");
        self.state = Some(new_state.clone());

        match new_state.as_str() {
            "PREGAME" => {
                self.bus.emit(Event::PregameStarted);
                self.pregame = Some(tokio::spawn(poll_pregame(
                    self.session.clone(),
                    self.backend.clone(),
                    self.cfg.pregame_poll_interval_ms,
                )));
            }
            "INGAME" => {
                self.bus.emit(Event::MatchStarted);
                self.ingame_loadout = Some(tokio::spawn(fetch_ingame_loadouts(
                    self.session.clone(),
                    self.backend.clone(),
                )));
            }
            _ => {}
        }
    }

    fn stop_pregame(&mut self) {
        if let Some(handle) = self.pregame.take() {
            handle.abort();
        }
    }

    fn stop_ingame_loadout(&mut self) {
        if let Some(handle) = self.ingame_loadout.take() {
            handle.abort();
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.stop_pregame();
        self.stop_ingame_loadout();
    }
}

const PREGAME_VOLATILE: &[&str] = &[
    "Version",
    "PhaseTimeRemainingNS",
    "StepTimeRemainingNS",
    "LastUpdated",
];

async fn poll_pregame(session: Arc<Session>, backend: Backend, interval_ms: u64) {
    let puuid = session.puuid().await;
    let Some(match_id) = fetch_match_id(&session, &format!("/pregame/v1/players/{puuid}")).await
    else {
        warn!("no pregame match id returned");
        return;
    };

    info!("pregame poll started for match {match_id} (every {interval_ms}ms)");
    let path = format!("/pregame/v1/matches/{match_id}");
    let mut ticker = interval(Duration::from_millis(interval_ms));
    let mut last_marker: Option<String> = None;

    loop {
        ticker.tick().await;
        match session.fetch(Method::GET, Api::Glz, &path, None).await {
            Ok(response) if response.status.is_success() => {
                let marker = dedup::marker(&response.body, &Marker::HashExcluding(PREGAME_VOLATILE));
                if last_marker.as_deref() != Some(marker.as_str()) {
                    last_marker = Some(marker);
                    backend.submit("/v1/pregame", &response.body).await;
                }
                if response.body.contains("character_select_finished") {
                    info!("pregame poll ended: agent select finished");
                    return;
                }
            }
            Ok(response) if response.status == StatusCode::NOT_FOUND => {
                info!("pregame poll ended: 404");
                return;
            }
            Ok(response) => warn!("pregame match returned {}", response.status),
            Err(e) => warn!("pregame fetch failed: {e}"),
        }
    }
}

// fetch the active match loadouts and forward them to the server, retrying every
// 10s until success. the task is aborted by the caller when the match ends.
async fn fetch_ingame_loadouts(session: Arc<Session>, backend: Backend) {
    let puuid = session.puuid().await;
    let player_path = format!("/core-game/v1/players/{puuid}");

    loop {
        let Some(match_id) = fetch_match_id(&session, &player_path).await else {
            debug!("core-game not ready yet, retrying in 10s");
            sleep(Duration::from_secs(10)).await;
            continue;
        };

        let path = format!("/core-game/v1/matches/{match_id}/loadouts");
        match session.fetch(Method::GET, Api::Glz, &path, None).await {
            Ok(response) if response.status.is_success() => {
                backend
                    .submit(
                        &format!("/v1/account/match-loadouts?match_id={match_id}"),
                        &response.body,
                    )
                    .await;
                info!("ingame loadouts sent for match {match_id}");
                return;
            }
            Ok(response) => {
                warn!("ingame loadouts returned {}, retrying in 10s", response.status);
                sleep(Duration::from_secs(10)).await;
            }
            Err(e) => {
                warn!("ingame loadouts fetch failed: {e}, retrying in 10s");
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn fetch_match_id(session: &Session, path: &str) -> Option<String> {
    let response = session.fetch(Method::GET, Api::Glz, path, None).await.ok()?;
    if !response.status.is_success() {
        return None;
    }
    let value: Value = serde_json::from_str(&response.body).ok()?;
    value.get("MatchID")?.as_str().map(str::to_string)
}

// pull the player's sessionLoopState out of a presence event, if present
fn extract_state(text: &str, puuid: &str) -> Option<String> {
    let message: Value = serde_json::from_str(text).ok()?;
    let presences = message.get(2)?.get("data")?.get("presences")?.as_array()?;

    for presence in presences {
        if presence.get("puuid").and_then(Value::as_str) != Some(puuid) {
            continue;
        }
        if presence.get("product").and_then(Value::as_str) != Some("valorant") {
            continue;
        }
        let private = presence.get("private").and_then(Value::as_str)?;
        let decoded = decode_private(private)?;
        let state = decoded
            .get("matchPresenceData")?
            .get("sessionLoopState")?
            .as_str()?;
        return Some(state.to_string());
    }
    None
}

fn decode_private(encoded: &str) -> Option<Value> {
    let padding = (4 - encoded.len() % 4) % 4;
    let padded = format!("{encoded}{}", "=".repeat(padding));
    let bytes = STANDARD.decode(padded).ok()?;
    serde_json::from_slice(&bytes).ok()
}
