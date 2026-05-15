//! HTTP handlers.
//!
//! `/register` and `/unregister` are unauthenticated — accepting fake
//! (token, pubkey) pairs costs nothing because a real lair pushing to a
//! non-existent device tokens just gets dropped at APNs.
//!
//! `/notify` requires an Ed25519 signature over the request body, with the
//! pubkey supplied in the `X-Lair-Pubkey` header (base32, no padding). Fresh
//! `ts` (within 60s of server clock) and unique `nonce` per pubkey prevent
//! replay.

use crate::{apns::PushOutcome, AppState};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

const FRESH_WINDOW_SECS: i64 = 60;

pub async fn health() -> &'static str {
    debug!("[routes] /health probe");
    "ok"
}

#[derive(Deserialize)]
pub struct RegisterBody {
    device_token: String,
    platform:     String,
    lair_pubkey:  String,
}

pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<RegisterBody>,
) -> impl IntoResponse {
    debug!("[routes] /register received; platform={} pubkey={}", body.platform, body.lair_pubkey);
    if !matches!(body.platform.as_str(), "ios" | "android") {
        warn!("[routes] /register rejected: bad platform={}", body.platform);
        return (StatusCode::BAD_REQUEST, "platform must be ios|android").into_response();
    }
    if body.device_token.is_empty() || body.lair_pubkey.is_empty() {
        warn!("[routes] /register rejected: empty device_token or lair_pubkey");
        return (StatusCode::BAD_REQUEST, "device_token and lair_pubkey required").into_response();
    }
    if let Err(e) = state.db.upsert_subscription(&body.device_token, &body.platform, &body.lair_pubkey) {
        error!("[routes] /register db error: {e:#}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
    }
    info!("[routes] /register: device registered; platform={} pubkey={} token=…{}",
        body.platform,
        body.lair_pubkey,
        tail4(&body.device_token));
    StatusCode::NO_CONTENT.into_response()
}

pub async fn unregister(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<RegisterBody>,
) -> impl IntoResponse {
    debug!("[routes] /unregister received; pubkey={} token=…{}",
        body.lair_pubkey, tail4(&body.device_token));
    match state.db.delete_subscription(&body.device_token, &body.lair_pubkey) {
        Ok(n) => {
            info!("[routes] /unregister: device unregistered; removed {n} row(s) for pubkey={} token=…{}",
                body.lair_pubkey, tail4(&body.device_token));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            error!("[routes] /unregister db error: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct NotifyBody {
    ts:       i64,
    nonce:    String,
    /// Stable category (e.g. "task_complete", "turn_done"). Mobile localises
    /// this client-side so the relay never sees prose.
    category: String,
    /// Optional title and body if the lair operator wants the relay to set
    /// them directly (loud-push mode). Skipped in silent-push mode — pass an
    /// empty `aps` object and content-available semantics via the mobile.
    #[serde(default)] title: Option<String>,
    #[serde(default)] body:  Option<String>,
}

pub async fn notify(
    State(state): State<Arc<AppState>>,
    headers:      HeaderMap,
    raw:          Bytes,
) -> impl IntoResponse {
    debug!("[routes] /notify received; body={} bytes", raw.len());
    let pubkey_b32 = match headers.get("x-lair-pubkey").and_then(|v| v.to_str().ok()) {
        Some(s) => s.to_string(),
        None    => {
            warn!("[routes] /notify rejected: missing X-Lair-Pubkey");
            return (StatusCode::BAD_REQUEST, "missing X-Lair-Pubkey").into_response();
        }
    };
    let sig_b64 = match headers.get("x-lair-sig").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None    => {
            warn!("[routes] /notify rejected: missing X-Lair-Sig pubkey={pubkey_b32}");
            return (StatusCode::BAD_REQUEST, "missing X-Lair-Sig").into_response();
        }
    };

    let pubkey_bytes = match base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &pubkey_b32) {
        Some(v) if v.len() == PUBLIC_KEY_LENGTH => v,
        _ => {
            warn!("[routes] /notify rejected: X-Lair-Pubkey bad base32 / wrong length");
            return (StatusCode::BAD_REQUEST, "X-Lair-Pubkey: bad base32 / wrong length").into_response();
        }
    };
    let pk_arr: [u8; PUBLIC_KEY_LENGTH] = pubkey_bytes.try_into().unwrap();
    let verifying = match VerifyingKey::from_bytes(&pk_arr) {
        Ok(v) => v,
        Err(_) => {
            warn!("[routes] /notify rejected: X-Lair-Pubkey not a valid Ed25519 key pubkey={pubkey_b32}");
            return (StatusCode::BAD_REQUEST, "X-Lair-Pubkey: not a valid Ed25519 key").into_response();
        }
    };

    let sig_bytes = match B64.decode(sig_b64) {
        Ok(v) if v.len() == SIGNATURE_LENGTH => v,
        _ => {
            warn!("[routes] /notify rejected: X-Lair-Sig bad base64 / wrong length pubkey={pubkey_b32}");
            return (StatusCode::BAD_REQUEST, "X-Lair-Sig: bad base64 / wrong length").into_response();
        }
    };
    let sig_arr: [u8; SIGNATURE_LENGTH] = sig_bytes.try_into().unwrap();
    let signature = Signature::from_bytes(&sig_arr);

    if verifying.verify(&raw, &signature).is_err() {
        warn!("[routes] /notify rejected: Ed25519 signature mismatch pubkey={pubkey_b32}");
        return (StatusCode::UNAUTHORIZED, "signature mismatch").into_response();
    }
    debug!("[routes] /notify signature verified pubkey={pubkey_b32}");

    let body: NotifyBody = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => {
            warn!("[routes] /notify rejected: malformed body pubkey={pubkey_b32}: {e}");
            return (StatusCode::BAD_REQUEST, format!("body: {e}")).into_response();
        }
    };

    let now = unix_now();
    if (now - body.ts).abs() > FRESH_WINDOW_SECS {
        warn!("[routes] /notify rejected: ts skew pubkey={pubkey_b32} skew={}s", now - body.ts);
        return (StatusCode::UNAUTHORIZED, "ts skew").into_response();
    }
    match state.db.record_nonce(&pubkey_b32, &body.nonce) {
        Ok(true)  => {}
        Ok(false) => {
            warn!("[routes] /notify rejected: nonce replay pubkey={pubkey_b32}");
            return (StatusCode::UNAUTHORIZED, "nonce replay").into_response();
        }
        Err(e) => {
            error!("[routes] /notify nonce db error pubkey={pubkey_b32}: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }

    let subs = match state.db.subscriptions_for_pubkey(&pubkey_b32) {
        Ok(v) => v,
        Err(e) => {
            error!("[routes] /notify subscription lookup error pubkey={pubkey_b32}: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    };
    debug!("[routes] /notify resolved {} subscription(s) pubkey={pubkey_b32}", subs.len());
    if subs.is_empty() {
        // Authentic push but no devices opted in. Nothing to do; respond 200
        // so the lair can prune its own retry queue.
        info!("[routes] /notify accepted but no subscribers; pubkey={pubkey_b32} category={}", body.category);
        return (StatusCode::OK, Json(json!({"delivered": 0, "invalid": 0}))).into_response();
    }

    let payload = build_aps(&body);

    let mut delivered = 0usize;
    let mut invalid   = 0usize;
    for s in &subs {
        if s.platform != "ios" {
            // FCM not wired yet — silently skip.
            debug!("[routes] /notify skipping non-iOS subscriber; platform={}", s.platform);
            continue;
        }
        match state.apns.push(&s.device_token, &state.bundle_id, &payload).await {
            PushOutcome::Delivered    => delivered += 1,
            PushOutcome::InvalidToken => {
                invalid += 1;
                warn!("[routes] /notify dropping invalid token=…{} pubkey={pubkey_b32}",
                    tail4(&s.device_token));
                if let Err(e) = state.db.forget_invalid_token(&s.device_token) {
                    error!("[routes] /notify failed to forget invalid token=…{}: {e:#}",
                        tail4(&s.device_token));
                }
            }
            PushOutcome::Failed(msg) => {
                warn!("[routes] /notify APNs failure for token=…{}: {msg}", tail4(&s.device_token));
            }
        }
    }
    info!("[routes] /notify forwarded; pubkey={pubkey_b32} category={} subs={} delivered={} invalid={}",
        body.category, subs.len(), delivered, invalid);
    (StatusCode::OK, Json(json!({"delivered": delivered, "invalid": invalid}))).into_response()
}

fn build_aps(body: &NotifyBody) -> serde_json::Value {
    let mut alert = serde_json::Map::new();
    if let Some(t) = &body.title { alert.insert("title".into(), json!(t)); }
    if let Some(b) = &body.body  { alert.insert("body".into(),  json!(b)); }
    let aps = if alert.is_empty() {
        // No title/body — silent push so mobile wakes and pulls real content
        // from lair over the Noise tunnel. Never seen by APNs as alert text.
        json!({ "content-available": 1 })
    } else {
        json!({
            "alert": alert,
            "sound": "default",
        })
    };
    json!({
        "aps":      aps,
        "category": body.category,
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn tail4(s: &str) -> &str {
    if s.len() <= 4 { s } else { &s[s.len() - 4..] }
}
