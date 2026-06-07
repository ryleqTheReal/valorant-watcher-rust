use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::error::{Error, Result};

// parsed riot client lockfile, format: name:pid:port:password:protocol
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub name: String,
    pub pid: u32,
    pub port: u16,
    pub password: String,
    pub protocol: String,
}

impl Lockfile {
    pub fn parse(content: &str) -> Result<Self> {
        let parts: Vec<&str> = content.trim().split(':').collect();
        if parts.len() != 5 {
            return Err(Error::Lockfile(format!(
                "expected 5 fields, got {}",
                parts.len()
            )));
        }

        Ok(Self {
            name: parts[0].to_string(),
            pid: parts[1]
                .parse()
                .map_err(|_| Error::Lockfile(format!("invalid pid: {}", parts[1])))?,
            port: parts[2]
                .parse()
                .map_err(|_| Error::Lockfile(format!("invalid port: {}", parts[2])))?,
            password: parts[3].to_string(),
            protocol: parts[4].to_string(),
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn base_url(&self) -> String {
        format!("{}://127.0.0.1:{}", self.protocol, self.port)
    }

    pub fn wss_url(&self) -> String {
        format!("wss://127.0.0.1:{}", self.port)
    }

    // basic auth header for the local riot api, user is always "riot"
    pub fn auth_header(&self) -> String {
        let token = STANDARD.encode(format!("riot:{}", self.password));
        format!("Basic {token}")
    }
}
