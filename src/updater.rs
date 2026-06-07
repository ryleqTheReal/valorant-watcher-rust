use tracing::{info, warn};

const REPO_OWNER: &str = "ryleqTheReal";
const REPO_NAME: &str = "valorant-watcher-rust";

// check the github releases for a newer version and replace the running binary
pub async fn check_and_apply() {
    match tokio::task::spawn_blocking(run_update).await {
        Ok(Ok(Some(version))) => {
            info!("updated to {version}, restarting");
            restart();
        }
        Ok(Ok(None)) => info!("already on the latest version"),
        Ok(Err(e)) => warn!("update check failed: {e}"),
        Err(e) => warn!("update task panicked: {e}"),
    }
}

fn run_update() -> Result<Option<String>, String> {
    let target = if cfg!(target_os = "windows") {
        "windows"
    } else {
        "macos"
    };

    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("valorant-watcher")
        .target(target)
        .show_download_progress(false)
        .no_confirm(true)
        .current_version(self_update::cargo_crate_version!())
        .build()
        .map_err(|e| e.to_string())?
        .update()
        .map_err(|e| e.to_string())?;

    match status {
        self_update::Status::Updated(version) => Ok(Some(version)),
        self_update::Status::UpToDate(_) => Ok(None),
    }
}

#[cfg(target_os = "windows")]
fn restart() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

#[cfg(not(target_os = "windows"))]
fn restart() -> ! {
    std::process::exit(0);
}
