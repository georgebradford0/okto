/// MCP (Model Context Protocol) client — stdio and HTTP/SSE transports.
///
/// Spawns external MCP server processes (stdio) or connects to remote MCP
/// servers (HTTP/SSE), performs the JSON-RPC handshake, discovers their tools,
/// and dispatches tool calls to them at runtime.
///
/// # Configuration
///
/// Read from `$OKTO_DATA_DIR/mcp.json` (i.e. `/data/mcp.json` in Docker).
/// Format: a JSON array of server descriptors.
///
/// ## Stdio transport (local process)
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
/// ## HTTP transport (remote server)
/// ```json
/// [
///   {
///     "name": "github",
///     "url": "https://api.githubcopilot.com/mcp/",
///     "headers": { "Authorization": "Bearer ${GH_TOKEN}" }
///   }
/// ]
/// ```
///
/// For stdio: `"env"` values of the form `"${VAR}"` are substituted from the
/// host environment at connect time.
/// For HTTP: `"headers"` values of the form `"${VAR}"` are similarly expanded.
/// When `"url"` is present the `"command"` field is ignored.
use std::collections::HashMap;

use futures_util::StreamExt;
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
    /// Executable to run (stdio transport). Ignored when `url` is set.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args:    Vec<String>,
    /// Extra environment variables for the spawned process (stdio only).
    /// Values of the form `"${VAR}"` are expanded from the host environment.
    #[serde(default)]
    pub env:     HashMap<String, String>,
    /// HTTP endpoint for remote MCP servers (e.g. `"https://api.githubcopilot.com/mcp/"`).
    /// When set, HTTP/SSE transport is used and `command`/`args`/`env` are ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url:     Option<String>,
    /// Additional HTTP headers sent with every request (HTTP transport only).
    /// Values of the form `"${VAR}"` are expanded from the host environment.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

/// Load MCP server configs from `$OKTO_DATA_DIR/mcp.json`.
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

// ── Transport ─────────────────────────────────────────────────────────────────

enum McpTransport {
    Stdio {
        stdin:  ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
    Http {
        client:  reqwest::Client,
        url:     String,
        headers: HashMap<String, String>,
    },
}

// ── Client ────────────────────────────────────────────────────────────────────

/// A live connection to one MCP server (stdio or HTTP).
pub struct McpClient {
    /// The logical name from `McpServerConfig`.
    pub name:  String,
    /// Tools advertised by this server (populated after `initialize` + `tools/list`).
    pub tools: Vec<AnthropicTool>,
    transport: McpTransport,
    next_id:   u64,
}

impl McpClient {
    /// Connect to an MCP server using whichever transport the config specifies.
    pub async fn connect(cfg: &McpServerConfig) -> Option<Self> {
        if cfg.url.is_some() {
            Self::connect_http(cfg).await
        } else {
            Self::connect_stdio(cfg).await
        }
    }

    // ── Stdio ─────────────────────────────────────────────────────────────────

    async fn connect_stdio(cfg: &McpServerConfig) -> Option<Self> {
        // Expand "${VAR}" references in env values. Fail loudly on a missing
        // var rather than passing the literal "${VAR}" through to the child —
        // that silently corrupts e.g. AWS credentials and surfaces later as
        // opaque downstream auth errors.
        let env = match resolve_env_or_headers(&cfg.name, &cfg.env, "env") {
            Some(e) => e,
            None    => return None,
        };

        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        info!("[mcp] spawning '{}': {} {:?}", cfg.name, cfg.command, cfg.args);
        let mut child = match cmd.spawn() {
            Ok(c)  => c,
            Err(e) => { error!("[mcp] failed to spawn '{}': {e}", cfg.name); return None; }
        };

        let Some(stdin) = child.stdin.take() else {
            error!("[mcp] '{}' spawned process has no stdin pipe", cfg.name);
            return None;
        };
        let Some(stdout_pipe) = child.stdout.take() else {
            error!("[mcp] '{}' spawned process has no stdout pipe", cfg.name);
            return None;
        };
        let stdout = BufReader::new(stdout_pipe);
        std::mem::forget(child); // detach — not reaped on drop

        let client = McpClient {
            name:      cfg.name.clone(),
            tools:     vec![],
            transport: McpTransport::Stdio { stdin, stdout },
            next_id:   1,
        };

        Self::do_connect(client, &cfg.name).await
    }

    // ── HTTP ──────────────────────────────────────────────────────────────────

    async fn connect_http(cfg: &McpServerConfig) -> Option<Self> {
        let url = cfg.url.as_deref().unwrap_or("").trim_end_matches('/').to_string();

        // Expand "${VAR}" references in header values. Same rationale as
        // connect_stdio: silently shipping a literal "${VAR}" produces an
        // unhelpful 401 from the remote rather than a clear local error.
        let headers = match resolve_env_or_headers(&cfg.name, &cfg.headers, "header") {
            Some(h) => h,
            None    => return None,
        };

        info!("[mcp] '{}' connecting via HTTP to {url}", cfg.name);

        let http_client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
        {
            Ok(c)  => c,
            Err(e) => { error!("[mcp] '{}' failed to build HTTP client: {e}", cfg.name); return None; }
        };

        let client = McpClient {
            name:      cfg.name.clone(),
            tools:     vec![],
            transport: McpTransport::Http { client: http_client, url, headers },
            next_id:   1,
        };

        Self::do_connect(client, &cfg.name).await
    }

    // ── Handshake (owns the client so it can be returned on success) ──────────

    async fn do_connect(mut client: McpClient, name: &str) -> Option<McpClient> {
        let init_result = client.request("initialize", serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "clientInfo": { "name": "okto", "version": env!("CARGO_PKG_VERSION") }
        })).await;

        if let Err(e) = init_result {
            error!("[mcp] '{name}' initialize failed: {e}");
            return None;
        }
        debug!("[mcp] '{name}' initialize OK");

        let _ = client.notify("notifications/initialized", serde_json::json!({})).await;

        match client.request("tools/list", serde_json::json!({})).await {
            Err(e) => {
                error!("[mcp] '{name}' tools/list failed: {e}");
                None
            }
            Ok(result) => {
                client.tools = parse_tools(name, &result);
                info!(
                    "[mcp] '{name}' connected — {} tool(s): {}",
                    client.tools.len(),
                    client.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
                );
                Some(client)
            }
        }
    }

    // ── Tool call ─────────────────────────────────────────────────────────────

    /// Call a tool by name and return its text output.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> String {
        debug!("[mcp] '{}' calling tool '{name}'", self.name);
        let start = std::time::Instant::now();
        let out = match self.request("tools/call", serde_json::json!({ "name": name, "arguments": arguments })).await {
            Err(e) => format!("[mcp error from '{}']: {e}", self.name),
            Ok(result) => {
                let is_error = result["isError"].as_bool().unwrap_or(false);
                let text = result["content"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| match c["type"].as_str() {
                                Some("text") => c["text"].as_str().map(str::to_owned),
                                _ => Some(c.to_string()),
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if is_error { format!("[mcp tool error]: {text}") } else { text }
            }
        };
        let elapsed = start.elapsed().as_millis();
        debug!("[mcp] '{}' tool '{name}' done in {elapsed}ms ({} chars)", self.name, out.len());
        out
    }

    // ── JSON-RPC ──────────────────────────────────────────────────────────────

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id":      id,
            "method":  method,
            "params":  params,
        });

        match &mut self.transport {
            McpTransport::Stdio { stdin, stdout } => {
                // Write request.
                let mut line = msg.to_string();
                line.push('\n');
                stdin.write_all(line.as_bytes()).await
                    .map_err(|e| format!("write error: {e}"))?;
                stdin.flush().await
                    .map_err(|e| format!("flush error: {e}"))?;

                // Read until we get a response matching our id.
                loop {
                    let mut line = String::new();
                    match stdout.read_line(&mut line).await {
                        Err(e)  => return Err(format!("read error: {e}")),
                        Ok(0)   => return Err("MCP server closed stdout".into()),
                        Ok(_) if line.trim().is_empty() => continue,
                        Ok(_)   => {}
                    }
                    let v: Value = serde_json::from_str(line.trim())
                        .map_err(|e| format!("JSON parse error ({e}): {line}"))?;
                    if v.get("id").is_none() { continue; } // skip notifications
                    if v["id"].as_u64() == Some(id) {
                        if let Some(err) = v.get("error") { return Err(err.to_string()); }
                        return Ok(v["result"].clone());
                    }
                }
            }

            McpTransport::Http { client, url, headers } => {
                let client  = client.clone();
                let url     = url.clone();
                let headers = headers.clone();
                http_rpc(&client, &url, &headers, &msg).await
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
        });
        match &mut self.transport {
            McpTransport::Stdio { stdin, .. } => {
                let mut line = msg.to_string();
                line.push('\n');
                stdin.write_all(line.as_bytes()).await
                    .map_err(|e| format!("write error: {e}"))?;
                stdin.flush().await
                    .map_err(|e| format!("flush error: {e}"))
            }
            McpTransport::Http { client, url, headers } => {
                let client  = client.clone();
                let url     = url.clone();
                let headers = headers.clone();
                // Fire-and-forget; server returns 202 or an SSE we don't need.
                let _ = http_post(&client, &url, &headers, &msg).await;
                Ok(())
            }
        }
    }
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

/// POST a JSON-RPC message and return the `result` value from the response.
/// Handles both `application/json` and `text/event-stream` response bodies.
async fn http_rpc(
    client:  &reqwest::Client,
    url:     &str,
    headers: &HashMap<String, String>,
    body:    &Value,
) -> Result<Value, String> {
    let response = http_post(client, url, headers, body).await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let ct = response.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if ct.contains("text/event-stream") {
        parse_sse_response(response).await
    } else {
        let json: Value = response.json().await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        // Direct JSON-RPC response envelope.
        if let Some(err) = json.get("error") { return Err(err.to_string()); }
        Ok(json.get("result").cloned().unwrap_or(json))
    }
}

async fn http_post(
    client:  &reqwest::Client,
    url:     &str,
    headers: &HashMap<String, String>,
    body:    &Value,
) -> Result<reqwest::Response, String> {
    let mut req = client
        .post(url)
        .header("Content-Type",  "application/json")
        .header("Accept",        "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.json(body).send().await.map_err(|e| format!("HTTP request failed: {e}"))
}

/// Read an SSE stream and return the `result` from the first JSON-RPC response event.
async fn parse_sse_response(response: reqwest::Response) -> Result<Value, String> {
    let mut stream = response.bytes_stream();
    let mut buf    = String::new();
    let mut data   = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("SSE read error: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // Process all complete lines in the buffer.
        loop {
            match buf.find('\n') {
                None => break,
                Some(pos) => {
                    let line = buf[..pos].trim_end_matches('\r').to_string();
                    buf = buf[pos + 1..].to_string();

                    if line.is_empty() {
                        // Blank line = end of SSE event; process accumulated data.
                        if !data.is_empty() {
                            if let Ok(json) = serde_json::from_str::<Value>(&data) {
                                if json.get("id").is_some() {
                                    if let Some(err) = json.get("error") {
                                        return Err(err.to_string());
                                    }
                                    if json.get("result").is_some() {
                                        return Ok(json["result"].clone());
                                    }
                                }
                            }
                            data.clear();
                        }
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim_start());
                    }
                    // Ignore event:, id:, retry: lines.
                }
            }
        }
    }

    Err("SSE stream closed without a JSON-RPC response".to_string())
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// Expand `"${VAR}"` references in MCP env / header values. Well-known
/// credential names are resolved against the operator's `config.json`
/// (`ANTHROPIC_API_KEY` → `config.anthropic_api_key`, etc.) so an mcp.json
/// like `{"env": {"API_KEY": "${ANTHROPIC_API_KEY}"}}` keeps working after
/// 0.6.2 moved secrets out of lair's process env. Anything not in that table
/// falls back to `std::env::var(...)`. Returns `Err(var_name)` if neither
/// source has the variable; the host-side `expand_host_env` resolver in the
/// CLI works the same way. Callers must surface this as a connect-time error
/// rather than letting the literal `${VAR}` reach the child process.
fn expand_var(v: &str) -> std::result::Result<String, String> {
    if !(v.starts_with("${") && v.ends_with('}')) {
        return Ok(v.to_string());
    }
    let var = &v[2..v.len() - 1];
    let cfg = crate::read_config();
    let from_cfg = match var {
        "ANTHROPIC_API_KEY" => cfg.anthropic_api_key,
        "OPENAI_API_KEY"    => cfg.openai_api_key,
        "OPENAI_API_URL"    => cfg.api_url,
        "MODEL"             => cfg.model,
        // `${GH_TOKEN}` falls through to the std::env::var() lookup below —
        // lair's `GH_TOKEN` lives in its process env (operator-supplied via
        // `okto init --env GH_TOKEN=…`), not in config.json.
        _                   => None,
    };
    from_cfg
        .or_else(|| std::env::var(var).ok())
        .ok_or_else(|| var.to_string())
}

/// Expand every `"${VAR}"` value in `map`. On any unresolved var, log a
/// connect-time failure using the same `[mcp] '<name>' initialize failed: …`
/// shape that `do_connect` emits, so the CLI's marker scanner classifies it
/// as `McpMarker::InitFailed` and surfaces it as `HANDSHAKE FAILED — …` in
/// `okto mcp add` / `okto mcp import` output. `kind` is `"env"` or `"header"`
/// — purely for the error message.
fn resolve_env_or_headers(
    name: &str,
    map:  &HashMap<String, String>,
    kind: &str,
) -> Option<HashMap<String, String>> {
    let mut out = HashMap::with_capacity(map.len());
    let mut missing: Vec<String> = Vec::new();
    for (k, v) in map {
        match expand_var(v) {
            Ok(resolved) => { out.insert(k.clone(), resolved); }
            Err(var)     => missing.push(var),
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        error!(
            "[mcp] '{name}' initialize failed: {kind} var(s) not set in lair container: {} \
             — set them via `okto env set KEY=VAL` (then `okto reload`), or inline literal \
             values in mcp.json",
            missing.join(", "),
        );
        return None;
    }
    Some(out)
}

// ── Tool parsing ──────────────────────────────────────────────────────────────

fn parse_tools(server_name: &str, result: &Value) -> Vec<AnthropicTool> {
    let Some(arr) = result["tools"].as_array() else {
        warn!("[mcp] '{server_name}' tools/list result had no 'tools' array");
        return vec![];
    };
    arr.iter().filter_map(|t| {
        let name        = t["name"].as_str()?;
        let description = t["description"].as_str().unwrap_or("").to_string();
        let input_schema = if t["inputSchema"].is_object() {
            t["inputSchema"].clone()
        } else {
            serde_json::json!({ "type": "object", "properties": {} })
        };
        let prefixed = format!("{server_name}__{name}");
        let display_label = Some(crate::derive_display_label(&prefixed));
        Some(AnthropicTool { name: prefixed, description, input_schema, display_label })
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
pub type McpPool = std::sync::Arc<RwLock<Vec<std::sync::Arc<Mutex<McpClient>>>>>;

/// Initialise all configured MCP servers.  Servers that fail to connect are
/// skipped.  Spawns a background task that watches `mcp.json` for changes and
/// hot-reloads the pool automatically.
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
            connected, configs.len(), names.join(", ")
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
pub async fn pool_call_tool(pool: &McpPool, name: &str, input: Value) -> Option<String> {
    let guard = pool.read().await;
    for client in guard.iter() {
        let mut c = client.lock().await;
        if c.tools.iter().any(|t| t.name == name) {
            let original = name.split_once("__").map(|(_, n)| n).unwrap_or(name);
            debug!("[mcp] dispatching '{name}' → server '{}' as '{original}'", c.name);
            return Some(c.call_tool(original, input).await);
        }
    }
    debug!("[mcp] no pool client owns tool '{name}'");
    None
}

/// Diff `mcp.json` against the live pool: connect newly-added servers, drop removed ones.
pub async fn reload_mcp_pool(pool: &McpPool) -> String {
    let new_configs = load_mcp_configs();
    let new_name_set: std::collections::HashSet<String> =
        new_configs.iter().map(|c| c.name.clone()).collect();

    let existing_names: Vec<String>;
    let removed_names: Vec<String>;
    {
        let mut guard = pool.write().await;
        let mut names = Vec::new();
        for client in guard.iter() {
            names.push(client.lock().await.name.clone());
        }
        existing_names = names.clone();
        let to_remove: Vec<usize> = names.iter().enumerate()
            .filter(|(_, n)| !new_name_set.contains(*n))
            .map(|(i, _)| i)
            .collect();
        removed_names = to_remove.iter().map(|i| names[*i].clone()).collect();
        for i in to_remove.into_iter().rev() { guard.remove(i); }
    }

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
    if !to_add.is_empty() { pool.write().await.extend(to_add); }

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
/// tool advertised by the live MCP pool.
pub async fn build_tools_with_mcp(pool: &McpPool, extra: &[AnthropicTool]) -> Vec<AnthropicTool> {
    let mut tools = extra.to_vec();
    tools.extend(pool_tool_definitions(pool).await);
    tools
}

type Executor = std::sync::Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>;

/// Wrap an existing executor so that any tool name not handled by `inner` is
/// first tried against the MCP pool before falling back to "unknown tool".
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
        let mut last_modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
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
