use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::backend::Backend;
use crate::config::Config;
use crate::events::{Bus, Event};
use crate::hardware;
use crate::lockfile::Lockfile;
use crate::session::{Api, Session};
use crate::session_service;
use crate::ws;

// owns the app lifecycle: backend auth at startup, then a riot session per
// rso login that authenticates and forwards data on a fixed interval.
pub async fn run(cfg: Config, bus: Bus) {
    let backend = match Backend::new(&cfg.server_base_url) {
        Ok(backend) => backend,
        Err(e) => {
            error!("could not create backend client: {e}");
            return;
        }
    };

    let mut rx = bus.subscribe();

    // start the session pinger early so it observes state events from the start
    let session_pinger = tokio::spawn(session_service::run(bus.clone(), backend.clone()));

    match hardware::collect_hwid() {
        Some(hwid) => backend.set_hwid(hwid).await,
        None => warn!("hwid unavailable, backend login may fail"),
    }

    if !backend.ensure_app_token().await {
        warn!("backend app token unavailable, submissions disabled until next login");
    }

    let backend_refresh = {
        let backend = backend.clone();
        tokio::spawn(async move { backend.proactive_refresh_loop().await })
    };

    let mut session_task: Option<JoinHandle<()>> = None;

    loop {
        match rx.recv().await {
            Ok(Event::RsoLogin(lockfile)) => {
                abort(&mut session_task);
                let backend = backend.clone();
                let bus = bus.clone();
                let cfg = cfg.clone();
                session_task = Some(tokio::spawn(run_session(lockfile, backend, bus, cfg)));
            }
            Ok(Event::RsoLogout) => {
                abort(&mut session_task);
                backend.clear_game_token().await;
            }
            Ok(Event::Shutdown) => {
                abort(&mut session_task);
                break;
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }

    backend_refresh.abort();
    let _ = session_pinger.await;
    info!("orchestrator stopped");
}

async fn run_session(lockfile: Lockfile, backend: Backend, bus: Bus, cfg: Config) {
    let ws_lockfile = lockfile.clone();
    let mut session = match Session::new(lockfile) {
        Ok(session) => session,
        Err(e) => {
            error!("could not build session: {e}");
            return;
        }
    };

    if let Err(e) = session.authenticate().await {
        error!("riot authentication failed: {e}");
        return;
    }

    let puuid = session.puuid().await;
    let shard = session.region().pd_shard.clone();
    if !backend.ensure_game_token(&puuid, &shard).await {
        warn!("game token unavailable, submissions will be skipped");
    }

    let session = Arc::new(session);
    let refresh = session.clone();

    tokio::select! {
        _ = collector_loop(session.clone(), backend.clone(), cfg.collect_interval) => {}
        _ = refresh.proactive_refresh_loop() => {}
        _ = ws::run(ws_lockfile, session.clone(), backend.clone(), bus.clone(), cfg.clone()) => {}
    }
}

async fn collector_loop(session: Arc<Session>, backend: Backend, interval_secs: u64) {
    let mut ticker = interval(Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;
        collect_once(&session, &backend).await;
    }
}

// fetch each riot endpoint and forward the raw body to the matching server path
async fn collect_once(session: &Session, backend: &Backend) {
    let puuid = session.puuid().await;

    let targets = [
        Target::get(Api::Pd, format!("/account-xp/v1/players/{puuid}"), "/v1/account/xp"),
        Target::get(Api::Pd, "/restrictions/v3/penalties".into(), "/v1/account/penalties"),
        Target::get(Api::Pd, format!("/store/v1/wallet/{puuid}"), "/v1/account/balances"),
        Target::get(Api::Pd, format!("/store/v1/entitlements/{puuid}"), "/v1/account/owned-items"),
        Target::post(
            Api::Pd,
            format!("/store/v3/storefront/{puuid}"),
            "/v1/account/storefront",
            json!({}),
        ),
    ];

    for target in targets {
        match session
            .fetch(target.method.clone(), target.api, &target.path, target.body.clone())
            .await
        {
            Ok(response) if response.status.is_success() => {
                backend.submit(target.server_path, &response.body).await;
            }
            Ok(response) => warn!("{} returned {}", target.path, response.status),
            Err(e) => warn!("{} failed: {e}", target.path),
        }
    }
}

struct Target {
    method: Method,
    api: Api,
    path: String,
    server_path: &'static str,
    body: Option<serde_json::Value>,
}

impl Target {
    fn get(api: Api, path: String, server_path: &'static str) -> Self {
        Self {
            method: Method::GET,
            api,
            path,
            server_path,
            body: None,
        }
    }

    fn post(api: Api, path: String, server_path: &'static str, body: serde_json::Value) -> Self {
        Self {
            method: Method::POST,
            api,
            path,
            server_path,
            body: Some(body),
        }
    }
}

fn abort(handle: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}
