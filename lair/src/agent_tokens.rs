//! Persistent capability-token store for agent-spawned-agent flows.
//!
//! When an agent spawns a child, lair mints a random opaque token and stashes
//! it here under the child's name. The token is passed to the child as
//! `OCTO_AGENT_TOKEN` and is the only thing that lets the child call lair's
//! agent-scoped endpoints (`POST /agents/child`, `DELETE /agents/child/:name`)
//! to spawn grandchildren or terminate one of its own descendants.
//!
//! The store lives at `<OCTO_DATA_DIR>/agent-tokens.json` with 0600 perms
//! (owned by lair, which runs as root inside the container). Children run
//! as a different per-agent uid, so they can't read this file directly.
//! Persistence lets the supervisor's lair-restart adoption path reissue
//! the same token to a still-running child instead of breaking its
//! spawn capability on every lair restart.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};

const TOKEN_BYTES: usize = 32;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenEntry {
    pub token:      String,
    pub created_at: u64,
}

/// In-memory + on-disk map from agent name → capability token.
pub struct AgentTokens {
    tokens: HashMap<String, TokenEntry>,
    path:   PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct OnDisk {
    #[serde(default)]
    tokens: HashMap<String, TokenEntry>,
}

impl AgentTokens {
    /// Load `<dir>/agent-tokens.json` if it exists, otherwise return an empty
    /// store bound to that path. Corrupt files are logged and treated as
    /// empty so a single bad write can't lock spawn-capability out forever.
    pub fn load(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create agent-tokens dir {}", parent.display()))?;
        }
        let tokens = match fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                match serde_json::from_str::<OnDisk>(&text) {
                    Ok(f) => f.tokens,
                    Err(e) => {
                        tracing::warn!("[agent_tokens] {} is corrupt ({e}); starting empty", path.display());
                        HashMap::new()
                    }
                }
            }
            _ => HashMap::new(),
        };
        Ok(Self { tokens, path })
    }

    /// Lookup token by agent name. Returns the raw token string.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.tokens.get(name).map(|e| e.token.as_str())
    }

    /// Reverse lookup — find the agent name whose token matches the supplied
    /// bearer value. Used by the agent-token middleware.
    pub fn name_for_token(&self, token: &str) -> Option<&str> {
        self.tokens.iter()
            .find(|(_, e)| e.token == token)
            .map(|(name, _)| name.as_str())
    }

    /// Mint a fresh token for `name`, persist the store, and return the token.
    /// If `name` already has a token, returns the existing one unchanged.
    pub fn ensure(&mut self, name: &str, now: u64) -> Result<String> {
        if let Some(existing) = self.tokens.get(name) {
            return Ok(existing.token.clone());
        }
        let mut buf = [0u8; TOKEN_BYTES];
        OsRng.fill_bytes(&mut buf);
        let token = base64_url(&buf);
        self.tokens.insert(name.to_string(), TokenEntry { token: token.clone(), created_at: now });
        self.save()?;
        Ok(token)
    }

    /// Drop `name`'s token, if any. Idempotent.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.tokens.remove(name).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Atomically write the store to disk with 0600 perms.
    fn save(&self) -> Result<()> {
        let snapshot = OnDisk { tokens: self.tokens.clone() };
        let json = serde_json::to_string_pretty(&snapshot)
            .context("serialise agent-tokens")?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json)
            .with_context(|| format!("write {}", tmp.display()))?;
        // Best-effort chmod 0600; if it fails (e.g. weird filesystem) we
        // still proceed — the in-memory copy is the authoritative source.
        chmod_0600_best_effort(&tmp);
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}

fn chmod_0600_best_effort(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(path, perms);
    }
}

/// URL-safe base64 without padding. Avoids `/` and `+` so the token is safe
/// to drop into an HTTP header value verbatim.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let chunks = bytes.chunks(3);
    for chunk in chunks {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize] as char);
        out.push(ALPHA[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHA[(b2 & 0b0011_1111) as usize] as char);
        }
    }
    out
}
