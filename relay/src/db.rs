//! SQLite-backed registry for device subscriptions + nonce replay-cache.
//!
//! Subscriptions are keyed by `(device_token, lair_pubkey)` so a device can
//! authorise multiple lairs and a lair can fan out to multiple devices.
//! Nonces prevent /notify replay within a 60s window — we GC old rows on
//! every insert so the table stays bounded.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{path::Path, sync::Mutex};
use tracing::{debug, error, info};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subscriptions (
    device_token TEXT NOT NULL,
    platform     TEXT NOT NULL CHECK(platform IN ('ios','android')),
    lair_pubkey  TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    environment  TEXT NOT NULL DEFAULT 'production'
                 CHECK(environment IN ('sandbox','production')),
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

-- One pending registration challenge per device token. Created by
-- POST /register/challenge, consumed (deleted) by a matching POST /register;
-- stale rows are GC'd on every insert.
CREATE TABLE IF NOT EXISTS register_challenges (
    device_token TEXT PRIMARY KEY,
    nonce        TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub device_token: String,
    pub platform:     String,
    pub environment:  String,
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

        // Migrate pre-`last_seen` databases — the column is absent on relays
        // created before push-challenge registration shipped. Backfill
        // existing rows from `created_at` so they aren't pruned immediately.
        let has_last_seen = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('subscriptions') WHERE name = 'last_seen'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_last_seen {
            conn.execute_batch(
                "ALTER TABLE subscriptions ADD COLUMN last_seen INTEGER NOT NULL DEFAULT 0;
                 UPDATE subscriptions SET last_seen = created_at WHERE last_seen = 0;",
            ).context("migrate subscriptions.last_seen")?;
            info!("[db] migrated subscriptions: added last_seen column");
        }

        // Migrate pre-`environment` databases. Existing rows default to
        // 'production' since the relay's previous single-gateway mode was
        // production for live deployments — that keeps existing TestFlight/App
        // Store devices pushing through the same gateway after upgrade.
        let has_environment = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('subscriptions') WHERE name = 'environment'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap_or(false);
        if !has_environment {
            conn.execute_batch(
                "ALTER TABLE subscriptions ADD COLUMN environment TEXT NOT NULL DEFAULT 'production';",
            ).context("migrate subscriptions.environment")?;
            info!("[db] migrated subscriptions: added environment column");
        }
        info!("[db] schema migrations applied (subscriptions, nonces, register_challenges)");
        // Pragmas for the access pattern: tiny writes, point reads.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn upsert_subscription(
        &self,
        device_token: &str,
        platform:     &str,
        lair_pubkey:  &str,
        environment:  &str,
    ) -> Result<()> {
        let now = unix_now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO subscriptions(device_token, platform, lair_pubkey, created_at, last_seen, environment)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)
             ON CONFLICT(device_token, lair_pubkey)
             DO UPDATE SET platform=excluded.platform,
                           last_seen=excluded.last_seen,
                           environment=excluded.environment",
            params![device_token, platform, lair_pubkey, now, environment],
        ).map_err(|e| {
            error!("[db] upsert_subscription failed for pubkey={lair_pubkey}: {e}");
            e
        })?;
        debug!("[db] subscription upserted; platform={platform} env={environment} pubkey={lair_pubkey}");
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
            "SELECT device_token, platform, environment FROM subscriptions WHERE lair_pubkey = ?1",
        ).map_err(|e| {
            error!("[db] subscriptions_for_pubkey prepare failed: {e}");
            e
        })?;
        let rows = stmt.query_map(params![lair_pubkey], |r| {
            Ok(Subscription {
                device_token: r.get(0)?,
                platform:     r.get(1)?,
                environment:  r.get(2)?,
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

    /// Delete subscriptions whose `last_seen` predates `cutoff`. Live devices
    /// re-register on every chat-mount so their rows stay fresh; abandoned or
    /// abusively-created rows age out, bounding table growth.
    pub fn prune_stale_subscriptions(&self, cutoff: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM subscriptions WHERE last_seen < ?1",
            params![cutoff],
        ).map_err(|e| {
            error!("[db] prune_stale_subscriptions failed: {e}");
            e
        })?;
        if n > 0 {
            info!("[db] pruned {n} stale subscription row(s)");
        }
        Ok(n)
    }

    /// Record a registration challenge for `device_token`, replacing any prior
    /// one, and GC challenges older than `ttl_secs`. Returns `Ok(false)`
    /// *without* recording if a challenge was already issued within
    /// `cooldown_secs` — the caller should then skip sending another push so a
    /// caller cannot spam challenge pushes at a device.
    pub fn upsert_challenge(
        &self,
        device_token: &str,
        nonce: &str,
        cooldown_secs: i64,
        ttl_secs: i64,
    ) -> Result<bool> {
        let now = unix_now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM register_challenges WHERE created_at < ?1",
            params![now - ttl_secs],
        ).map_err(|e| {
            error!("[db] register_challenges GC failed: {e}");
            e
        })?;
        let recent: Option<i64> = conn.query_row(
            "SELECT created_at FROM register_challenges WHERE device_token = ?1",
            params![device_token],
            |r| r.get(0),
        ).optional()?;
        if let Some(ts) = recent {
            if now - ts < cooldown_secs {
                debug!("[db] challenge suppressed: within {cooldown_secs}s cooldown");
                return Ok(false);
            }
        }
        conn.execute(
            "INSERT INTO register_challenges(device_token, nonce, created_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(device_token) DO UPDATE SET nonce=excluded.nonce, created_at=excluded.created_at",
            params![device_token, nonce, now],
        ).map_err(|e| {
            error!("[db] upsert_challenge failed: {e}");
            e
        })?;
        debug!("[db] registration challenge recorded");
        Ok(true)
    }

    /// Atomically verify and consume a registration challenge: deletes the row
    /// iff `(device_token, nonce)` matches a challenge no older than
    /// `ttl_secs`. Returns `true` on a successful single-use consume.
    pub fn consume_challenge(&self, device_token: &str, nonce: &str, ttl_secs: i64) -> Result<bool> {
        let now = unix_now();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM register_challenges
             WHERE device_token = ?1 AND nonce = ?2 AND created_at >= ?3",
            params![device_token, nonce, now - ttl_secs],
        ).map_err(|e| {
            error!("[db] consume_challenge failed: {e}");
            e
        })?;
        Ok(n == 1)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

