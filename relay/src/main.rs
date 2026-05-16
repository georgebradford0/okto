//! octo-relay — push-notification relay for self-hosted lair instances.
//!
//! Architecture: lair has its own keypair; mobile pairs with lair over the
//! Noise tunnel and learns the lair's Ed25519 *relay-signing* pubkey, then
//! POSTs `/register` here with `(device_token, platform, lair_pubkey)`. Lair
//! emits `/notify` POSTs signed with its private Ed25519 key; this relay
//! verifies the signature, looks up authorised device tokens, and forwards
//! to APNs (FCM later). The relay never sees Noise traffic and never holds
//! per-user secrets — only Apple's APNs key.

use anyhow::Context;
use axum::{routing::{get, post}, Router};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{fmt, EnvFilter};

mod apns;
mod db;
mod routes;

#[derive(Parser, Debug, Clone)]
#[command(name = "octo-relay", version)]
struct Args {
    /// Address to bind. Caddy (or another TLS terminator) reverse-proxies
    /// 443 → this address. Bind to 127.0.0.1 in production.
    #[arg(long, env = "RELAY_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// SQLite database path. Stores subscriptions and replay-protection nonces.
    #[arg(long, env = "RELAY_DB_PATH", default_value = "/var/lib/octo-relay/relay.db")]
    db_path: PathBuf,

    /// Apple .p8 private key file (PKCS#8 PEM).
    #[arg(long, env = "APNS_P8_PATH", default_value = "/etc/octo-relay/apns.p8")]
    apns_p8: PathBuf,

    /// APNs Auth Key ID (10 chars).
    #[arg(long, env = "APNS_KEY_ID")]
    apns_key_id: String,

    /// Apple Developer Team ID (10 chars).
    #[arg(long, env = "APNS_TEAM_ID")]
    apns_team_id: String,

    /// iOS app Bundle ID (the APNs `apns-topic`).
    #[arg(long, env = "APNS_BUNDLE_ID")]
    apns_bundle_id: String,

    /// Use Apple's production APNs gateway. Set to `false` while building
    /// against TestFlight/sandbox tokens.
    #[arg(long, env = "APNS_PRODUCTION", default_value_t = true, action = clap::ArgAction::Set)]
    apns_production: bool,

    /// Subscriptions not re-registered within this many days are pruned. The
    /// mobile client re-registers on every chat-mount, so live devices stay;
    /// abandoned or abusively-created rows age out, bounding table growth.
    #[arg(long, env = "RELAY_SUBSCRIPTION_TTL_DAYS", default_value_t = 30)]
    subscription_ttl_days: i64,
}

pub struct AppState {
    pub db:   db::Db,
    pub apns: apns::Client,
    pub bundle_id: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,octo_relay=debug")))
        .init();

    let args = Args::parse();
    tracing::info!("[relay] starting; listen={} bundle={} production={}", args.listen, args.apns_bundle_id, args.apns_production);

    let db = db::Db::open(&args.db_path).context("open db")?;
    let apns = apns::Client::new(
        &args.apns_p8,
        args.apns_key_id.clone(),
        args.apns_team_id.clone(),
        args.apns_production,
    ).context("init apns client")?;

    let state = Arc::new(AppState {
        db,
        apns,
        bundle_id: args.apns_bundle_id,
    });

    // Periodically prune subscriptions that no live device has refreshed.
    {
        let state = state.clone();
        let ttl_secs = args.subscription_ttl_days.max(1) * 86_400;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
            loop {
                tick.tick().await;
                match state.db.prune_stale_subscriptions(unix_now() - ttl_secs) {
                    Ok(n)  => tracing::debug!("[relay] subscription prune: {n} removed"),
                    Err(e) => tracing::warn!("[relay] subscription prune failed: {e:#}"),
                }
            }
        });
    }

    let app = Router::new()
        .route("/health",             get(routes::health))
        .route("/register",           post(routes::register))
        .route("/register/challenge", post(routes::register_challenge))
        .route("/unregister",         post(routes::unregister))
        .route("/notify",             post(routes::notify))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(args.listen).await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!("[relay] listening on {}", args.listen);
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
