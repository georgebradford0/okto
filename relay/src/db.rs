//! SQLite-backed registry for device subscriptions + nonce replay-cache.
//!
//! Subscriptions are keyed by `(device_token, lair_pubkey)` so a device can
//! authorise multiple lairs and a lair can fan out to multiple devices.
//! Nonces prevent /notify replay within a 60s window — we GC old rows on
//! every insert so the table stays bounded.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::{path::Path, sync::Mutex};
use tracing::{debug, error, info};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subscriptions (
    device_token TEXT NOT NULL,
    platform     TEXT NOT NULL CHECK(platform IN ('ios','android')),
    lair_pubkey  TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (device_token, lair_pubkey)
);
CREATE INDEX IF NOT EXISTS idx_subs_pubkey ON subscriptions(lair_pubkey);

CREATE TABLE IF NOT EXISTS nonces (
    pubkey  TEXT NOT NULL,
    nonce   TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (pubkey, nonce)
);
CREATE INDEX IF NOT EXISTS idx_nonces_seen ON nonces(seen_at);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub device_token: String,
    pub platform:     String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        info!("[db] opened SQLite database at {}", path.display());
        conn.execute_batch(SCHEMA).map_err(|e| {
            error!("[db] schema migration failed: {e}");
            e
        }).context("apply schema")?;
        info!("[db] schema migrations applied (subscriptions, nonces)");
        // Pragmas for the access pattern: tiny writes, point reads.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn upsert_subscription(&self, device_token: &str, platform: &str, lair_pubkey: &str) -> Result<()> {
        let now = unix_now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO subscriptions(device_token, platform, lair_pubkey, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_token, lair_pubkey) DO UPDATE SET platform=excluded.platform",
            params![device_token, platform, lair_pubkey, now],
        ).map_err(|e| {
            error!("[db] upsert_subscription failed for pubkey={lair_pubkey}: {e}");
            e
        })?;
        debug!("[db] subscription upserted; platform={platform} pubkey={lair_pubkey}");
        Ok(())
    }

    pub fn delete_subscription(&self, device_token: &str, lair_pubkey: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM subscriptions WHERE device_token = ?1 AND lair_pubkey = ?2",
            params![device_token, lair_pubkey],
        ).map_err(|e| {
            error!("[db] delete_subscription failed for pubkey={lair_pubkey}: {e}");
            e
        })?;
        debug!("[db] delete_subscription removed {n} row(s) for pubkey={lair_pubkey}");
        Ok(n)
    }

    pub fn subscriptions_for_pubkey(&self, lair_pubkey: &str) -> Result<Vec<Subscription>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT device_token, platform FROM subscriptions WHERE lair_pubkey = ?1",
        ).map_err(|e| {
            error!("[db] subscriptions_for_pubkey prepare failed: {e}");
            e
        })?;
        let rows = stmt.query_map(params![lair_pubkey], |r| {
            Ok(Subscription {
                device_token: r.get(0)?,
                platform:     r.get(1)?,
            })
        }).map_err(|e| {
            error!("[db] subscriptions_for_pubkey query failed for pubkey={lair_pubkey}: {e}");
            e
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        debug!("[db] subscriptions_for_pubkey returned {} row(s) for pubkey={lair_pubkey}", out.len());
        Ok(out)
    }

    /// Atomically check + record a nonce for replay protection. Returns
    /// `Ok(true)` if the nonce is fresh (and was recorded), `Ok(false)` if
    /// it's been seen before for this pubkey within the retention window.
    pub fn record_nonce(&self, pubkey: &str, nonce: &str) -> Result<bool> {
        let now = unix_now();
        let conn = self.conn.lock().unwrap();
        // GC nonces older than 5 minutes — well past the 60s freshness check
        // applied to ts in the route handler.
        let gc = conn.execute(
            "DELETE FROM nonces WHERE seen_at < ?1",
            params![now - 300],
        ).map_err(|e| {
            error!("[db] nonce GC failed: {e}");
            e
        })?;
        if gc > 0 {
            debug!("[db] GC'd {gc} expired nonce row(s)");
        }
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO nonces(pubkey, nonce, seen_at) VALUES (?1, ?2, ?3)",
            params![pubkey, nonce, now],
        ).map_err(|e| {
            error!("[db] record_nonce insert failed for pubkey={pubkey}: {e}");
            e
        })?;
        Ok(inserted == 1)
    }

    pub fn forget_invalid_token(&self, device_token: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM subscriptions WHERE device_token = ?1",
            params![device_token],
        ).map_err(|e| {
            error!("[db] forget_invalid_token failed: {e}");
            e
        })?;
        info!("[db] forgot invalid device token; removed {n} subscription row(s)");
        Ok(())
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

