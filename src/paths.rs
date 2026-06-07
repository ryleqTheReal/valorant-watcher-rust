use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

// directory of the running executable; config.json and data/ live next to it
pub fn app_base_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::Path("executable has no parent directory".into()))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(app_base_dir()?.join("config.json"))
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(app_base_dir()?.join("data"))
}

pub fn auth_tokens_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("auth.json"))
}

pub fn lockfile_path() -> Result<PathBuf> {
    Ok(riot_client_config_dir()?.join("lockfile"))
}

pub fn riot_log_path() -> Result<PathBuf> {
    let home = home()?;
    #[cfg(target_os = "windows")]
    let path = home
        .join("AppData")
        .join("Local")
        .join("VALORANT")
        .join("Saved")
        .join("Logs")
        .join("ShooterGame.log");
    #[cfg(target_os = "macos")]
    let path = home
        .join("Library")
        .join("Application Support")
        .join("VALORANT")
        .join("Saved")
        .join("Logs")
        .join("ShooterGame.log");
    Ok(path)
}

fn riot_client_config_dir() -> Result<PathBuf> {
    let home = home()?;
    #[cfg(target_os = "windows")]
    let path = home
        .join("AppData")
        .join("Local")
        .join("Riot Games")
        .join("Riot Client")
        .join("Config");
    #[cfg(target_os = "macos")]
    let path = home
        .join("Library")
        .join("Application Support")
        .join("Riot Games")
        .join("Riot Client")
        .join("Config");
    Ok(path)
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| Error::Path("home directory not found".into()))
}
