#![allow(dead_code)]

mod config;
mod error;
mod events;
mod lockfile;
mod logging;
mod paths;
mod session;
mod watchers;

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
    bus.emit(Event::Startup);

    let handles = vec![
        watchers::spawn_riot_client(bus.clone(), cfg.poll_interval),
        watchers::spawn_process(bus.clone(), cfg.poll_interval),
    ];

    shutdown_signal().await;
    info!("shutdown signal received, stopping");
    bus.emit(Event::Shutdown);

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
