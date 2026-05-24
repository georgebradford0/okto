//! HTTP handlers.
//!
//! `/register` is a two-step, ownership-proving flow. `POST /register/challenge`
//! makes the relay send a silent push carrying a random nonce to the named
//! device token; only the device that actually holds that token can receive
//! it. `POST /register` must then echo that nonce back — so a caller who
//! merely *knows* a device token cannot bind it to a key they control. The
//! nonce is never returned in an HTTP response; it travels solely via the push.
//!
//! `/unregister` only validates the `lair_pubkey` shape; a caller who knows a
//! victim's token + pubkey can delete that subscription (the victim
//! re-registers on next chat-mount) — a lower-stakes gap tracked separately.
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
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

const FRESH_WINDOW_SECS: i64 = 60;

/// Upper bound on a stored `device_token`. APNs tokens are 32 bytes (64 hex
/// chars) and FCM tokens run longer; 512 clears both and caps how much an
/// unauthenticated `/register` can persist per row.
const MAX_DEVICE_TOKEN_LEN: usize = 512;

/// A registration-challenge nonce is valid this long after issue — the window
/// in which the device must receive the silent push and echo the nonce back
/// to `POST /register`.
const CHALLENGE_TTL_SECS: i64 = 300;

/// Minimum gap between challenge pushes for the same device token. Caps how
/// fast a caller can make the relay push at a device they do not control.
const CHALLENGE_COOLDOWN_SECS: i64 = 30;

/// A fresh 128-bit registration-challenge nonce, hex-encoded.
fn gen_nonce() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// True if `s` is a syntactically valid relay-signing public key: RFC4648
/// base32 (no padding) decoding to a 32-byte Ed25519 key. Mirrors the check
/// `/notify` applies to the `X-Lair-Pubkey` header.
fn valid_lair_pubkey(s: &str) -> bool {
    match base32::decode(base32::Alphabet::Rfc4648 { padding: false }, s) {
        Some(v) if v.len() == PUBLIC_KEY_LENGTH => {
            let arr: [u8; PUBLIC_KEY_LENGTH] = v.try_into().unwrap();
            VerifyingKey::from_bytes(&arr).is_ok()
        }
        _ => false,
    }
}

pub async fn health() -> &'static str {
    debug!("[routes] /health probe");
    "ok"
}

#[derive(Deserialize)]
pub struct RegisterBody {
    device_token: String,
    platform:     String,
    lair_pubkey:  String,
    /// Required by `/register` — the nonce from the challenge push. Ignored by
    /// `/unregister`, which shares this struct.
    #[serde(default)]
    challenge_nonce: Option<String>,
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
    if body.device_token.len() > MAX_DEVICE_TOKEN_LEN {
        warn!("[routes] /register rejected: device_token over {MAX_DEVICE_TOKEN_LEN} bytes");
        return (StatusCode::BAD_REQUEST, "device_token too long").into_response();
    }
    if !valid_lair_pubkey(&body.lair_pubkey) {
        warn!("[routes] /register rejected: lair_pubkey not a valid base32 Ed25519 key");
        return (StatusCode::BAD_REQUEST, "lair_pubkey: not a valid base32 Ed25519 key").into_response();
    }
    // Proof of device-token ownership: the caller must echo the nonce the
    // relay pushed to this token in the /register/challenge step. A caller
    // who only knows the token string never received that push.
    let nonce = match body.challenge_nonce.as_deref() {
        Some(n) if !n.is_empty() => n,
        _ => {
            warn!("[routes] /register rejected: missing challenge_nonce");
            return (StatusCode::UNAUTHORIZED, "challenge_nonce required — call /register/challenge first").into_response();
        }
    };
    match state.db.consume_challenge(&body.device_token, nonce, CHALLENGE_TTL_SECS) {
        Ok(true) => {}
        Ok(false) => {
            warn!("[routes] /register rejected: invalid or expired challenge; token=…{}", tail4(&body.device_token));
            return (StatusCode::UNAUTHORIZED, "invalid or expired challenge_nonce").into_response();
        }
        Err(e) => {
            error!("[routes] /register challenge check db error: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
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

#[derive(Deserialize)]
pub struct ChallengeBody {
    device_token: String,
    platform:     String,
}

/// Step one of registration: prove the caller controls `device_token` by
/// having the relay push a nonce to it. The nonce is recorded server-side and
/// delivered *only* via APNs — never in this response (a 202 with no body) —
/// so a caller who merely knows the token string cannot learn it.
pub async fn register_challenge(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<ChallengeBody>,
) -> impl IntoResponse {
    debug!("[routes] /register/challenge received; platform={}", body.platform);
    if body.platform != "ios" {
        warn!("[routes] /register/challenge rejected: unsupported platform={}", body.platform);
        return (StatusCode::BAD_REQUEST, "platform must be ios").into_response();
    }
    if body.device_token.is_empty() || body.device_token.len() > MAX_DEVICE_TOKEN_LEN {
        warn!("[routes] /register/challenge rejected: bad device_token length");
        return (StatusCode::BAD_REQUEST, "device_token required, <= 512 bytes").into_response();
    }
    // APNs device tokens are hex; rejecting non-hex also keeps the value safe
    // to interpolate into the APNs request path.
    if !body.device_token.bytes().all(|b| b.is_ascii_hexdigit()) {
        warn!("[routes] /register/challenge rejected: device_token not hex");
        return (StatusCode::BAD_REQUEST, "device_token must be hex").into_response();
    }

    let nonce = gen_nonce();
    match state.db.upsert_challenge(&body.device_token, &nonce, CHALLENGE_COOLDOWN_SECS, CHALLENGE_TTL_SECS) {
        Ok(true) => {
            // Silent push: wakes the app and carries the nonce, shows no alert.
            let payload = json!({
                "aps": { "content-available": 1 },
                "okto_challenge": nonce,
            });
            match state.apns.push_background(&body.device_token, &state.bundle_id, &payload).await {
                PushOutcome::Delivered => {
                    info!("[routes] /register/challenge: nonce push sent; token=…{}", tail4(&body.device_token));
                }
                PushOutcome::InvalidToken => {
                    warn!("[routes] /register/challenge: APNs rejected token=…{}", tail4(&body.device_token));
                    if let Err(e) = state.db.forget_invalid_token(&body.device_token) {
                        error!("[routes] /register/challenge: failed to forget invalid token: {e:#}");
                    }
                    return (StatusCode::BAD_REQUEST, "device_token rejected by APNs").into_response();
                }
                PushOutcome::Failed(msg) => {
                    warn!("[routes] /register/challenge: APNs push failed token=…{}: {msg}", tail4(&body.device_token));
                    return (StatusCode::BAD_GATEWAY, "push delivery failed").into_response();
                }
            }
        }
        Ok(false) => {
            // Within cooldown — the prior challenge is still valid, so the
            // device can finish /register with the nonce it already received.
            info!("[routes] /register/challenge: within cooldown, no new push; token=…{}", tail4(&body.device_token));
        }
        Err(e) => {
            error!("[routes] /register/challenge db error: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response();
        }
    }
    // 202 with an empty body — the nonce is delivered solely via the push.
    StatusCode::ACCEPTED.into_response()
}

pub async fn unregister(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<RegisterBody>,
) -> impl IntoResponse {
    debug!("[routes] /unregister received; pubkey={} token=…{}",
        body.lair_pubkey, tail4(&body.device_token));
    if !valid_lair_pubkey(&body.lair_pubkey) {
        warn!("[routes] /unregister rejected: lair_pubkey not a valid base32 Ed25519 key");
        return (StatusCode::BAD_REQUEST, "lair_pubkey: not a valid base32 Ed25519 key").into_response();
    }
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
