#![allow(dead_code)]

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
mod watchers;
mod ws;

use tracing::{error, info};

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

// resolves once ctrl + c is received
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("failed to listen for shutdown signal: {e}");
    }
}
