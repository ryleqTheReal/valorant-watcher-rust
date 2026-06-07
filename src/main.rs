#![allow(dead_code)]
// no console window in release builds; dev builds keep it for live logs
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod config;
mod dedup;
mod error;
mod events;
mod hardware;
mod lockfile;
mod logging;
mod orchestrator;
mod paths;
mod session;
mod session_service;
mod single_instance;
mod updater;
mod watchers;
mod ws;

use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::Result;
use crate::events::{Bus, Event};

#[tokio::main]
async fn main() {
    let _guards = match logging::init() {
        Ok(guards) => guards,
        Err(e) => {
            eprintln!("failed to initialize logging: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run().await {
        error!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cfg = Config::load()?;
    info!("valorant watcher started");
    info!(
        server = %cfg.server_base_url,
        poll_interval = cfg.poll_interval,
        "config loaded"
    );

    if cfg.auto_update {
        updater::check_and_apply().await;
    }

    // acquired after the update check so the restart handoff never races on the lock
    let _instance = match single_instance::acquire() {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            info!("another instance is already running, exiting");
            return Ok(());
        }
        Err(e) => {
            warn!("could not acquire single-instance lock: {e}, continuing");
            None
        }
    };

    let bus = Bus::new();

    // start the orchestrator first so it is subscribed before the watchers emit
    let orchestrator = tokio::spawn(orchestrator::run(cfg.clone(), bus.clone()));
    bus.emit(Event::Startup);

    let handles = vec![
        watchers::spawn_riot_client(bus.clone(), cfg.poll_interval),
        watchers::spawn_process(bus.clone(), cfg.poll_interval),
    ];

    shutdown_signal().await;
    info!("shutdown signal received, stopping");
    bus.emit(Event::Shutdown);

    let _ = orchestrator.await;
    for handle in handles {
        let _ = handle.await;
    }
    info!("app terminated");
    Ok(())
}

// resolves once ctrl + c is received. in a gui build there is no console to
// deliver ctrl-c, so we must not treat that as a shutdown; run until terminated.
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("ctrl-c received"),
        Err(e) => {
            error!("ctrl-c handler unavailable ({e}); running until terminated");
            std::future::pending::<()>().await;
        }
    }
}
