//! OAuth 2.0 Authorization Code + PKCE for the X API, plus the on-disk token store.

use std::fs;
use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::config::Config;

const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
pub const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
const SCOPES: &str = "tweet.read%20users.read%20follows.read%20follows.write%20offline.access";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// OAuth app credentials from config.yaml.
#[derive(Clone)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

/// Persisted user tokens (token.json, mode 0600).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds
    pub expires_at: i64,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub username: String,
}

impl Token {
    pub fn load() -> Result<Self> {
        let path = Config::token_path();
        let contents = fs::read_to_string(&path).map_err(|_| {
            anyhow::anyhow!(
                "Not authenticated (no {}). Run `twit auth`.",
                path.display()
            )
        })?;
        serde_json::from_str(&contents).map_err(|_| {
            anyhow::anyhow!(
                "Token file {} is unreadable. Run `twit auth`.",
                path.display()
            )
        })
    }

    pub fn save(&self) -> Result<()> {
        let path = Config::token_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            opts.mode(0o600);
            // Tighten a pre-existing file too.
            if path.exists() {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
        }
        opts.open(&path)
            .and_then(|mut f| f.write_all(serde_json::to_string_pretty(self)?.as_bytes()))
            .with_context(|| format!("Failed to write {}", path.display()))
    }

    pub fn expires_within(&self, secs: i64) -> bool {
        self.expires_at - chrono::Utc::now().timestamp() < secs
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

/// POST the token endpoint (Basic auth when a client secret is configured).
/// Returns (access_token, refresh_token, expires_at); never includes secrets in errors.
async fn token_request(
    http: &reqwest::Client,
    app: &OAuthApp,
    form: &[(&str, &str)],
) -> Result<(String, Option<String>, i64)> {
    let mut req = http.post(TOKEN_URL).form(form);
    if let Some(secret) = app.client_secret.as_deref().filter(|s| !s.is_empty()) {
        req = req.basic_auth(&app.client_id, Some(secret));
    }
    let resp = req.send().await.context("Token request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let msg = ["error_description", "error", "detail", "title"]
            .iter()
            .find_map(|k| v.get(k).and_then(|s| s.as_str()))
            .unwrap_or("no details");
        anyhow::bail!("Token endpoint returned HTTP {}: {}", status.as_u16(), msg);
    }
    let t: TokenResponse =
        serde_json::from_str(&body).context("Unexpected token endpoint response")?;
    let expires_at = chrono::Utc::now().timestamp() + t.expires_in.unwrap_or(7200);
    Ok((t.access_token, t.refresh_token, expires_at))
}

/// Exchange the stored refresh token for a new access token; updates `token` in place.
pub async fn refresh(http: &reqwest::Client, app: &OAuthApp, token: &mut Token) -> Result<()> {
    let (access, refresh, expires_at) = token_request(
        http,
        app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &token.refresh_token),
            ("client_id", &app.client_id),
        ],
    )
    .await?;
    token.access_token = access;
    // Refresh tokens rotate; keep the old one only if none was returned.
    if let Some(r) = refresh {
        token.refresh_token = r;
    }
    token.expires_at = expires_at;
    Ok(())
}

fn random_string(n: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

/// Run the browser flow and return fresh tokens (user_id/username still empty).
pub async fn authorize(http: &reqwest::Client, app: &OAuthApp) -> Result<Token> {
    let redirect = Url::parse(&app.redirect_uri).context("Invalid redirect_uri")?;
    let host = redirect.host_str().context("redirect_uri has no host")?;
    let port = redirect
        .port_or_known_default()
        .context("redirect_uri has no port")?;
    let listener = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("Cannot listen on {host}:{port} (is it in use?)"))?;

    let verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(32);
    let url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={SCOPES}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        urlencoding::encode(&app.client_id),
        urlencoding::encode(&app.redirect_uri),
    );
    println!("Open this URL in your browser to authorize twit:\n\n{url}\n");
    let _ = open::that(&url);
    println!("Waiting for the redirect on {} ...", app.redirect_uri);

    let code = tokio::time::timeout(
        CALLBACK_TIMEOUT,
        wait_for_code(&listener, &redirect, &state),
    )
    .await
    .context("Timed out waiting for authorization")??;

    let (access_token, refresh_token, expires_at) = token_request(
        http,
        app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &app.redirect_uri),
            ("code_verifier", &verifier),
            ("client_id", &app.client_id),
        ],
    )
    .await?;
    Ok(Token {
        access_token,
        refresh_token: refresh_token
            .context("No refresh token returned (is the offline.access scope enabled?)")?,
        expires_at,
        ..Token::default()
    })
}

/// Accept connections until the callback path is hit; verify `state`, return `code`.
async fn wait_for_code(listener: &TcpListener, redirect: &Url, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 8192 {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let head = String::from_utf8_lossy(&buf);
        let target = head.split_whitespace().nth(1).unwrap_or("/");
        let Ok(req) = Url::parse("http://localhost").and_then(|b| b.join(target)) else {
            continue;
        };
        if req.path() != redirect.path() {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let param = |k: &str| {
            req.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        let result = if param("state").as_deref() != Some(state) {
            Err(anyhow::anyhow!("OAuth state mismatch; aborting"))
        } else if let Some(err) = param("error") {
            Err(anyhow::anyhow!("Authorization denied: {err}"))
        } else {
            param("code").context("Callback is missing the authorization code")
        };
        let msg = if result.is_ok() {
            "twit is authorized. You can close this tab."
        } else {
            "Authorization failed. See the terminal."
        };
        let body = format!("<!doctype html><meta charset=utf-8><title>twit</title><p>{msg}</p>");
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return result;
    }
}
