//! Push-notification plumbing: device-token registry + APNs sender.
//!
//! The mobile client registers its APNs device token over the existing /stream
//! WebSocket via `register_push_token { token, platform }`. Lair persists the
//! tokens in `/data/push_tokens.json` and, when a background task finishes,
//! sends an APNs notification to every registered iOS device.
//!
//! APNs config comes from env (set on the lair Deployment via lair-secrets):
//!
//! | Env var            | Required | Notes                                   |
//! |--------------------|----------|-----------------------------------------|
//! | `APNS_KEY_P8`      | yes      | Full PEM contents of the .p8 file        |
//! | `APNS_KEY_ID`      | yes      | 10-char Key ID from Apple Developer      |
//! | `APNS_TEAM_ID`     | yes      | 10-char Team ID                          |
//! | `APNS_BUNDLE_ID`   | yes      | The iOS app bundle identifier            |
//! | `APNS_USE_SANDBOX` | no       | "true"/"false". Default true (Dev/TF).   |
//!
//! When any required var is missing the sender stays disabled and pushes are
//! silently no-ops; the in-app `system` event still fires.

use a2::{Client, ClientConfig, Endpoint, NotificationBuilder, NotificationOptions, DefaultNotificationBuilder};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    path::PathBuf,
    sync::Mutex,
    time::SystemTime,
};
use tracing::{error, info, warn};

const TOKENS_FILE: &str = "push_tokens.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PushToken {
    pub platform:        String,
    pub token:           String,
    /// Unix timestamp seconds. Useful for expiring stale tokens later.
    pub registered_at:   u64,
}

/// Persistent list of registered device tokens. Wraps a `Mutex<Vec<_>>` and
/// rewrites the JSON file in full on every mutation; the volume is tiny (one
/// token per active device) so this is fine.
pub struct PushTokenRegistry {
    path:   PathBuf,
    tokens: Mutex<Vec<PushToken>>,
}

impl PushTokenRegistry {
    /// Load the registry from `<data_dir>/push_tokens.json`. Returns an empty
    /// registry if the file is missing or malformed (logging the error).
    pub fn load(data_dir: &std::path::Path) -> Self {
        let path = data_dir.join(TOKENS_FILE);
        let tokens: Vec<PushToken> = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v)  => { info!("[push] loaded {} token(s) from {}", (&v as &Vec<_>).len(), path.display()); v }
                Err(e) => { warn!("[push] failed to parse {}: {e} — starting empty", path.display()); Vec::new() }
            },
            Err(_) => Vec::new(),
        };
        Self { path, tokens: Mutex::new(tokens) }
    }

    fn save(&self, tokens: &[PushToken]) {
        if let Some(parent) = self.path.parent() { std::fs::create_dir_all(parent).ok(); }
        match serde_json::to_string_pretty(tokens) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    error!("[push] failed to save {}: {e}", self.path.display());
                }
            }
            Err(e) => error!("[push] failed to serialize tokens: {e}"),
        }
    }

    /// Add the (platform, token) pair if not already present. Refreshes
    /// `registered_at` on existing entries so we can evict stale ones later.
    pub fn register(&self, platform: &str, token: &str) {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(existing) = tokens.iter_mut().find(|t| t.platform == platform && t.token == token) {
            existing.registered_at = now;
            info!("[push] refreshed {platform} token (…{})", tail(token));
        } else {
            tokens.push(PushToken {
                platform:      platform.to_string(),
                token:         token.to_string(),
                registered_at: now,
            });
            info!("[push] registered new {platform} token (…{}) — {} total", tail(token), tokens.len());
        }
        let snapshot = tokens.clone();
        drop(tokens);
        self.save(&snapshot);
    }

    /// Remove a token (called when APNs returns BadDeviceToken / Unregistered).
    pub fn remove(&self, platform: &str, token: &str) {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|t| !(t.platform == platform && t.token == token));
        if tokens.len() != before {
            info!("[push] removed {platform} token (…{}) — {} remaining", tail(token), tokens.len());
            let snapshot = tokens.clone();
            drop(tokens);
            self.save(&snapshot);
        }
    }

    pub fn list_for(&self, platform: &str) -> Vec<PushToken> {
        self.tokens.lock().unwrap().iter().filter(|t| t.platform == platform).cloned().collect()
    }
}

fn tail(s: &str) -> String {
    let n = s.len().saturating_sub(8);
    s[n..].to_string()
}

/// APNs sender. Constructed once at startup; clones cheaply.
#[derive(Clone)]
pub struct ApnsSender {
    client:    Client,
    bundle_id: String,
}

#[derive(Debug)]
pub enum ApnsSendOutcome {
    /// Apple accepted the notification.
    Ok,
    /// Token is invalid (BadDeviceToken / Unregistered) — caller should remove
    /// it from the registry.
    InvalidToken,
    /// Anything else (rate-limit, server error, network); leave the token alone
    /// and try again next time.
    Other(String),
}

impl ApnsSender {
    /// Build the sender from env vars. Returns `Ok(None)` (silently disabled)
    /// when any required var is missing — that's the expected configuration in
    /// dev and on operators who don't want push.
    pub fn from_env() -> Result<Option<Self>, String> {
        let p8       = match std::env::var("APNS_KEY_P8")    { Ok(v) if !v.trim().is_empty() => v, _ => return Ok(None) };
        let key_id   = match std::env::var("APNS_KEY_ID")    { Ok(v) if !v.trim().is_empty() => v, _ => { warn!("[push] APNS_KEY_P8 set but APNS_KEY_ID missing — push disabled"); return Ok(None) } };
        let team_id  = match std::env::var("APNS_TEAM_ID")   { Ok(v) if !v.trim().is_empty() => v, _ => { warn!("[push] APNS_KEY_P8 set but APNS_TEAM_ID missing — push disabled"); return Ok(None) } };
        let bundle   = match std::env::var("APNS_BUNDLE_ID") { Ok(v) if !v.trim().is_empty() => v, _ => { warn!("[push] APNS_KEY_P8 set but APNS_BUNDLE_ID missing — push disabled"); return Ok(None) } };
        let sandbox  = std::env::var("APNS_USE_SANDBOX").ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(true);

        let endpoint = if sandbox { Endpoint::Sandbox } else { Endpoint::Production };

        let client = Client::token(
            &mut Cursor::new(p8.as_bytes()),
            key_id.trim(),
            team_id.trim(),
            ClientConfig::new(endpoint),
        ).map_err(|e| format!("APNs client construction failed: {e}"))?;

        info!(
            "[push] APNs sender ready — bundle={bundle} endpoint={} team={team_id} key={key_id}",
            if sandbox { "sandbox" } else { "production" },
        );

        Ok(Some(Self { client, bundle_id: bundle }))
    }

    /// Send a single notification. The body is truncated to the first ~180
    /// chars for the alert (APNs payloads are capped at 4 KiB total).
    pub async fn send(&self, token: &str, title: &str, body: &str) -> ApnsSendOutcome {
        let preview: String = body.chars().take(180).collect();
        let payload = DefaultNotificationBuilder::new()
            .set_title(title)
            .set_body(&preview)
            .set_sound("default")
            .set_mutable_content()
            .build(token, NotificationOptions {
                apns_topic: Some(&self.bundle_id),
                ..Default::default()
            });

        match self.client.send(payload).await {
            Ok(resp) => {
                let code = resp.code;
                if (200..300).contains(&code) {
                    info!("[push] APNs accepted (token …{}) HTTP {code}", tail(token));
                    ApnsSendOutcome::Ok
                } else {
                    let reason = resp.error.as_ref().map(|e| format!("{e:?}")).unwrap_or_else(|| "(no reason)".into());
                    warn!("[push] APNs rejected (token …{}) HTTP {code} reason={reason}", tail(token));
                    if matches!(code, 400 | 410) || reason.contains("BadDeviceToken") || reason.contains("Unregistered") {
                        ApnsSendOutcome::InvalidToken
                    } else {
                        ApnsSendOutcome::Other(reason)
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("BadDeviceToken") || msg.contains("Unregistered") {
                    ApnsSendOutcome::InvalidToken
                } else {
                    ApnsSendOutcome::Other(msg)
                }
            }
        }
    }
}

/// Fan a (title, body) push out to every registered iOS token. Stale tokens
/// (BadDeviceToken / Unregistered) are pruned from the registry.
pub async fn push_to_ios(
    sender:   &ApnsSender,
    registry: &PushTokenRegistry,
    title:    &str,
    body:     &str,
) {
    let tokens = registry.list_for("ios");
    if tokens.is_empty() {
        info!("[push] no iOS tokens registered — skipping APNs send");
        return;
    }
    info!("[push] sending APNs to {} iOS device(s): {title}", tokens.len());
    for t in tokens {
        match sender.send(&t.token, title, body).await {
            ApnsSendOutcome::Ok => {}
            ApnsSendOutcome::InvalidToken => registry.remove("ios", &t.token),
            ApnsSendOutcome::Other(_) => {} // transient — leave the token alone
        }
    }
}
