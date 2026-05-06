/// MCP (Model Context Protocol) client — stdio transport.
///
/// Spawns external MCP server processes, performs the JSON-RPC handshake,
/// discovers their tools, and dispatches tool calls to them at runtime.
///
/// # Configuration
///
/// Read from `$OCTO_DATA_DIR/mcp.json` (i.e. `/data/mcp.json` in Docker).
/// Format: a JSON array of server descriptors:
///
/// ```json
/// [
///   {
///     "name": "github",
///     "command": "npx",
///     "args": ["-y", "@modelcontextprotocol/server-github"],
///     "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_..." }
///   }
/// ]
/// ```
///
/// The optional `"env"` map is merged on top of the inherited process environment.
/// Values of the form `"${VAR}"` are substituted from the host environment.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, RwLock},
};
use tracing::{debug, error, info, warn};

use crate::AnthropicTool;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct McpServerConfig {
    /// Logical name — used in log messages; does not need to match the server's
    /// own `serverInfo.name`.
    pub name:    String,
    /// Executable to run (e.g. `"npx"`, `"python"`, `"/data/tools/my_server"`).
    pub command: String,
    #[serde(default)]
    pub args:    Vec<String>,
    /// Extra environment variables.  Values of the form `"${VAR}"` are expanded
    /// from the host environment at load time.
    #[serde(default)]
    pub env:     HashMap<String, String>,
}

/// Load MCP server configs from `$OCTO_DATA_DIR/mcp.json`.
/// Returns an empty vec if the file is absent or unparseable.
pub fn load_mcp_configs() -> Vec<McpServerConfig> {
    let path = crate::data_dir().join("mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        debug!("[mcp] no config file at {}", path.display());
        return vec![];
    };
    match serde_json::from_str::<Vec<McpServerConfig>>(&text) {
        Ok(cfgs) => {
            info!("[mcp] loaded {} server config(s) from {}", cfgs.len(), path.display());
            cfgs
        }
        Err(e) => {
            error!("[mcp] failed to parse {}: {e}", path.display());
            vec![]
        }
    }
}

// ── Client ────────────────────────────────────────────────────────────────────

/// A live connection to one MCP server process.
pub struct McpClient {
    /// The logical name from `McpServerConfig`.
    pub name:  String,
    /// Tools advertised by this server (populated after `initialize` + `tools/list`).
    pub tools: Vec<AnthropicTool>,
    stdin:     ChildStdin,
    stdout:    BufReader<ChildStdout>,
    next_id:   u64,
}

impl McpClient {
    /// Spawn the server process and complete the MCP initialization handshake.
    /// Returns `None` (with a logged error) if the process cannot be started or
    /// the handshake fails.
    pub async fn connect(cfg: &McpServerConfig) -> Option<Self> {
        // Expand "${VAR}" references in env values.
        let env: HashMap<String, String> = cfg.env.iter()
            .map(|(k, v)| {
                let expanded = if v.starts_with("${") && v.ends_with('}') {
                    let var = &v[2..v.len() - 1];
                    std::env::var(var).unwrap_or_else(|_| v.clone())
                } else {
                    v.clone()
                };
                (k.clone(), expanded)
            })
            .collect();

        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()); // surface MCP server logs

        info!("[mcp] spawning '{}': {} {:?}", cfg.name, cfg.command, cfg.args);
        let mut child = match cmd.spawn() {
            Ok(c)  => c,
            Err(e) => {
                error!("[mcp] failed to spawn '{}': {e}", cfg.name);
                return None;
            }
        };

        let stdin  = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);

        // Detach the child so it is not reaped when we drop the handle.
        std::mem::forget(child);

        let mut client = McpClient { name: cfg.name.clone(), tools: vec![], stdin, stdout, next_id: 1 };

        // initialize
        let init_result = client.request("initialize", serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "clientInfo": { "name": "octo", "version": env!("CARGO_PKG_VERSION") }
        })).await;

        if let Err(e) = init_result {
            error!("[mcp] '{}' initialize failed: {e}", cfg.name);
            return None;
        }
        debug!("[mcp] '{}' initialize OK", cfg.name);

        // notifications/initialized  (fire-and-forget, no response expected)
        let _ = client.notify("notifications/initialized", serde_json::json!({})).await;

        // tools/list
        match client.request("tools/list", serde_json::json!({})).await {
            Err(e) => {
                error!("[mcp] '{}' tools/list failed: {e}", cfg.name);
                return None;
            }
            Ok(result) => {
                client.tools = parse_tools(&cfg.name, &result);
                info!(
                    "[mcp] '{}' connected — {} tool(s): {}",
                    cfg.name,
                    client.tools.len(),
                    client.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
        }

        Some(client)
    }

    /// Call a tool by name and return its text output.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> String {
        debug!("[mcp] '{}' calling tool '{name}'", self.name);
        let start = std::time::Instant::now();
        let out = match self.request("tools/call", serde_json::json!({ "name": name, "arguments": arguments })).await {
            Err(e) => format!("[mcp error from '{}']: {e}", self.name),
            Ok(result) => {
                // MCP result: { content: [{ type: "text", text: "..." }, ...], isError?: bool }
                let is_error = result["isError"].as_bool().unwrap_or(false);
                let text = result["content"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| {
                                match c["type"].as_str() {
                                    Some("text") => c["text"].as_str().map(str::to_owned),
                                    // For non-text content blocks, serialise to JSON so
                                    // the model still receives something meaningful.
                                    _ => Some(c.to_string()),
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();

                if is_error {
                    format!("[mcp tool error]: {text}")
                } else {
                    text
                }
            }
        };
        let elapsed = start.elapsed().as_millis();
        debug!("[mcp] '{}' tool '{name}' done in {elapsed}ms ({} chars)", self.name, out.len());
        out
    }

    // ── JSON-RPC helpers ──────────────────────────────────────────────────────

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })).await?;

        // Read lines until we get a response matching our id.
        // Notifications (no "id") are silently skipped.
        loop {
            let mut line = String::new();
            match self.stdout.read_line(&mut line).await {
                Err(e)               => return Err(format!("read error: {e}")),
                Ok(0)                => return Err("MCP server closed stdout".into()),
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {}
            }

            let v: Value = serde_json::from_str(line.trim())
                .map_err(|e| format!("JSON parse error ({e}): {line}"))?;

            // Skip notifications (they have "method" but no matching "id").
            if v.get("id").is_none() { continue; }

            if v["id"].as_u64() == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(err.to_string());
                }
                return Ok(v["result"].clone());
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })).await
    }

    async fn send_raw(&mut self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await
            .map_err(|e| format!("write error: {e}"))?;
        self.stdin.flush().await
            .map_err(|e| format!("flush error: {e}"))
    }
}

// ── Tool parsing ──────────────────────────────────────────────────────────────

fn parse_tools(server_name: &str, result: &Value) -> Vec<AnthropicTool> {
    let Some(arr) = result["tools"].as_array() else {
        warn!("[mcp] '{server_name}' tools/list result had no 'tools' array");
        return vec![];
    };
    arr.iter().filter_map(|t| {
        let name = t["name"].as_str()?;
        let description = t["description"].as_str().unwrap_or("").to_string();
        // MCP uses "inputSchema"; fall back to an empty object schema.
        let input_schema = if t["inputSchema"].is_object() {
            t["inputSchema"].clone()
        } else {
            serde_json::json!({ "type": "object", "properties": {} })
        };
        // Prefix with server name to avoid collisions with built-in tools.
        let prefixed = format!("{server_name}__{name}");
        Some(AnthropicTool { name: prefixed, description, input_schema })
    }).collect::<Vec<_>>()
    .tap_warn_empty(server_name)
}

trait TapWarnEmpty {
    fn tap_warn_empty(self, server: &str) -> Self;
}
impl TapWarnEmpty for Vec<AnthropicTool> {
    fn tap_warn_empty(self, server: &str) -> Self {
        if self.is_empty() {
            warn!("[mcp] server '{server}' advertised no tools");
        }
        self
    }
}

// ── Pool ──────────────────────────────────────────────────────────────────────

/// A thread-safe pool of connected MCP clients.
/// The outer `Arc<RwLock<…>>` allows the pool itself to be mutated at runtime
/// (hot-reload) while multiple async tasks hold cheap clones of the `Arc`.
pub type McpPool = std::sync::Arc<RwLock<Vec<std::sync::Arc<Mutex<McpClient>>>>>;

/// Initialise all configured MCP servers.  Servers that fail to start are
/// skipped (errors are printed to stderr).  Spawns a background task that
/// watches `mcp.json` for changes and hot-reloads the pool automatically.
pub async fn init_mcp_pool() -> McpPool {
    let configs = load_mcp_configs();
    let mut inner = Vec::with_capacity(configs.len());
    for cfg in &configs {
        if let Some(client) = McpClient::connect(cfg).await {
            inner.push(std::sync::Arc::new(Mutex::new(client)));
        }
    }
    if !configs.is_empty() {
        let connected = inner.len();
        let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
        info!(
            "[mcp] initialised {}/{} server(s) from config: {}",
            connected,
            configs.len(),
            names.join(", ")
        );
    } else {
        debug!("[mcp] no servers configured");
    }
    let pool = std::sync::Arc::new(RwLock::new(inner));
    start_mcp_watcher(pool.clone());
    pool
}

/// Collect all tools from every client in the pool.
pub async fn pool_tool_definitions(pool: &McpPool) -> Vec<AnthropicTool> {
    let mut tools = Vec::new();
    for client in pool.read().await.iter() {
        tools.extend(client.lock().await.tools.clone());
    }
    tools
}

/// Dispatch a tool call to the first client in the pool that owns it.
/// Returns `None` if no client owns the tool (caller should fall through to
/// built-ins or return "unknown tool").
pub async fn pool_call_tool(pool: &McpPool, name: &str, input: Value) -> Option<String> {
    let guard = pool.read().await;
    for client in guard.iter() {
        let mut c = client.lock().await;
        if c.tools.iter().any(|t| t.name == name) {
            // Strip the "{server_name}__" prefix before forwarding to the MCP server,
            // which expects the original unprefixed tool name.
            let original = name.split_once("__").map(|(_, n)| n).unwrap_or(name);
            debug!("[mcp] dispatching '{name}' → server '{}' as '{original}'", c.name);
            return Some(c.call_tool(original, input).await);
        }
    }
    debug!("[mcp] no pool client owns tool '{name}'");
    None
}

/// Diff `mcp.json` against the live pool: connect newly-added servers, drop
/// removed ones.  Returns a human-readable summary of changes.
pub async fn reload_mcp_pool(pool: &McpPool) -> String {
    let new_configs = load_mcp_configs();
    let new_name_set: std::collections::HashSet<String> =
        new_configs.iter().map(|c| c.name.clone()).collect();

    // --- Phase 1: collect current names and remove stale entries (write lock) ---
    let existing_names: Vec<String>;
    let removed_names: Vec<String>;
    {
        let mut guard = pool.write().await;

        // Collect names (requires async lock on each client).
        let mut names = Vec::new();
        for client in guard.iter() {
            names.push(client.lock().await.name.clone());
        }
        existing_names = names.clone();

        // Identify indices to remove (reverse order to preserve indexing).
        let to_remove: Vec<usize> = names.iter().enumerate()
            .filter(|(_, n)| !new_name_set.contains(*n))
            .map(|(i, _)| i)
            .collect();
        removed_names = to_remove.iter().map(|i| names[*i].clone()).collect();
        for i in to_remove.into_iter().rev() {
            guard.remove(i);
        }
    } // write lock released

    // --- Phase 2: connect new servers (outside lock — may involve I/O) ---
    let existing_name_set: std::collections::HashSet<String> =
        existing_names.into_iter().collect();
    let mut added_names = Vec::new();
    let mut to_add = Vec::new();
    for cfg in &new_configs {
        if !existing_name_set.contains(&cfg.name) {
            if let Some(client) = McpClient::connect(cfg).await {
                added_names.push(cfg.name.clone());
                to_add.push(std::sync::Arc::new(Mutex::new(client)));
            }
        }
    }

    if !to_add.is_empty() {
        pool.write().await.extend(to_add);
    }

    // --- Build summary ---
    if added_names.is_empty() && removed_names.is_empty() {
        debug!("[mcp] reload: no changes");
        return "no changes".to_string();
    }
    let mut parts = Vec::new();
    if !added_names.is_empty() {
        info!("[mcp] reload: added servers: {}", added_names.join(", "));
        parts.push(format!("added: {}", added_names.join(", ")));
    }
    if !removed_names.is_empty() {
        info!("[mcp] reload: removed servers: {}", removed_names.join(", "));
        parts.push(format!("removed: {}", removed_names.join(", ")));
    }
    parts.join("; ")
}

// ── Convenience helpers for callers ──────────────────────────────────────────

/// Build the full extra-tools list: caller-supplied extras first, then every
/// tool advertised by the live MCP pool.  Call once per `send_message` turn so
/// that tools added via hot-reload are visible immediately.
pub async fn build_tools_with_mcp(
    pool:  &McpPool,
    extra: &[AnthropicTool],
) -> Vec<AnthropicTool> {
    let mut tools = extra.to_vec();
    tools.extend(pool_tool_definitions(pool).await);
    tools
}

type Executor = std::sync::Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>;

/// Wrap an existing executor so that any tool name not handled by `inner` is
/// first tried against the MCP pool before falling back to "unknown tool".
/// This is the only change needed to add MCP dispatch to an existing server.
pub fn chain_executor_with_mcp(pool: McpPool, inner: Option<Executor>) -> Option<Executor> {
    Some(std::sync::Arc::new(move |name: String, input: serde_json::Value| {
        let pool  = pool.clone();
        let inner = inner.clone();
        Box::pin(async move {
            if let Some(result) = pool_call_tool(&pool, &name, input.clone()).await {
                return result;
            }
            match inner {
                Some(f) => f(name, input).await,
                None    => format!("unknown tool: {name}"),
            }
        })
    }))
}

/// Spawn a background task that polls `mcp.json`'s modification time every
/// 2 seconds and calls `reload_mcp_pool` when it changes.
fn start_mcp_watcher(pool: McpPool) {
    let path = crate::data_dir().join("mcp.json");
    tokio::spawn(async move {
        let mut last_modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok();
            if modified != last_modified {
                last_modified = modified;
                if modified.is_some() {
                    info!("[mcp] mcp.json changed, hot-reloading pool");
                    let summary = reload_mcp_pool(&pool).await;
                    info!("[mcp] hot-reload complete: {summary}");
                }
            }
        }
    });
}
