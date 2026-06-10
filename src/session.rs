use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use regex::Regex;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::lockfile::Lockfile;
use crate::paths;

// static client identity headers expected by the riot pd/glz endpoints
const CLIENT_PLATFORM: &str = "ew0KCSJwbGF0Zm9ybVR5cGUiOiAiUEMiLA0KCSJwbGF0Zm9ybU9TIjog\
IldpbmRvd3MiLA0KCSJwbGF0Zm9ybU9TVmVyc2lvbiI6ICIxMC4wLjE5\
MDQyLjEuMjU2LjY0Yml0IiwNCgkicGxhdGZvcm1DaGlwc2V0IjogIlVua25vd24iDQp9";
const USER_AGENT: &str = "ShooterGame/13 Windows/10.0.19043.1.256.64bit";

const RATELIMIT_COOLDOWN: Duration = Duration::from_secs(65);

// which riot service a request targets; decides the base url and auth scheme
#[derive(Debug, Clone, Copy)]
pub enum Api {
    Pd,
    Glz,
    Shared,
    Local,
}

#[derive(Debug, Clone)]
pub struct Region {
    pub pd_shard: String,
    pub glz_shard: String,
    pub glz_region: String,
}

#[derive(Debug, Clone, Default)]
struct BaseUrls {
    pd: String,
    glz: String,
    shared: String,
    local: String,
}

// the mutable part of the session, shared with the proactive refresh task
#[derive(Default)]
struct State {
    puuid: String,
    access_token: String,
    entitlements_token: String,
    client_version: String,
    expires_at: f64,
    cooldown_until: Option<Instant>,
}

// status and raw body of an api call; the body is forwarded to the server as-is
pub struct ApiResponse {
    pub status: StatusCode,
    pub body: String,
}

pub struct Session {
    lockfile: Lockfile,
    client: reqwest::Client,
    state: Arc<RwLock<State>>,
    region: Region,
    base_urls: BaseUrls,
}

impl Session {
    pub fn new(lockfile: Lockfile) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            lockfile,
            client,
            state: Arc::new(RwLock::new(State::default())),
            region: Region {
                pd_shard: String::new(),
                glz_shard: String::new(),
                glz_region: String::new(),
            },
            base_urls: BaseUrls::default(),
        })
    }

    // fetch entitlements from the local api and assemble the auth headers.
    // blocks until the region appears in the valorant logs, so call this from a
    // task that gets aborted on logout or shutdown.
    pub async fn authenticate(&mut self) -> Result<()> {
        self.region = wait_for_region().await;
        self.base_urls = BaseUrls {
            pd: format!("https://pd.{}.a.pvp.net", self.region.pd_shard),
            glz: format!(
                "https://glz-{}.{}.a.pvp.net",
                self.region.glz_shard, self.region.glz_region
            ),
            shared: format!("https://shared.{}.a.pvp.net", self.region.pd_shard),
            local: self.lockfile.base_url(),
        };

        let entitlements = self.wait_for_entitlements().await?;
        let access_token = entitlements
            .access_token
            .ok_or_else(|| Error::Auth("entitlements response missing accessToken".into()))?;
        let entitlements_token = entitlements
            .token
            .ok_or_else(|| Error::Auth("entitlements response missing token".into()))?;
        let puuid = entitlements.subject.unwrap_or_default();
        let expires_at = decode_jwt_exp(&access_token).unwrap_or_else(|| now_unix() + 3600.0);
        let client_version = resolve_version().await?;

        {
            let mut state = self.state.write().await;
            state.puuid = puuid.clone();
            state.access_token = access_token;
            state.entitlements_token = entitlements_token;
            state.client_version = client_version;
            state.expires_at = expires_at;
        }

        info!(puuid = %puuid, "riot authentication complete");
        Ok(())
    }

    pub async fn puuid(&self) -> String {
        self.state.read().await.puuid.clone()
    }

    pub fn region(&self) -> &Region {
        &self.region
    }

    // send an authenticated request and return the status and raw body.
    // handles the 429 cooldown and the 401 / 400 BAD_CLAIMS refresh transparently.
    pub async fn fetch(
        &self,
        method: Method,
        api: Api,
        path: &str,
        body: Option<Value>,
    ) -> Result<ApiResponse> {
        let is_riot = matches!(api, Api::Pd | Api::Glz | Api::Shared);

        if is_riot {
            self.wait_for_cooldown().await;
        }

        let mut response = self.send(method.clone(), api, path, body.clone()).await?;

        if is_riot && response.status() == StatusCode::TOO_MANY_REQUESTS {
            self.set_cooldown().await;
            warn!("rate limited on {path}, waiting before retry");
            self.wait_for_cooldown().await;
            response = self.send(method.clone(), api, path, body.clone()).await?;
        }

        let status = response.status();
        let text = response.text().await?;

        let mut needs_refresh = is_riot && status == StatusCode::UNAUTHORIZED;
        if is_riot && status == StatusCode::BAD_REQUEST {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("errorCode").and_then(Value::as_str) == Some("BAD_CLAIMS") {
                    needs_refresh = true;
                }
            }
        }

        if needs_refresh {
            warn!("got {status} on {path}, refreshing entitlements");
            self.refresh_entitlements().await?;
            let retry = self.send(method, api, path, body).await?;
            let status = retry.status();
            let body = retry.text().await?;
            return Ok(ApiResponse { status, body });
        }

        Ok(ApiResponse { status, body: text })
    }

    async fn send(
        &self,
        method: Method,
        api: Api,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url(api), path);
        let mut request = self.client.request(method, url);

        match api {
            Api::Local => {
                request = request.header("Authorization", self.lockfile.auth_header());
            }
            _ => {
                let state = self.state.read().await;
                request = request
                    .header("Authorization", format!("Bearer {}", state.access_token))
                    .header("X-Riot-Entitlements-JWT", &state.entitlements_token)
                    .header("X-Riot-ClientPlatform", CLIENT_PLATFORM)
                    .header("X-Riot-ClientVersion", &state.client_version)
                    .header("User-Agent", USER_AGENT);
            }
        }

        if let Some(payload) = body {
            request = request.json(&payload);
        }

        Ok(request.send().await?)
    }

    fn base_url(&self, api: Api) -> &str {
        match api {
            Api::Pd => &self.base_urls.pd,
            Api::Glz => &self.base_urls.glz,
            Api::Shared => &self.base_urls.shared,
            Api::Local => &self.base_urls.local,
        }
    }

    async fn wait_for_entitlements(&self) -> Result<EntitlementsResponse> {
        loop {
            let response = self
                .fetch(Method::GET, Api::Local, "/entitlements/v1/token", None)
                .await?;
            let data: EntitlementsResponse = serde_json::from_str(&response.body)?;

            match data.message.as_deref() {
                Some("Entitlements token is not ready yet") => {
                    debug!("entitlements not ready yet, retrying");
                    sleep(Duration::from_secs(1)).await;
                }
                Some("Invalid URI format") => {
                    warn!("invalid uri format from entitlements endpoint, retrying");
                    sleep(Duration::from_secs(5)).await;
                }
                _ if data.access_token.is_some() && data.token.is_some() => return Ok(data),
                _ => sleep(Duration::from_secs(1)).await,
            }
        }
    }

    // re-fetch the entitlements token and update the shared state.
    // retries with a linear backoff, mirroring the python handler.
    async fn refresh_entitlements(&self) -> Result<()> {
        const MAX_RETRIES: u32 = 10;
        for attempt in 1..=MAX_RETRIES {
            match self.try_refresh_entitlements().await {
                Ok(()) => {
                    info!("entitlements refreshed successfully");
                    return Ok(());
                }
                Err(e) => {
                    warn!("entitlements refresh failed (attempt {attempt}/{MAX_RETRIES}): {e}");
                    if attempt < MAX_RETRIES {
                        sleep(Duration::from_secs(2 * attempt as u64)).await;
                    }
                }
            }
        }
        Err(Error::Auth("entitlements refresh exhausted all retries".into()))
    }

    async fn try_refresh_entitlements(&self) -> Result<()> {
        let response = self
            .send(Method::GET, Api::Local, "/entitlements/v1/token", None)
            .await?;
        if response.status() != StatusCode::OK {
            return Err(Error::Auth(format!(
                "entitlements endpoint returned {}",
                response.status()
            )));
        }
        let data: EntitlementsResponse = response.json().await?;
        if let Some(message) = data.message {
            return Err(Error::Auth(format!("entitlements refresh rejected: {message}")));
        }

        let access_token = data
            .access_token
            .ok_or_else(|| Error::Auth("refresh missing accessToken".into()))?;
        let entitlements_token = data
            .token
            .ok_or_else(|| Error::Auth("refresh missing token".into()))?;
        let expires_at = decode_jwt_exp(&access_token).unwrap_or_else(|| now_unix() + 3600.0);

        let mut state = self.state.write().await;
        state.access_token = access_token;
        state.entitlements_token = entitlements_token;
        state.expires_at = expires_at;
        if let Some(subject) = data.subject {
            state.puuid = subject;
        }
        Ok(())
    }

    // refresh at 75% of the token lifetime; runs until the task is aborted
    pub async fn proactive_refresh_loop(&self) {
        loop {
            let lifetime = self.state.read().await.expires_at - now_unix();
            // floor at 30s so an expired token never produces a tight spin
            let sleep_for = (lifetime * 0.75).max(30.0);
            debug!("proactive refresh scheduled in {sleep_for:.0}s");
            sleep(Duration::from_secs_f64(sleep_for)).await;
            if let Err(e) = self.refresh_entitlements().await {
                warn!("proactive refresh failed: {e}, retrying in 60s");
                sleep(Duration::from_secs(60)).await;
            }
        }
    }

    async fn wait_for_cooldown(&self) {
        let remaining = {
            let state = self.state.read().await;
            state
                .cooldown_until
                .map(|until| until.saturating_duration_since(Instant::now()))
        };
        if let Some(remaining) = remaining {
            if !remaining.is_zero() {
                info!("rate limit active, waiting {remaining:?}");
                sleep(remaining).await;
            }
        }
    }

    async fn set_cooldown(&self) {
        self.state.write().await.cooldown_until = Some(Instant::now() + RATELIMIT_COOLDOWN);
    }
}

#[derive(Debug, Deserialize)]
struct EntitlementsResponse {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    token: Option<String>,
    subject: Option<String>,
    message: Option<String>,
}

// poll the valorant logs until the region and shard can be parsed out
async fn wait_for_region() -> Region {
    loop {
        if let Ok(log) = read_riot_log() {
            if let Some(region) = parse_region(&log) {
                info!(
                    pd = %region.pd_shard,
                    glz = %region.glz_shard,
                    "resolved region from logs"
                );
                return region;
            }
        }
        debug!("region not in logs yet, retrying in 2s");
        sleep(Duration::from_secs(2)).await;
    }
}

// prefer the version from the logs, fall back to valorant-api.com
async fn resolve_version() -> Result<String> {
    if let Ok(log) = read_riot_log() {
        if let Some(version) = parse_version(&log) {
            info!(%version, "resolved client version from logs");
            return Ok(version);
        }
    }
    let version = fetch_version_fallback().await?;
    info!(%version, "resolved client version from fallback api");
    Ok(version)
}

fn read_riot_log() -> Result<String> {
    Ok(std::fs::read_to_string(paths::riot_log_path()?)?)
}

fn parse_region(log: &str) -> Option<Region> {
    let pd_re = Regex::new(r"https://pd\.(\w+)\.a\.pvp\.net/account-xp/v1/").ok()?;
    let glz_re = Regex::new(r"https://glz-(\w[\w-]*)\.(\w+)\.a\.pvp\.net").ok()?;

    let mut pd_shard: Option<String> = None;
    let mut glz: Option<(String, String)> = None;

    for line in log.lines() {
        if pd_shard.is_none() {
            if let Some(caps) = pd_re.captures(line) {
                pd_shard = Some(caps[1].to_string());
            }
        }
        if glz.is_none() {
            if let Some(caps) = glz_re.captures(line) {
                glz = Some((caps[1].to_string(), caps[2].to_string()));
            }
        }
        if pd_shard.is_some() && glz.is_some() {
            break;
        }
    }

    let pd_shard = pd_shard?;
    let (glz_shard, glz_region) = glz?;

    // pbe shares the na shard
    if pd_shard == "pbe" {
        return Some(Region {
            pd_shard: "na".into(),
            glz_shard: "na-1".into(),
            glz_region: "na".into(),
        });
    }

    Some(Region {
        pd_shard,
        glz_shard,
        glz_region,
    })
}

fn parse_version(log: &str) -> Option<String> {
    let re = Regex::new(r"CI server version:\s*(.+)").ok()?;
    let caps = log.lines().find_map(|line| re.captures(line))?;
    let mut parts: Vec<&str> = caps[1].trim().split('-').collect();
    if !parts.contains(&"shipping") && parts.len() >= 2 {
        parts.insert(2, "shipping");
    }
    Some(parts.join("-"))
}

async fn fetch_version_fallback() -> Result<String> {
    #[derive(Deserialize)]
    struct VersionApiResponse {
        data: VersionData,
    }
    #[derive(Deserialize)]
    struct VersionData {
        #[serde(rename = "riotClientVersion")]
        riot_client_version: String,
    }

    let response = reqwest::get("https://valorant-api.com/v1/version").await?;
    let parsed: VersionApiResponse = response.json().await?;
    Ok(parsed.data.riot_client_version)
}

fn decode_jwt_exp(token: &str) -> Option<f64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp")?.as_f64()
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
