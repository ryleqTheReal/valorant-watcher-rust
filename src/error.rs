use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("path resolution error: {0}")]
    Path(String),

    #[error("malformed lockfile: {0}")]
    Lockfile(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
