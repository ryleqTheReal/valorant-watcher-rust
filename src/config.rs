use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{Error, Result};
use crate::paths;

const LEGACY_HOST: &str = "valorant-streamsniper.com";
const CANONICAL_URL: &str = "https://valstats.site";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_base_url: String,

    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,

    #[serde(default = "default_collect_interval")]
    pub collect_interval: u64,

    #[serde(default = "default_true")]
    pub enable_data_sending: bool,

    #[serde(default = "default_ratelimit_timeout")]
    pub ratelimit_timeout: u64,

    #[serde(default = "default_ratelimit_offset")]
    pub ratelimit_offset: u64,

    #[serde(default = "default_pregame_poll_interval_ms")]
    pub pregame_poll_interval_ms: u64,

    #[serde(default = "default_true")]
    pub auto_update: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = paths::config_path()?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let mut config: Config =
            serde_json::from_str(&raw).map_err(|e| Error::Config(format!("invalid config.json: {e}")))?;

        if config.server_base_url.contains(LEGACY_HOST) {
            info!("migrating server_base_url from {} to {CANONICAL_URL}", config.server_base_url);
            config.server_base_url = CANONICAL_URL.to_string();
            if let Ok(updated) = serde_json::to_string_pretty(&config) {
                let _ = std::fs::write(&path, updated);
            }
        }

        Ok(config)
    }
}

fn default_poll_interval() -> u64 {
    3
}

fn default_collect_interval() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

fn default_ratelimit_timeout() -> u64 {
    60
}

fn default_ratelimit_offset() -> u64 {
    60
}

fn default_pregame_poll_interval_ms() -> u64 {
    250
}
