//! Persisted registry of agent (child) processes managed by lair.
//!
//! Lair spawns each local child as a `lair --role agent` OS process
//! and registers it here. Remote agents (provisioned on a separate VM via a
//! cloud-MCP and bootstrapped over SSH) are also tracked in the same file,
//! distinguished by `host.is_some()`.
//!
//! The file lives at `<data_dir>/agents.json`. Lair is the sole writer.

use std::{
    fs,
    path::PathBuf,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Lifecycle state of an agent process. Mirrors the strings emitted to the
/// mobile wire protocol.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    /// Process is alive (local) or last-known reachable (remote).
    Running,
    /// Process exited (clean or crashed). For remote agents, this is only set
    /// explicitly — there's no continuous health probe.
    Stopped,
    /// Spawned (or being bootstrapped over SSH) but not yet observed live.
    Pending,
}

impl AgentStatus {
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Stopped => "stopped",
            AgentStatus::Pending => "pending",
        }
    }
}

/// One agent the lair owns.
///
/// `git_url` deliberately isn't stored — it's a spawn-time arg, not a
/// permanent property. The cloned repo (if any) lives in the agent's
/// workspace dir, and `bootstrap::ensure_workspace` detects it on restart
/// via the `.git` marker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentRecord {
    /// Stable, human-readable identifier. Doubles as the wire `id`.
    pub name:           String,
    /// OS pid of the last spawned `lair --role agent` process. Local
    /// agents only — `None` for remote agents and for local agents whose
    /// process has exited.
    #[serde(default)]
    pub pid:            Option<u32>,
    /// Port lair connects to when proxying mobile traffic.
    ///   - Local agent: the loopback HTTP port the child binds (30100–30199).
    ///   - Remote agent: the public Noise port the VM publishes.
    pub port:           u16,
    /// External host for remote agents (`Some(<public_ip>)`). `None` for
    /// local agents (they're reached on 127.0.0.1).
    #[serde(default)]
    pub host:           Option<String>,
    /// Base32-encoded Noise static pubkey. Only set for remote agents — lair
    /// uses it to verify the Noise handshake when opening a proxy tunnel.
    /// Local agents speak plain HTTP on loopback, so this stays `None`.
    #[serde(default)]
    pub pubkey:         Option<String>,
    /// Last observed status. Reconciled against pid liveness for local
    /// agents; left untouched for remote agents (they age out on explicit
    /// `forget_agent`).
    pub status:         AgentStatus,
    /// Lair version (`CARGO_PKG_VERSION`) at the time the row was created.
    pub binary_version: String,
    /// Unix seconds when the record was created.
    pub created_at:     u64,
    /// Unix seconds the registry last observed the agent live.
    pub last_seen:      u64,
    /// Cloud instance id (e.g. `i-0abc…`) for remote agents.
    #[serde(default)]
    pub instance_id:    Option<String>,
    /// Free-form provider tag for remote agents (`aws`, `hetzner`, …). Lair
    /// doesn't interpret it; it's surfaced to the LLM so subsequent tool
    /// calls (e.g. terminate) know which MCP to invoke.
    #[serde(default)]
    pub provider:       Option<String>,
    /// Opaque provider-specific blob (region, instance_type, image id, …).
    #[serde(default)]
    pub metadata:       serde_json::Value,
    /// Name of the agent that spawned this one, if any. `None` when the
    /// agent was created by the operator (CLI / lair's own LLM); `Some(_)`
    /// when another agent spawned it via the agent-token-gated API. Used
    /// to drive cascade-terminate and to enforce depth / descendant caps.
    #[serde(default)]
    pub parent:         Option<String>,
    /// Name of the source ("main repo") agent this agent is a git worktree
    /// of. `None` for ordinary agents. Orthogonal to `parent`: a worktree is
    /// operator-created (so `parent` is `None`) but git-anchored to a shared
    /// repo cache, not to the source agent. Drives the indented sidebar
    /// nesting on the clients and worktree teardown.
    #[serde(default)]
    pub worktree_of:    Option<String>,
    /// Shared repo-cache key (slug derived from the repo origin URL). Set
    /// alongside `worktree_of`; locates the primary clone under
    /// `OKTO_REPOS_DIR/<slug>` for `git worktree add/remove/prune`.
    #[serde(default)]
    pub repo_slug:      Option<String>,
    /// Branch this worktree checked out. Stored so teardown can delete the
    /// branch (`git branch -D`) even when the workspace dir is already gone.
    #[serde(default)]
    pub worktree_branch: Option<String>,
}

impl AgentRecord {
    /// True when this agent lives on a remote VM (registered via
    /// `register_remote_agent`), false when it's a local process on the
    /// lair host.
    pub fn is_remote(&self) -> bool {
        self.host.is_some()
    }
}

/// On-disk registry. Held under a `Mutex` in `AppState`.
#[derive(Default)]
pub struct Registry {
    agents: Vec<AgentRecord>,
    path:   PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    agents: Vec<AgentRecord>,
}

impl Registry {
    /// Load `<dir>/agents.json` if it exists, otherwise return an empty
    /// registry bound to that path. Corrupt files are logged and treated as
    /// empty so a single bad write can't lock lair out forever.
    pub fn load(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create registry dir {}", parent.display()))?;
        }
        let agents = match fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                match serde_json::from_str::<RegistryFile>(&text) {
                    Ok(f) => f.agents,
                    Err(e) => {
                        warn!("[registry] {} is corrupt ({e}); starting empty", path.display());
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        };
        info!("[registry] loaded {} agent(s) from {}", agents.len(), path.display());
        Ok(Self { agents, path })
    }

    pub fn list(&self) -> &[AgentRecord] { &self.agents }

    pub fn get(&self, name: &str) -> Option<&AgentRecord> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut AgentRecord> {
        self.agents.iter_mut().find(|a| a.name == name)
    }

    /// Insert or replace a row by name, preserving insertion order on updates.
    pub fn set(&mut self, record: AgentRecord) -> Result<()> {
        let name = record.name.clone();
        if let Some(slot) = self.agents.iter_mut().find(|a| a.name == record.name) {
            *slot = record;
            debug!("[registry] updated agent '{name}'");
        } else {
            self.agents.push(record);
            debug!("[registry] inserted agent '{name}'");
        }
        self.save()
    }

    pub fn add(&mut self, record: AgentRecord) -> Result<()> {
        if self.agents.iter().any(|a| a.name == record.name) {
            warn!("[registry] add rejected: agent '{}' already exists", record.name);
            anyhow::bail!("agent '{}' already exists in registry", record.name);
        }
        info!("[registry] added agent '{}' port={}", record.name, record.port);
        self.agents.push(record);
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let before = self.agents.len();
        self.agents.retain(|a| a.name != name);
        let removed = self.agents.len() != before;
        if removed {
            info!("[registry] removed agent '{name}'");
            self.save()?;
        } else {
            debug!("[registry] remove no-op: agent '{name}' not found");
        }
        Ok(removed)
    }

    pub fn update_status(&mut self, name: &str, status: AgentStatus) -> Result<bool> {
        let Some(r) = self.get_mut(name) else { return Ok(false); };
        if r.status == status { return Ok(false); }
        info!("[registry] agent '{name}' status {} -> {}", r.status.as_wire_str(), status.as_wire_str());
        r.status = status;
        self.save()?;
        Ok(true)
    }

    pub fn update_last_seen(&mut self, name: &str, ts: u64) -> Result<bool> {
        let Some(r) = self.get_mut(name) else { return Ok(false); };
        r.last_seen = ts;
        Ok(true)
    }

    pub fn update_pid(&mut self, name: &str, pid: Option<u32>) -> Result<bool> {
        let Some(r) = self.get_mut(name) else { return Ok(false); };
        if r.pid == pid { return Ok(false); }
        debug!("[registry] agent '{name}' pid {:?} -> {:?}", r.pid, pid);
        r.pid = pid;
        self.save()?;
        Ok(true)
    }

    /// First port in `range` not currently used by any registered agent.
    pub fn assign_free_port(&self, range: std::ops::RangeInclusive<u16>) -> Option<u16> {
        let used: std::collections::HashSet<u16> =
            self.agents.iter().map(|a| a.port).collect();
        range.into_iter().find(|p| !used.contains(p))
    }

    /// Depth of `name` in the parent chain: 0 for top-level (no parent), 1
    /// for a direct child of a top-level agent, etc. Returns `None` if
    /// `name` is not in the registry. Unknown / dangling parents short-circuit
    /// to the depth where the chain breaks (treated as top-level above the
    /// break) — they don't loop.
    pub fn depth_of(&self, name: &str) -> Option<usize> {
        let mut current = self.get(name)?.parent.clone();
        let mut depth = 0usize;
        // Hard cap on chain walks to prevent any accidental cycle.
        for _ in 0..256 {
            match current {
                None => return Some(depth),
                Some(p) => {
                    depth += 1;
                    current = self.get(&p).and_then(|r| r.parent.clone());
                    if current.is_none() && self.get(&p).is_none() {
                        // Parent name doesn't resolve — treat as top-level above the break.
                        return Some(depth);
                    }
                }
            }
        }
        Some(depth)
    }

    /// All direct children of `name` (one level below).
    pub fn direct_children(&self, name: &str) -> Vec<String> {
        self.agents.iter()
            .filter(|a| a.parent.as_deref() == Some(name))
            .map(|a| a.name.clone())
            .collect()
    }

    /// Transitive descendants of `name`, ordered leaves-first (so callers
    /// can terminate them in order without orphaning intermediate nodes).
    /// Excludes `name` itself.
    pub fn descendants_leaves_first(&self, name: &str) -> Vec<String> {
        // BFS by level, then reverse so leaves come first.
        let mut levels: Vec<Vec<String>> = Vec::new();
        let mut frontier = self.direct_children(name);
        while !frontier.is_empty() {
            let next: Vec<String> = frontier.iter()
                .flat_map(|n| self.direct_children(n))
                .collect();
            levels.push(frontier);
            frontier = next;
        }
        levels.into_iter().rev().flatten().collect()
    }

    /// Write the registry to disk atomically (temp file + rename).
    pub fn save(&self) -> Result<()> {
        let file = RegistryFile { agents: self.agents.clone() };
        let json = serde_json::to_string_pretty(&file)
            .context("serialise registry")?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &json)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }
}
