//! APNs HTTP/2 client.
//!
//! Apple's auth model: ES256 JWT signed with the .p8 private key, valid for
//! up to 1 hour. We mint a token, cache it, refresh just before expiry. Each
//! push is a POST to `/3/device/{token}` with the JWT as `authorization` and
//! the app's bundle ID as `apns-topic`.

use anyhow::{Context, Result};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::Client as HttpClient;
use serde::Serialize;
use std::{path::Path, sync::Mutex, time::{Duration, Instant}};
use tracing::{debug, error, info, warn};

const APNS_PROD: &str = "https://api.push.apple.com";
const APNS_SANDBOX: &str = "https://api.sandbox.push.apple.com";

/// Refresh the JWT slightly before Apple's 1-hour cap — Apple rejects tokens
/// older than 60 minutes with `ExpiredProviderToken`.
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    iat: u64,
}

pub struct Client {
    http:        HttpClient,
    base_url:    &'static str,
    enc_key:     EncodingKey,
    key_id:      String,
    team_id:     String,
    cached:      Mutex<Option<(String, Instant)>>,
}

#[derive(Debug)]
pub enum PushOutcome {
    Delivered,
    /// Apple rejected the device token as no-longer-registered. Caller should
    /// drop the row from the subscriptions table.
    InvalidToken,
    /// Other failure; transient or config-related. Logged but not surfaced
    /// per-token to /notify callers.
    Failed(String),
}

impl Client {
    pub fn new(p8_path: &Path, key_id: String, team_id: String, production: bool) -> Result<Self> {
        let pem = std::fs::read(p8_path)
            .with_context(|| format!("read APNs key {}", p8_path.display()))?;
        let enc_key = EncodingKey::from_ec_pem(&pem)
            .context("APNs key must be a PKCS#8 PEM EC private key (.p8 from Apple Developer)")?;
        let http = HttpClient::builder()
            .http2_prior_knowledge()
            .pool_idle_timeout(Duration::from_secs(60 * 30))
            .build()
            .context("build HTTP/2 client")?;
        info!("[apns] client initialized; gateway={} key_id={}",
            if production { APNS_PROD } else { APNS_SANDBOX }, key_id);
        Ok(Self {
            http,
            base_url: if production { APNS_PROD } else { APNS_SANDBOX },
            enc_key,
            key_id,
            team_id,
            cached: Mutex::new(None),
        })
    }

    fn jwt(&self) -> Result<String> {
        let mut cached = self.cached.lock().unwrap();
        if let Some((tok, minted)) = cached.as_ref() {
            if minted.elapsed() < TOKEN_TTL {
                debug!("[apns] JWT cache hit; age={}s", minted.elapsed().as_secs());
                return Ok(tok.clone());
            }
        }
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let claims = JwtClaims { iss: &self.team_id, iat: now };
        let tok = encode(&header, &claims, &self.enc_key).context("sign APNs JWT")?;
        *cached = Some((tok.clone(), Instant::now()));
        info!("[apns] minted new ES256 JWT; key_id={} iat={}", self.key_id, now);
        Ok(tok)
    }

    /// Send an alert push. `payload` should be a complete APNs payload object
    /// (i.e. include the `aps` key); the relay just wraps in HTTP/2 + auth.
    pub async fn push(&self, device_token: &str, bundle_id: &str, payload: &serde_json::Value) -> PushOutcome {
        self.send(device_token, bundle_id, payload, "alert", "10").await
    }

    /// Send a silent/background push (`content-available`, no alert). Apple
    /// requires `apns-push-type: background` and `apns-priority: 5` for these;
    /// the relay uses them to deliver registration-challenge nonces.
    pub async fn push_background(&self, device_token: &str, bundle_id: &str, payload: &serde_json::Value) -> PushOutcome {
        self.send(device_token, bundle_id, payload, "background", "5").await
    }

    async fn send(
        &self,
        device_token: &str,
        bundle_id: &str,
        payload: &serde_json::Value,
        push_type: &str,
        priority: &str,
    ) -> PushOutcome {
        let tok_tail = tail4(device_token);
        let jwt = match self.jwt() {
            Ok(j) => j,
            Err(e) => {
                error!("[apns] JWT mint failed for token=…{tok_tail}: {e:#}");
                return PushOutcome::Failed(format!("jwt: {e}"));
            }
        };
        let url = format!("{}/3/device/{device_token}", self.base_url);
        debug!("[apns] POST /3/device for token=…{tok_tail} topic={bundle_id} type={push_type}");
        let res = self.http
            .post(&url)
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-topic", bundle_id)
            .header("apns-push-type", push_type)
            .header("apns-priority", priority)
            .json(payload)
            .send()
            .await;
        match res {
            Ok(r) => {
                let status = r.status();
                let apns_id = r.headers()
                    .get("apns-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                if status.is_success() {
                    debug!("[apns] delivered token=…{tok_tail} status={status} apns-id={apns_id}");
                    PushOutcome::Delivered
                } else if status == 410 {
                    warn!("[apns] token rejected (410 Unregistered) token=…{tok_tail} apns-id={apns_id}");
                    PushOutcome::InvalidToken
                } else {
                    let body = r.text().await.unwrap_or_default();
                    if status == 400 && body.contains("BadDeviceToken") {
                        warn!("[apns] token rejected (400 BadDeviceToken) token=…{tok_tail} apns-id={apns_id}");
                        PushOutcome::InvalidToken
                    } else {
                        error!("[apns] push failed token=…{tok_tail} status={status} apns-id={apns_id}: {body}");
                        PushOutcome::Failed(format!("APNs {status}: {body}"))
                    }
                }
            }
            Err(e) => {
                error!("[apns] HTTP request error for token=…{tok_tail}: {e}");
                PushOutcome::Failed(e.to_string())
            }
        }
    }
}

/// Last 4 chars of a device token, for log correlation without exposing the
/// full token (PII / push-target secret).
fn tail4(s: &str) -> &str {
    if s.len() <= 4 { s } else { &s[s.len() - 4..] }
}
