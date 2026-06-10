use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use crate::error::{Error, Result};
use crate::paths;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

// error codes that mean a token is permanently dead; refreshing will never recover it
const DEAD_TOKEN_ERRORS: &[&str] = &[
    "REFRESH_TOKEN_EXPIRED",
    "TOKEN_DECODE_ERROR",
    "WRONG_TOKEN_TYPE",
    "WRONG_TOKEN_SCOPE",
    "USER_NOT_FOUND",
    "USER_IS_BANNED",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenPair {
    access_token: String,
    access_token_expires_at: i64,
    refresh_token: String,
    refresh_token_expires_at: i64,
}

impl TokenPair {
    fn access_expires_in(&self) -> f64 {
        self.access_token_expires_at as f64 - now_secs() as f64
    }

    fn refresh_expires_in(&self) -> f64 {
        self.refresh_token_expires_at as f64 - now_secs() as f64
    }

    fn is_refresh_expired(&self) -> bool {
        self.refresh_expires_in() <= 60.0
    }
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    app: TokenPair,
}

#[derive(Default)]
struct Tokens {
    app: Option<TokenPair>,
    game: Option<TokenPair>,
    bound_puuid: Option<String>,
    bound_shard: Option<String>,
}

// drives the backend auth lifecycle. cheap to clone; all shared state sits
// behind Arcs so the orchestrator, refresh loop, and submitter can share it.
#[derive(Clone)]
pub struct Backend {
    base_url: String,
    client: reqwest::Client,
    tokens: Arc<RwLock<Tokens>>,
    hwid: Arc<RwLock<Option<String>>>,
    tokens_path: PathBuf,
}

impl Backend {
    pub fn new(server_base_url: &str) -> Result<Self> {
        Ok(Self {
            base_url: normalize_base_url(server_base_url),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            tokens: Arc::new(RwLock::new(Tokens::default())),
            hwid: Arc::new(RwLock::new(None)),
            tokens_path: paths::auth_tokens_path()?,
        })
    }

    pub async fn set_hwid(&self, hwid: String) {
        *self.hwid.write().await = Some(hwid);
    }

    // headers for game-scoped backend submissions, or None if no game token is held
    pub async fn game_headers(&self) -> Option<Vec<(String, String)>> {
        let tokens = self.tokens.read().await;
        let game = tokens.game.as_ref()?;
        let puuid = tokens.bound_puuid.as_ref()?;
        let shard = tokens.bound_shard.as_ref()?;
        Some(vec![
            ("Authorization".into(), format!("Bearer {}", game.access_token)),
            ("puuid".into(), puuid.clone()),
            ("shard".into(), shard.clone()),
        ])
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn app_access_token(&self) -> Option<String> {
        self.tokens
            .read()
            .await
            .app
            .as_ref()
            .map(|app| app.access_token.clone())
    }

    // post a raw riot response body to the server using the game token headers
    pub async fn submit(&self, path: &str, body: &str) {
        let headers = match self.game_headers().await {
            Some(headers) => headers,
            None => {
                info!("game token unavailable, skipping submission to {path}");
                return;
            }
        };

        let mut request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header("Content-Type", "application/json")
            .body(body.to_string());
        for (key, value) in headers {
            request = request.header(key, value);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!("submitted {path} ({})", response.status());
            }
            Ok(response) => warn!("submit {path} returned {}", response.status()),
            Err(e) => warn!("submit {path} failed: {e}"),
        }
    }

    // ensure an app token is held: refresh from disk if possible, otherwise run
    // the discord oauth loopback login and POST /v1/login
    pub async fn ensure_app_token(&self) -> bool {
        if let Some(app) = &self.tokens.read().await.app {
            if !app.is_refresh_expired() {
                return true;
            }
        }

        if let Some(stored) = self.load_session() {
            if !stored.app.is_refresh_expired() {
                info!("refreshing backend app token using stored refresh_token");
                match self.post_refresh_app(&stored.app.refresh_token).await {
                    Ok(pair) => {
                        self.tokens.write().await.app = Some(pair);
                        self.save_session().await;
                        info!("app token refreshed from disk");
                        return true;
                    }
                    Err(e) => {
                        log_refresh_failure("app", &e);
                        if !e.is_dead_token() {
                            return false;
                        }
                    }
                }
            }
        }

        info!("running discord oauth loopback login for app token");
        let proof = match self.discord_login().await {
            Ok(proof) => proof,
            Err(e) => {
                error!("discord login failed: {e}");
                return false;
            }
        };

        let hwid = match self.hwid.read().await.clone() {
            Some(hwid) => hwid,
            None => {
                error!("hwid not available, cannot complete backend login");
                return false;
            }
        };

        match self
            .post_login(&hwid, &proof.provider, &proof.provider_id, &proof.access_token)
            .await
        {
            Ok(pair) => {
                let mut tokens = self.tokens.write().await;
                tokens.app = Some(pair);
                // a fresh login invalidates any previously bound game token
                tokens.game = None;
                tokens.bound_puuid = None;
                tokens.bound_shard = None;
                drop(tokens);
                self.save_session().await;
                info!("backend app login complete");
                true
            }
            Err(e) => {
                error!("POST /v1/login rejected: {e}");
                false
            }
        }
    }

    // ensure a game token bound to (puuid, shard) -> refreshes if already bound
    pub async fn ensure_game_token(&self, puuid: &str, shard: &str) -> bool {
        let app_access = match &self.tokens.read().await.app {
            Some(app) => app.access_token.clone(),
            None => {
                error!("cannot mint game token: app token unavailable");
                return false;
            }
        };

        let can_refresh = {
            let tokens = self.tokens.read().await;
            match (&tokens.game, &tokens.bound_puuid, &tokens.bound_shard) {
                (Some(game), Some(p), Some(s)) => {
                    p == puuid && s == shard && !game.is_refresh_expired()
                }
                _ => false,
            }
        };

        if can_refresh {
            let refresh_token = self.tokens.read().await.game.as_ref().unwrap().refresh_token.clone();
            match self.post_refresh_game(&refresh_token).await {
                Ok(pair) => {
                    self.tokens.write().await.game = Some(pair);
                    self.save_session().await;
                    info!("game token refreshed for same account");
                    return true;
                }
                Err(e) => {
                    log_refresh_failure("game", &e);
                    self.tokens.write().await.game = None;
                }
            }
        }

        info!("minting fresh game token (puuid={}..., shard={shard})", short(puuid));
        match self.post_game_token(&app_access, puuid, shard).await {
            Ok(pair) => {
                let mut tokens = self.tokens.write().await;
                tokens.game = Some(pair);
                tokens.bound_puuid = Some(puuid.to_string());
                tokens.bound_shard = Some(shard.to_string());
                drop(tokens);
                self.save_session().await;
                info!("game token issued");
                true
            }
            Err(e) => {
                error!("POST /v1/auth/game-token rejected: {e}");
                let mut tokens = self.tokens.write().await;
                tokens.game = None;
                tokens.bound_puuid = None;
                tokens.bound_shard = None;
                false
            }
        }
    }

    // drop the game token on logout but keep the app token alive
    pub async fn clear_game_token(&self) {
        let mut tokens = self.tokens.write().await;
        tokens.game = None;
        tokens.bound_puuid = None;
        tokens.bound_shard = None;
    }

    // refresh whichever access token expires soonest ->  runs until the task is aborted
    pub async fn proactive_refresh_loop(&self) {
        loop {
            let (app_remaining, game_remaining, has_app) = {
                let tokens = self.tokens.read().await;
                let app_remaining = tokens.app.as_ref().map(TokenPair::access_expires_in);
                let game_remaining = tokens
                    .game
                    .as_ref()
                    .map(TokenPair::access_expires_in)
                    .unwrap_or(f64::INFINITY);
                (app_remaining.unwrap_or(0.0), game_remaining, tokens.app.is_some())
            };

            if !has_app {
                return;
            }

            // floor at 30s so an expired token never produces a tight spin
            let (sleep_for, refresh_app) = if app_remaining <= game_remaining {
                ((app_remaining * 0.75).max(30.0), true)
            } else {
                ((game_remaining * 0.75).max(30.0), false)
            };

            sleep(Duration::from_secs_f64(sleep_for)).await;

            let result = if refresh_app {
                let rt = match &self.tokens.read().await.app {
                    Some(app) => app.refresh_token.clone(),
                    None => return,
                };
                self.post_refresh_app(&rt).await
            } else {
                let rt = match &self.tokens.read().await.game {
                    Some(game) => game.refresh_token.clone(),
                    None => continue,
                };
                self.post_refresh_game(&rt).await
            };

            match result {
                Ok(pair) => {
                    let mut tokens = self.tokens.write().await;
                    if refresh_app {
                        tokens.app = Some(pair);
                    } else {
                        tokens.game = Some(pair);
                    }
                    drop(tokens);
                    self.save_session().await;
                    info!("backend {} token refreshed proactively", scope_name(refresh_app));
                }
                Err(e) if e.is_dead_token() => {
                    error!("proactive {} refresh permanently failed ({e}); session is dead", scope_name(refresh_app));
                    let mut tokens = self.tokens.write().await;
                    tokens.app = None;
                    tokens.game = None;
                    return;
                }
                Err(e) => {
                    warn!("proactive {} refresh failed ({e}); retrying in 60s", scope_name(refresh_app));
                    sleep(Duration::from_secs(60)).await;
                }
            }
        }
    }

    // --- discord oauth loopback --- 

    async fn discord_login(&self) -> Result<LoginProof> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let loopback = format!("http://127.0.0.1:{port}/callback");

        let auth_url = self
            .client
            .get(format!("{}/v1/auth/discord", self.base_url))
            .query(&[("redirect", "false"), ("loopback_redirect", &loopback)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let auth_url = auth_url.trim().trim_matches('"').to_string();

        info!("opening discord oauth in browser, loopback {loopback}");
        open_browser(&auth_url);

        match timeout(LOGIN_TIMEOUT, accept_callback(&listener)).await {
            Ok(Some(proof)) => Ok(proof),
            Ok(None) => Err(Error::Auth("oauth callback closed without credentials".into())),
            Err(_) => Err(Error::Auth("oauth loopback timed out".into())),
        }
    }

    // --- http ---

    async fn post_login(
        &self,
        hwid: &str,
        provider: &str,
        provider_id: &str,
        access_token: &str,
    ) -> std::result::Result<TokenPair, BackendAuthError> {
        self.post_token(
            "/v1/login",
            json!({
                "hwid": hwid,
                "provider": provider,
                "provider_id": provider_id,
                "access_token": access_token,
            }),
            None,
        )
        .await
    }

    async fn post_game_token(
        &self,
        app_access_token: &str,
        puuid: &str,
        shard: &str,
    ) -> std::result::Result<TokenPair, BackendAuthError> {
        self.post_token(
            "/v1/auth/game-token",
            json!({ "puuid": puuid, "shard": shard }),
            Some(app_access_token),
        )
        .await
    }

    async fn post_refresh_app(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<TokenPair, BackendAuthError> {
        self.post_token(
            "/v1/auth/refresh/app",
            json!({ "refresh_token": refresh_token }),
            None,
        )
        .await
    }

    async fn post_refresh_game(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<TokenPair, BackendAuthError> {
        self.post_token(
            "/v1/auth/refresh/game",
            json!({ "refresh_token": refresh_token }),
            None,
        )
        .await
    }

    async fn post_token(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> std::result::Result<TokenPair, BackendAuthError> {
        let mut request = self.client.post(format!("{}{path}", self.base_url)).json(&body);
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request
            .send()
            .await
            .map_err(|e| BackendAuthError::network(e.to_string()))?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            let error_code = serde_json::from_str::<ErrorBody>(&text)
                .ok()
                .and_then(|body| body.error_code);
            return Err(BackendAuthError {
                status_code: Some(status.as_u16()),
                error_code,
            });
        }

        let parsed: TokenResponseBody = serde_json::from_str(&text)
            .map_err(|e| BackendAuthError::network(format!("malformed token response: {e}")))?;
        Ok(TokenPair {
            access_token: parsed.access_token,
            access_token_expires_at: parsed.access_token_expires_at,
            refresh_token: parsed.refresh_token,
            refresh_token_expires_at: parsed.refresh_token_expires_at,
        })
    }

    // --- persistence ---

    fn load_session(&self) -> Option<StoredSession> {
        let raw = std::fs::read_to_string(&self.tokens_path).ok()?;
        match serde_json::from_str(&raw) {
            Ok(session) => Some(session),
            Err(e) => {
                warn!("stored auth tokens are malformed: {e}");
                None
            }
        }
    }

    async fn save_session(&self) {
        let app = match &self.tokens.read().await.app {
            Some(app) => app.clone(),
            None => return,
        };
        let stored = StoredSession { app };

        if let Some(parent) = self.tokens_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string(&stored) {
            Ok(payload) => {
                if let Err(e) = std::fs::write(&self.tokens_path, payload) {
                    warn!("could not persist app token: {e}");
                }
            }
            Err(e) => warn!("could not serialize app token: {e}"),
        }
    }
}

struct LoginProof {
    provider: String,
    provider_id: String,
    access_token: String,
}

// a non-2xx backend response, carrying the error_code for dead-token detection
#[derive(Debug)]
struct BackendAuthError {
    status_code: Option<u16>,
    error_code: Option<String>,
}

impl BackendAuthError {
    fn network(message: String) -> Self {
        // network failures are transient, not dead tokens
        Self {
            status_code: None,
            error_code: Some(format!("NETWORK_ERROR: {message}")),
        }
    }

    fn is_dead_token(&self) -> bool {
        self.error_code
            .as_deref()
            .is_some_and(|code| DEAD_TOKEN_ERRORS.contains(&code))
    }
}

impl std::fmt::Display for BackendAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HTTP {:?} error_code={:?}",
            self.status_code, self.error_code
        )
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error_code: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponseBody {
    access_token: String,
    access_token_expires_at: i64,
    refresh_token: String,
    refresh_token_expires_at: i64,
}

fn log_refresh_failure(scope: &str, e: &BackendAuthError) {
    if e.is_dead_token() {
        warn!("stored {scope} refresh_token is dead ({e})");
    } else {
        warn!("{scope} refresh failed: {e}");
    }
}

// accept connections until one carries valid /callback credentials
async fn accept_callback(listener: &TcpListener) -> Option<LoginProof> {
    loop {
        let (mut stream, _) = listener.accept().await.ok()?;
        match read_callback(&mut stream).await {
            Some(proof) => {
                write_response(&mut stream, "200 OK", "text/html; charset=utf-8", LOGIN_SUCCESS_HTML).await;
                return Some(proof);
            }
            None => {
                write_response(&mut stream, "400 Bad Request", "text/plain; charset=utf-8", "bad request").await;
            }
        }
    }
}

async fn read_callback(stream: &mut TcpStream) -> Option<LoginProof> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.ok()?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let target = request.lines().next()?.split_whitespace().nth(1)?;
    let (path, query) = target.split_once('?')?;
    if path != "/callback" {
        return None;
    }

    let mut provider = None;
    let mut provider_id = None;
    let mut access_token = None;
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        let value = percent_decode(value);
        match key {
            "provider" => provider = Some(value),
            "provider_id" => provider_id = Some(value),
            "access_token" => access_token = Some(value),
            _ => {}
        }
    }

    Some(LoginProof {
        provider: provider?,
        provider_id: provider_id?,
        access_token: access_token?,
    })
}

async fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.flush().await;
}

fn percent_decode(input: &str) -> String {
    let bytes = input.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) {
    if let Err(e) = open::that_detached(url) {
        warn!("could not open browser automatically, open this url manually: {url} ({e})");
    }
}

fn normalize_base_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

fn scope_name(is_app: bool) -> &'static str {
    if is_app { "app" } else { "game" }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(8)]
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const LOGIN_SUCCESS_HTML: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>Signed in</title></head><body style=\"font-family:sans-serif;text-align:center;padding-top:4rem\">\
<h1>you're signed in</h1><p>you can close this tab and return to the app.</p></body></html>";
