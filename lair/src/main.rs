use octo_k8s_ops::k8s;

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use tracing::{debug, error, info, warn};

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use octo_core::{
    build_ephemeral_system_prompt, build_tools_with_mcp, chain_executor_with_mcp,
    init_mcp_pool, init_shell_env, load_or_generate_keypair,
    resolve_api_key, resolve_model, run_noise_proxy, send_message, to_base32, ApiMessage, AnthropicTool,
    ChatEvent, ContentBlock, McpPool, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use hex;
use futures_util::{SinkExt, StreamExt};
use octo_k8s_ops::Client;
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tower_http::cors::{Any, CorsLayer};

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_KEY_FILE: &str = "/data/noise_key.bin";

// ── Container registry ────────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OCTO_DATA_DIR") {
        PathBuf::from(d)
    } else {
        PathBuf::from("/data")
    }
}

// ── Container types ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct ContainerInfo {
    id:      String,
    name:    String,
    git_url: String,
    status:  String,
    host:    String,
    port:    u16,
    pubkey:  String,
}

// ── Session persistence ───────────────────────────────────────────────────────

fn session_dir() -> PathBuf { data_dir().join("session") }

fn save_messages(messages: &[ApiMessage]) {
    let dir = session_dir();
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        let path = dir.join("messages.json");
        if let Err(e) = fs::write(&path, json) {
            error!("[lair] failed to save messages to {}: {e}", path.display());
        } else {
            debug!("[lair] saved {} message(s) to {}", messages.len(), path.display());
        }
    }
}

fn load_messages() -> Vec<ApiMessage> {
    let path = session_dir().join("messages.json");
    match fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(msgs) => {
            let v: Vec<ApiMessage> = msgs;
            info!("[lair] loaded {} message(s) from {}", v.len(), path.display());
            v
        }
        None => {
            debug!("[lair] no saved messages at {}", path.display());
            vec![]
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct HistMsg {
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
}

fn messages_to_history(messages: &[ApiMessage], last_cost_usd: Option<f64>) -> Vec<HistMsg> {
    // Build tool_use_id → output text from ToolResult blocks in user messages.
    let mut tool_outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages {
        if m.role == "user" {
            for block in &m.content {
                if let ContentBlock::ToolResult { tool_use_id, content } = block {
                    let text = content.first()
                        .and_then(|v| v["text"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    tool_outputs.insert(tool_use_id.clone(), text);
                }
            }
        }
    }

    let mut result = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "user".to_string(), text, cost_usd: None, output: None }); }
            }
            "interrupted" => {
                result.push(HistMsg { role: "interrupted".to_string(), text: "interrupted".to_string(), cost_usd: None, output: None });
            }
            "assistant" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "assistant".to_string(), text, cost_usd: None, output: None }); }
                for block in &m.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let preview = input.as_object()
                            .and_then(|map| map.values().next())
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string());
                        let text = match preview {
                            Some(p) => format!("{name}({p})"),
                            None    => name.clone(),
                        };
                        let output = tool_outputs.get(id).cloned();
                        result.push(HistMsg { role: "tool".to_string(), text, cost_usd: None, output });
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(cost) = last_cost_usd {
        for msg in result.iter_mut().rev() {
            if msg.role == "assistant" {
                msg.cost_usd = Some(cost);
                break;
            }
        }
    }
    result
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages:             Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd:        Mutex<Option<f64>>,
    system:               String,
    containers:           Arc<Mutex<Vec<ContainerInfo>>>,
    poll_trigger:         Arc<Notify>,
    pubkey_b32:           String,
    /// Hex-encoded 64-byte keypair (32 private + 32 public); injected into children.
    noise_private_key_hex: String,
    public_host:          String,
    lair_url:           String,
    kube_client:          Client,
    mcp_pool:             McpPool,
    /// Cancellation token for the current streaming turn. Replaced at the start of each turn.
    cancel:               Mutex<CancellationToken>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

async fn interrupt_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    state.cancel.lock().unwrap().cancel();
    StatusCode::OK
}

async fn info_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "pubkey": state.pubkey_b32 }))
}

async fn history_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cost = *state.last_cost_usd.lock().unwrap();
    let msgs = messages_to_history(&state.messages.lock().unwrap(), cost);
    Json(serde_json::json!({ "messages": msgs }))
}

#[derive(Deserialize)]
struct PostMessage { text: String }

async fn message_handler(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PostMessage>,
) -> impl IntoResponse {
    let preview: String = body.text.chars().take(120).collect();
    info!("[lair/message_handler] received ({} chars): {preview}", body.text.len());
    let start = Instant::now();

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => {
            error!("[lair/message_handler] no API key configured");
            return (StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": "no API key configured"}))).into_response();
        }
    };
    let model = resolve_model();

    let messages = vec![ApiMessage {
        role:    "user".to_string(),
        content: vec![ContentBlock::Text { text: body.text }],
    }];

    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &lair_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), lair_extra_executor(state.clone()));
    match send_message(messages, build_ephemeral_system_prompt(), &model, &api_key, "/", None, CancellationToken::new(), &extra_tools, executor).await {
        Ok((text, cost_usd, _)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[lair/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err((e, _)) => {
            let elapsed = start.elapsed().as_millis();
            error!("[lair/message_handler] error in {elapsed}ms: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response()
        }
    }
}

async fn stream_handler(
    ws:           WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_stream(socket, state))
}

async fn handle_stream(socket: WebSocket, state: Arc<AppState>) {
    info!("[lair/stream] WebSocket connection opened");
    let (mut ws_tx, mut ws_rx) = socket.split();

    let text = loop {
        match ws_rx.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                match serde_json::from_str::<serde_json::Value>(&t)
                    .ok()
                    .and_then(|v| v["text"].as_str().map(str::to_string))
                {
                    Some(t) => break t,
                    None    => {
                        warn!("[lair/stream] first frame missing 'text' field, closing");
                        return;
                    }
                }
            }
            Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
            _ => {
                debug!("[lair/stream] connection closed before first frame");
                return;
            }
        }
    };
    let preview: String = text.chars().take(120).collect();
    info!("[lair/stream] new loop ({} chars): {preview}", text.len());

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None => {
            ws_tx.send(WsMessage::Text(
                serde_json::json!({"type":"error","message":"no API key configured"}).to_string()
            )).await.ok();
            return;
        }
    };
    let model = resolve_model();

    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: text.clone() }],
        });
        save_messages(&msgs);
    }

    let messages: Vec<ApiMessage> = state.messages.lock().unwrap().iter()
        .filter(|m| m.role != "interrupted")
        .cloned()
        .collect();
    let system    = state.system.clone();
    let msgs_arc  = state.messages.clone();
    let state_arc = Arc::clone(&state);

    let (event_tx, mut event_rx) = mpsc::channel::<ChatEvent>(256);
    let done_tx = event_tx.clone();

    // Fresh cancellation token for this turn; stored on AppState so /interrupt can reach it.
    let cancel = CancellationToken::new();
    *state.cancel.lock().unwrap() = cancel.clone();
    let cancel_for_listener = cancel.clone();

    tokio::spawn(async move {
        while let Some(Ok(WsMessage::Text(t))) = ws_rx.next().await {
            if serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["type"].as_str().map(str::to_string))
                .as_deref() == Some("interrupt")
            {
                cancel_for_listener.cancel();
                break;
            }
        }
    });

    let extra_tools = build_tools_with_mcp(&state.mcp_pool, &lair_extra_tools()).await;
    let executor    = chain_executor_with_mcp(state.mcp_pool.clone(), lair_extra_executor(Arc::clone(&state)));
    tokio::spawn(async move {
        match send_message(messages, &system, &model, &api_key, "/", Some(event_tx), cancel.clone(), &extra_tools, executor).await {
            Ok((_, cost_usd, mut updated)) => {
                if cancel.is_cancelled() {
                    updated.push(ApiMessage {
                        role:    "interrupted".to_string(),
                        content: vec![ContentBlock::Text { text: "interrupted".to_string() }],
                    });
                    *msgs_arc.lock().unwrap() = updated.clone();
                    save_messages(&updated);
                    *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                    done_tx.send(ChatEvent::Interrupted { cost_usd }).await.ok();
                } else {
                    *msgs_arc.lock().unwrap() = updated.clone();
                    save_messages(&updated);
                    *state_arc.last_cost_usd.lock().unwrap() = Some(cost_usd);
                    done_tx.send(ChatEvent::Result {
                        cost_usd, turns: 0, session_id: String::new(), result: None,
                    }).await.ok();
                }
            }
            Err((e, mut partial)) => {
                partial.push(ApiMessage {
                    role:    "error".to_string(),
                    content: vec![ContentBlock::Text { text: e.clone() }],
                });
                *msgs_arc.lock().unwrap() = partial.clone();
                save_messages(&partial);
                done_tx.send(ChatEvent::Error { message: e }).await.ok();
            }
        }
    });

    while let Some(event) = event_rx.recv().await {
        let json_opt: Option<serde_json::Value> = match &event {
            ChatEvent::Text { text } => {
                debug!("[lair/stream] text event ({} chars)", text.len());
                Some(serde_json::json!({"type":"text","text":text}))
            }
            ChatEvent::ToolUse { tool, input } => {
                info!("[lair/stream] tool_use event tool={tool}");
                Some(serde_json::json!({"type":"tool_use","tool":tool,"input":input}))
            }
            ChatEvent::ToolOutput { line } =>
                Some(serde_json::json!({"type":"tool_output","line":line})),
            ChatEvent::ToolResult { tool_use_id, content } => {
                let preview = content.as_str().map(|s| s.chars().take(80).collect::<String>()).unwrap_or_default();
                debug!("[lair/stream] tool_result id={tool_use_id}: {preview}");
                Some(serde_json::json!({"type":"tool_result","tool_use_id":tool_use_id,"output":content}))
            }
            ChatEvent::Result { cost_usd, .. } => {
                info!("[lair/stream] done event cost=${cost_usd:.4}");
                Some(serde_json::json!({"type":"done","cost_usd":cost_usd}))
            }
            ChatEvent::Interrupted { cost_usd } => {
                info!("[lair/stream] interrupted event cost=${cost_usd:.4}");
                Some(serde_json::json!({"type":"interrupted","cost_usd":cost_usd}))
            }
            ChatEvent::InterruptAck =>
                Some(serde_json::json!({"type":"interrupt_ack"})),
            ChatEvent::Error { message } => {
                error!("[lair/stream] error event: {message}");
                Some(serde_json::json!({"type":"error","message":message}))
            }
            _ => None,
        };
        if let Some(json) = json_opt {
            if ws_tx.send(WsMessage::Text(json.to_string())).await.is_err() {
                info!("[lair/stream] WebSocket send failed — client disconnected");
                break;
            }
        }
    }
    info!("[lair/stream] session complete");
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[lair/clear] clearing conversation history");
    let mut msgs = state.messages.lock().unwrap();
    msgs.clear();
    save_messages(&msgs);
    StatusCode::OK
}

async fn containers_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let list = state.containers.lock().unwrap().clone();
    Json(serde_json::json!({ "containers": list }))
}

#[derive(Deserialize)]
struct StartContainerBody { id: String }

async fn start_container_handler(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<StartContainerBody>,
) -> impl IntoResponse {
    let name = {
        let containers = state.containers.lock().unwrap();
        containers.iter().find(|c| c.id == body.id).map(|c| c.name.clone())
    };

    let name = match name {
        Some(n) => n,
        None    => return (StatusCode::NOT_FOUND,
                           Json(serde_json::json!({"error": "container not found"}))).into_response(),
    };

    match k8s::scale_deployment(&state.kube_client, &name, 1).await {
        Ok(_) => {
            info!("[containers] scaled {name} to 1, triggering re-poll");
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.poll_trigger.notify_one();
            (StatusCode::OK, Json(serde_json::json!({}))).into_response()
        }
        Err(e) => {
            error!("[containers] scale {name} failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
    }
}

// ── Container poller ──────────────────────────────────────────────────────────

async fn poll_containers(state: Arc<AppState>) {
    info!("[containers] poller starting, initial delay 5s");
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        debug!("[containers] polling K8s for managed deployments");
        match k8s::list_managed_deployments(&state.kube_client).await {
            Ok(children) => {
                debug!("[containers] K8s returned {} deployment(s)", children.len());
                let new_containers: Vec<ContainerInfo> = children
                    .into_iter()
                    .map(|c| {
                        debug!("[containers]   {} status={} port={}", c.name, c.status, c.noise_port);
                        ContainerInfo {
                            id:          c.name.clone(),
                            name:        c.name.clone(),
                            git_url:     c.git_url.clone(),
                            status:      c.status.clone(),
                            host:        state.public_host.clone(),
                            port:        c.noise_port,
                            pubkey:      state.pubkey_b32.clone(),
                        }
                    })
                    .collect();

                let changed = {
                    let current = state.containers.lock().unwrap();
                    *current != new_containers
                };
                if changed {
                    let n = new_containers.len();
                    let names = new_containers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ");
                    *state.containers.lock().unwrap() = new_containers;
                    info!("[containers] state changed: {n} child(ren): {names}");
                }
            }
            Err(e) => error!("[containers] poll error: {e}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                debug!("[containers] poll interval elapsed");
            }
            _ = state.poll_trigger.notified() => {
                info!("[containers] poll triggered manually");
            }
        }
    }
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt() -> String {
    "\
You are the master control node for a fleet of octo coding assistant containers running on Kubernetes.\n\n\
To create a new child for a Git repository, use the create_pod tool — \
it handles Kubernetes resources (Deployments, Services, PVCs), port assignment (NodePorts 30100–30199), \
and all required environment variables automatically.\n\n\
To send a message to a running child's agent, use message_child(container_name, text). \
Use this to delegate coding tasks or coordinate work across children.\n\n\
To permanently remove a child and all its resources, use terminate_pod(name).\n\n\
Be concise and direct."
        .to_string()
}

// ── Tools ─────────────────────────────────────────────────────────────────────

fn message_child_tool() -> AnthropicTool {
    AnthropicTool {
        name: "message_child".to_string(),
        description: "Send a message to a child container's agent and wait for its response. \
                       Use this to delegate tasks or ask questions to a specific child."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "container_name": {
                    "type": "string",
                    "description": "The name of the child to message."
                },
                "text": {
                    "type": "string",
                    "description": "The message to send to the child agent."
                }
            },
            "required": ["container_name", "text"]
        }),
    }
}

fn create_pod_tool() -> AnthropicTool {
    AnthropicTool {
        name: "create_pod".to_string(),
        description: "Create and start a new octo child for a Git repository on Kubernetes. \
                       Handles port assignment (NodePorts 30100–30199), PVCs, Deployment, and Services."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "git_url": {
                    "type": "string",
                    "description": "The Git repository URL to clone and operate on. Omit to start a container without a repository (e.g. for ML workloads or arbitrary compute)."
                },
                "name": {
                    "type": "string",
                    "description": "Optional name override. Defaults to lair-<repo-name>, or lair-workload if no git_url."
                },
                "noise_port": {
                    "type": "integer",
                    "description": "Optional NodePort (30100–30199). Auto-assigned if omitted."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional shell script run inside the child before the server starts. Never include sensitive data such as API keys or tokens — these are stored as plaintext in the Kubernetes Deployment spec."
                },
                "startup_prompt": {
                    "type": "string",
                    "description": "Optional initial prompt sent to the child's agentic loop once ready. Never include sensitive data such as API keys or tokens — these are stored as plaintext in the Kubernetes Deployment spec."
                }
            },
            "required": []
        }),
    }
}

fn terminate_pod_tool() -> AnthropicTool {
    AnthropicTool {
        name: "terminate_pod".to_string(),
        description: "Permanently terminate a child and delete all its Kubernetes resources \
                       (Deployment, Services, PVCs). Irreversible — all PVC data is lost."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The name of the child to terminate."
                }
            },
            "required": ["name"]
        }),
    }
}

fn restart_all_containers_tool() -> AnthropicTool {
    AnthropicTool {
        name: "restart_all_containers".to_string(),
        description: "Rollout-restart all managed child Deployments and lair itself so that \
                       they pick up the latest image. Use this after pushing a new container image \
                       to apply the update across the cluster."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn lair_extra_tools() -> Vec<AnthropicTool> {
    vec![message_child_tool(), create_pod_tool(), terminate_pod_tool(), restart_all_containers_tool()]
}

fn lair_extra_executor(state: Arc<AppState>) -> Option<Arc<dyn Fn(String, serde_json::Value)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send + Sync>>
{
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build message_child HTTP client");
    Some(Arc::new(move |name: String, input: serde_json::Value| {
        let client = client.clone();
        let state  = state.clone();
        Box::pin(async move {
            match name.as_str() {
                "message_child" => exec_message_child(client, input).await,
                "create_pod" => exec_create_pod(state, input).await,
                "terminate_pod" => exec_terminate_pod(state, input).await,
                "restart_all_containers" => exec_restart_all_containers(state).await,
                other => format!("unknown tool: {other}"),
            }
        })
    }))
}

async fn exec_message_child(client: reqwest::Client, input: serde_json::Value) -> String {
    let container_name = match input.get("container_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return "error: missing 'container_name' field".to_string(),
    };
    let text = match input.get("text").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return "error: missing 'text' field".to_string(),
    };
    let preview: String = text.chars().take(120).collect();
    let url = format!("http://{}:8000/message", container_name);
    info!("[lair/message_child] → POST {url} ({} chars): {preview}", text.len());
    let start = Instant::now();
    match client.post(&url).json(&serde_json::json!({ "text": text })).send().await {
        Ok(resp) => {
            let status  = resp.status();
            let elapsed = start.elapsed().as_millis();
            info!("[lair/message_child] ← HTTP {status} in {elapsed}ms from {container_name}");
            match resp.json::<serde_json::Value>().await {
                Ok(body) => {
                    let result = body.get("text").and_then(|v| v.as_str())
                        .unwrap_or("(no response text)").to_string();
                    let rpreview: String = result.chars().take(120).collect();
                    info!("[lair/message_child] response ({} chars): {rpreview}", result.len());
                    result
                }
                Err(e) => {
                    error!("[lair/message_child] parse error from {container_name}: {e}");
                    format!("error parsing child response: {e}")
                }
            }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            error!("[lair/message_child] request to {container_name} failed in {elapsed}ms: {e}");
            format!("error contacting child '{container_name}': {e}")
        }
    }
}

async fn exec_create_pod(state: Arc<AppState>, input: serde_json::Value) -> String {
    let git_url = input.get("git_url").and_then(|v| v.as_str()).map(str::to_string);

    let child_name = input.get("name").and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            match &git_url {
                Some(u) => {
                    let slug = u.trim_end_matches('/')
                        .split('/')
                        .last()
                        .unwrap_or("repo")
                        .trim_end_matches(".git")
                        .to_lowercase();
                    format!("lair-{slug}")
                }
                None => format!("lair-workload"),
            }
        });

    let api_key             = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let gh_token            = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());
    let pub_host            = state.public_host.clone();
    let lair_url          = state.lair_url.clone();
    let noise_private_key   = state.noise_private_key_hex.clone();
    let startup_script = input.get("startup_script").and_then(|v| v.as_str()).map(str::to_string);
    let startup_prompt = input.get("startup_prompt").and_then(|v| v.as_str()).map(str::to_string);

    // Assign NodePort
    let noise_port = match input.get("noise_port").and_then(|v| v.as_u64()) {
        Some(p) => p as u16,
        None => match k8s::assign_nodeport(&state.kube_client).await {
            Ok(p) => p,
            Err(e) => return format!("error: {e}"),
        },
    };

    info!("[lair/create_pod] creating {child_name} port={noise_port} git={}", git_url.as_deref().unwrap_or("(none)"));

    let params = k8s::CreateChildParams {
        name:              &child_name,
        git_url:           git_url.as_deref(),
        noise_port,
        api_key:           &api_key,
        gh_token:          gh_token.as_deref(),
        pub_host:          &pub_host,
        lair_url:        &lair_url,
        startup_script:    startup_script.as_deref(),
        startup_prompt:    startup_prompt.as_deref(),
        noise_private_key: &noise_private_key,
    };

    match k8s::create_child_resources(&state.kube_client, &params).await {
        Ok(_) => {
            info!("[lair/create_pod] created {child_name}");
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.poll_trigger.notify_one();
            format!("Created child '{child_name}' on NodePort {noise_port}.")
        }
        Err(e) => {
            error!("[lair/create_pod] failed: {e:#}");
            format!("error: {e:#}")
        }
    }
}

async fn exec_terminate_pod(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return "error: missing 'name' field".to_string(),
    };

    info!("[lair/terminate_pod] terminating '{name}'");
    match k8s::delete_child_resources(&state.kube_client, &name).await {
        Ok(_) => {
            info!("[lair/terminate_pod] '{name}' deleted, triggering re-poll");
            state.poll_trigger.notify_one();
            format!("Terminated '{name}' and deleted all resources.")
        }
        Err(e) => {
            error!("[lair/terminate_pod] failed to delete '{name}': {e}");
            format!("error: {e}")
        }
    }
}

async fn exec_restart_all_containers(state: Arc<AppState>) -> String {
    info!("[lair/restart_all] triggering rollout restart for all deployments");
    match k8s::restart_deployments(&state.kube_client, &[]).await {
        Ok(restarted) if restarted.is_empty() => {
            info!("[lair/restart_all] no deployments found");
            "No deployments found to restart.".to_string()
        }
        Ok(restarted) => {
            info!("[lair/restart_all] restarted: {}", restarted.join(", "));
            state.poll_trigger.notify_one();
            format!("Rollout restart triggered for: {}.", restarted.join(", "))
        }
        Err(e) => {
            error!("[lair/restart_all] error: {e}");
            format!("error: {e}")
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    init_shell_env();

    let args: Vec<String> = std::env::args().collect();
    let is_dev   = std::env::var("OCTO_DEV").as_deref() == Ok("1");
    let key_file = std::env::var("NOISE_KEY_FILE").unwrap_or_else(|_| NOISE_KEY_FILE.to_string());

    let injected_keypair: Option<(Vec<u8>, Vec<u8>)> = std::env::var("NOISE_PRIVATE_KEY").ok()
        .and_then(|s| {
            let bytes = hex::decode(s.trim()).ok()?;
            if bytes.len() == 64 {
                Some((bytes[..32].to_vec(), bytes[32..].to_vec()))
            } else {
                None
            }
        });

    if args.get(1).map(|s| s.as_str()) == Some("--print-pubkey") {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
        } else if let Some((_, public)) = &injected_keypair {
            to_base32(public)
        } else {
            let (_, public) = load_or_generate_keypair(&key_file);
            to_base32(&public)
        };
        println!("{pubkey}");
        return;
    }

    let (static_private, static_public) = if is_dev {
        warn!("[lair] DEV MODE: using fixed dev keypair");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else if let Some(kp) = injected_keypair {
        kp
    } else {
        load_or_generate_keypair(&key_file)
    };

    let pubkey_b32 = to_base32(&static_public);
    // Hex-encode the 64-byte keypair so it can be injected into children as an env var.
    let noise_private_key_hex = {
        let mut combined = static_private.clone();
        combined.extend_from_slice(&static_public);
        hex::encode(&combined)
    };
    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    let public_host = std::env::var("PUBLIC_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::net::UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string())
        });
    let lair_name = std::env::var("LAIR_NAME").unwrap_or_else(|_| "lair".to_string());
    let lair_url  = format!("http://{}:{}", lair_name, http_port);

    info!("[lair] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port} public_host={public_host}");

    let kube_client = match k8s::build_client().await {
        Ok(c) => { info!("[lair] K8s client initialized"); c }
        Err(e) => {
            error!("[lair] failed to initialize K8s client: {e}");
            std::process::exit(1);
        }
    };

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();
    let messages = load_messages();
    info!("[lair] loaded {} message(s) from history", messages.len());

    let mcp_json_path = dir.join("mcp.json");
    if !mcp_json_path.exists() {
        if let Ok(json) = std::env::var("MCP_CONFIG_JSON") {
            if let Err(e) = fs::write(&mcp_json_path, &json) {
                warn!("[lair] failed to seed mcp.json: {e}");
            } else {
                info!("[lair] seeded mcp.json from MCP_CONFIG_JSON secret");
            }
        }
    }

    let mcp_pool     = init_mcp_pool().await;
    let poll_trigger = Arc::new(Notify::new());

    let state = Arc::new(AppState {
        messages:              Arc::new(Mutex::new(messages)),
        last_cost_usd:         Mutex::new(None),
        system:                build_system_prompt(),
        containers:            Arc::new(Mutex::new(Vec::new())),
        poll_trigger:          poll_trigger.clone(),
        pubkey_b32,
        noise_private_key_hex,
        public_host,
        lair_url,
        kube_client,
        mcp_pool,
        cancel:                Mutex::new(CancellationToken::new()),
    });

    tokio::spawn(poll_containers(state.clone()));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let app = Router::new()
        .route("/health",           get(health_handler))
        .route("/info",             get(info_handler))
        .route("/history",          get(history_handler))
        .route("/message",          post(message_handler))
        .route("/stream",           get(stream_handler))
        .route("/interrupt",        post(interrupt_handler))
        .route("/clear",            post(clear_handler))
        .route("/containers",       get(containers_handler))
        .route("/containers/start", post(start_container_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    info!("[lair] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    axum::serve(listener, app).await.unwrap();
}
