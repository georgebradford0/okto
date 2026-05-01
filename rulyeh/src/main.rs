mod k8s;
mod aws;

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tracing::{error, info, warn};

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
use claudulhu_core::{
    build_ephemeral_system_prompt, init_shell_env, load_or_generate_keypair, read_config,
    resolve_api_key, run_noise_proxy, send_message, to_base32, ApiMessage, AnthropicTool,
    ChatEvent, ContentBlock, DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use hex;
use futures_util::{SinkExt, StreamExt};
use kube::Client;
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tower_http::cors::{Any, CorsLayer};

// ── Noise Protocol ────────────────────────────────────────────────────────────

const NOISE_KEY_FILE: &str = "/data/noise_key.bin";

// ── Container registry ────────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDULHU_DATA_DIR") {
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
    remote:  bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
}

// ── Session persistence ───────────────────────────────────────────────────────

fn session_dir() -> PathBuf { data_dir().join("session") }

fn save_messages(messages: &[ApiMessage]) {
    let dir = session_dir();
    fs::create_dir_all(&dir).ok();
    if let Ok(json) = serde_json::to_string(messages) {
        fs::write(dir.join("messages.json"), json).ok();
    }
}

fn load_messages() -> Vec<ApiMessage> {
    fs::read_to_string(session_dir().join("messages.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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
    rulyeh_url:           String,
    kube_client:          Client,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

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
    info!("[rulyeh/message_handler] received ({} chars): {preview}", body.text.len());
    let start = Instant::now();

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => {
            error!("[rulyeh/message_handler] no API key configured");
            return (StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": "no API key configured"}))).into_response();
        }
    };
    let model = read_config().model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    let messages = vec![ApiMessage {
        role:    "user".to_string(),
        content: vec![ContentBlock::Text { text: body.text }],
    }];

    match send_message(messages, build_ephemeral_system_prompt(), &model, &api_key, "/", None, Arc::new(AtomicBool::new(false)), &rulyeh_extra_tools(), rulyeh_extra_executor(state.clone())).await {
        Ok((text, cost_usd, _)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[rulyeh/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            error!("[rulyeh/message_handler] error in {elapsed}ms: {e}");
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
    let (mut ws_tx, mut ws_rx) = socket.split();

    let text = loop {
        match ws_rx.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                match serde_json::from_str::<serde_json::Value>(&t)
                    .ok()
                    .and_then(|v| v["text"].as_str().map(str::to_string))
                {
                    Some(t) => break t,
                    None    => return,
                }
            }
            Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
            _ => return,
        }
    };

    let api_key = match resolve_api_key() {
        Some(k) => k,
        None => {
            ws_tx.send(WsMessage::Text(
                serde_json::json!({"type":"error","message":"no API key configured"}).to_string()
            )).await.ok();
            return;
        }
    };
    let model = read_config().model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

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

    let aborted              = Arc::new(AtomicBool::new(false));
    let aborted_for_listener = aborted.clone();

    tokio::spawn(async move {
        while let Some(Ok(WsMessage::Text(t))) = ws_rx.next().await {
            if serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["type"].as_str().map(str::to_string))
                .as_deref() == Some("interrupt")
            {
                aborted_for_listener.store(true, Ordering::Relaxed);
                break;
            }
        }
    });

    let executor = rulyeh_extra_executor(Arc::clone(&state));
    tokio::spawn(async move {
        match send_message(messages, &system, &model, &api_key, "/", Some(event_tx), aborted.clone(), &rulyeh_extra_tools(), executor).await {
            Ok((_, cost_usd, mut updated)) => {
                if aborted.load(Ordering::Relaxed) {
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
            Err(e) => {
                msgs_arc.lock().unwrap().pop();
                save_messages(&msgs_arc.lock().unwrap());
                done_tx.send(ChatEvent::Error { message: e }).await.ok();
            }
        }
    });

    while let Some(event) = event_rx.recv().await {
        let json_opt: Option<serde_json::Value> = match event {
            ChatEvent::Text { text } =>
                Some(serde_json::json!({"type":"text","text":text})),
            ChatEvent::ToolUse { tool, input } =>
                Some(serde_json::json!({"type":"tool_use","tool":tool,"input":input})),
            ChatEvent::ToolOutput { line } =>
                Some(serde_json::json!({"type":"tool_output","line":line})),
            ChatEvent::ToolResult { tool_use_id, content } =>
                Some(serde_json::json!({"type":"tool_result","tool_use_id":tool_use_id,"output":content})),
            ChatEvent::Result { cost_usd, .. } =>
                Some(serde_json::json!({"type":"done","cost_usd":cost_usd})),
            ChatEvent::Interrupted { cost_usd } =>
                Some(serde_json::json!({"type":"interrupted","cost_usd":cost_usd})),
            ChatEvent::Error { message } =>
                Some(serde_json::json!({"type":"error","message":message})),
            _ => None,
        };
        if let Some(json) = json_opt {
            if ws_tx.send(WsMessage::Text(json.to_string())).await.is_err() { break; }
        }
    }
}

async fn clear_handler(State(state): State<Arc<AppState>>) -> StatusCode {
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
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        match k8s::list_managed_deployments(&state.kube_client).await {
            Ok(children) => {
                let new_containers: Vec<ContainerInfo> = children
                    .into_iter()
                    .map(|c| ContainerInfo {
                        id:          c.name.clone(),
                        name:        c.name.clone(),
                        git_url:     c.git_url.clone(),
                        status:      c.status.clone(),
                        host:        state.public_host.clone(),
                        port:        c.noise_port,
                        pubkey:      state.pubkey_b32.clone(),
                        remote:      c.remote,
                        instance_id: c.instance_id.clone(),
                    })
                    .collect();

                let changed = {
                    let current = state.containers.lock().unwrap();
                    *current != new_containers
                };
                if changed {
                    let n = new_containers.len();
                    *state.containers.lock().unwrap() = new_containers;
                    info!("[containers] state changed: {n} child(ren)");
                }
            }
            Err(e) => error!("[containers] poll error: {e}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = state.poll_trigger.notified() => {}
        }
    }
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt() -> String {
    "\
You are the master control node for a fleet of claudulhu coding assistant containers running on Kubernetes.\n\n\
To create a new child for a Git repository, use the create_container tool — \
it handles Kubernetes resources (Deployments, Services, PVCs), port assignment (NodePorts 30100–30199), \
and all required environment variables automatically. \
Pass remote=true and instance_type to provision an EC2 worker node first.\n\n\
To send a message to a running child's agent, use message_child(container_name, text). \
Use this to delegate coding tasks or coordinate work across children.\n\n\
To permanently remove a child and all its resources, use terminate_container(name).\n\n\
GH_TOKEN is set in this environment and the gh CLI is available — use it for all GitHub operations, including finding and searching repos.\n\n\
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

fn create_container_tool() -> AnthropicTool {
    AnthropicTool {
        name: "create_container".to_string(),
        description: "Create and start a new claudulhu child for a Git repository on Kubernetes. \
                       Handles port assignment (NodePorts 30100–30199), PVCs, Deployment, and Services. \
                       Set remote=true to provision an EC2 worker node first; requires instance_type in that case."
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
                    "description": "Optional name override. Defaults to rulyeh-<repo-name>, or rulyeh-workload-<port> if no git_url."
                },
                "noise_port": {
                    "type": "integer",
                    "description": "Optional NodePort (30100–30199). Auto-assigned if omitted."
                },
                "startup_script": {
                    "type": "string",
                    "description": "Optional shell script run inside the child before the server starts."
                },
                "startup_prompt": {
                    "type": "string",
                    "description": "Optional initial prompt sent to the child's agentic loop once ready."
                },
                "remote": {
                    "type": "boolean",
                    "description": "If true, provision an EC2 worker node before scheduling the child."
                },
                "instance_type": {
                    "type": "string",
                    "description": "EC2 instance type (e.g. t3.medium). Required when remote=true."
                }
            },
            "required": []
        }),
    }
}

fn terminate_container_tool() -> AnthropicTool {
    AnthropicTool {
        name: "terminate_container".to_string(),
        description: "Permanently terminate a child and delete all its Kubernetes resources \
                       (Deployment, Services, PVCs). For remote children, also terminates the EC2 \
                       instance and removes the K8s node. Irreversible — all PVC data is lost."
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
        description: "Rollout-restart all managed child Deployments and rulyeh itself so that \
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

fn rulyeh_extra_tools() -> Vec<AnthropicTool> {
    vec![message_child_tool(), create_container_tool(), terminate_container_tool(), restart_all_containers_tool()]
}

fn rulyeh_extra_executor(state: Arc<AppState>) -> Option<Arc<dyn Fn(String, serde_json::Value)
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
                "create_container" => exec_create_container(state, input).await,
                "terminate_container" => exec_terminate_container(state, input).await,
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
    info!("[rulyeh/message_child] → POST {url} ({} chars): {preview}", text.len());
    let start = Instant::now();
    match client.post(&url).json(&serde_json::json!({ "text": text })).send().await {
        Ok(resp) => {
            let status  = resp.status();
            let elapsed = start.elapsed().as_millis();
            info!("[rulyeh/message_child] ← HTTP {status} in {elapsed}ms from {container_name}");
            match resp.json::<serde_json::Value>().await {
                Ok(body) => {
                    let result = body.get("text").and_then(|v| v.as_str())
                        .unwrap_or("(no response text)").to_string();
                    let rpreview: String = result.chars().take(120).collect();
                    info!("[rulyeh/message_child] response ({} chars): {rpreview}", result.len());
                    result
                }
                Err(e) => {
                    error!("[rulyeh/message_child] parse error from {container_name}: {e}");
                    format!("error parsing child response: {e}")
                }
            }
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            error!("[rulyeh/message_child] request to {container_name} failed in {elapsed}ms: {e}");
            format!("error contacting child '{container_name}': {e}")
        }
    }
}

async fn exec_create_container(state: Arc<AppState>, input: serde_json::Value) -> String {
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
                    format!("rulyeh-{slug}")
                }
                None => format!("rulyeh-workload"),
            }
        });

    let remote       = input.get("remote").and_then(|v| v.as_bool()).unwrap_or(false);
    let instance_type = input.get("instance_type").and_then(|v| v.as_str()).map(str::to_string);

    if remote && instance_type.is_none() {
        return "error: instance_type is required when remote=true".to_string();
    }

    let api_key             = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let gh_token            = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());
    let pub_host            = state.public_host.clone();
    let rulyeh_url          = state.rulyeh_url.clone();
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

    let mut node_selector: Option<std::collections::HashMap<String, String>> = None;
    let mut ec2_instance_id: Option<String> = None;

    if remote {
        let sg  = std::env::var("AWS_SECURITY_GROUP_ID").unwrap_or_default();
        let sub = std::env::var("AWS_SUBNET_ID").ok().filter(|s| !s.is_empty());
        let cp  = std::env::var("K3S_CONTROL_PLANE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("https://{}:6443", state.public_host));
        if sg.is_empty() {
            return "error: AWS_SECURITY_GROUP_ID must be set for remote provisioning".to_string();
        }

        // Read join token
        let join_token = match k8s::read_join_token(&state.kube_client).await {
            Ok(t) => t,
            Err(e) => return format!("error reading k3s join token: {e}"),
        };

        // Select latest Ubuntu 24.04 AMI
        let ami = match aws::describe_latest_ubuntu_ami().await {
            Ok(a) => a,
            Err(e) => return format!("error selecting AMI: {e}"),
        };
        info!("[rulyeh/create_container] remote ami={ami} instance_type={}", instance_type.as_deref().unwrap_or(""));

        let user_data = format!(
            "#!/bin/bash\nset -e\ncurl -sfL https://get.k3s.io | K3S_URL={cp} K3S_TOKEN={join_token} K3S_NODE_LABEL=\"claudulhu.child-name={child_name}\" sh -\n"
        );

        let spec = aws::InstanceSpec {
            ami: &ami,
            instance_type: instance_type.as_deref().unwrap_or("t3.medium"),
            security_group_id: &sg,
            subnet_id: sub.as_deref(),
            child_name: &child_name,
            user_data: &user_data,
        };

        let instance_id = match aws::run_instance(&spec).await {
            Ok(id) => id,
            Err(e) => return format!("error launching EC2 instance: {e}"),
        };
        info!("[rulyeh/create_container] launched EC2 {instance_id}");

        // Poll for running state
        if let Err(e) = aws::wait_for_instance_running(&instance_id).await {
            return format!("error waiting for EC2 instance: {e}");
        }

        // Wait for node to join and be Ready
        let node_name = match k8s::wait_for_node_ready(&state.kube_client, &child_name, 180).await {
            Ok(n) => n,
            Err(e) => return format!("error waiting for K8s node: {e}"),
        };

        // Label the node
        let mut labels = std::collections::HashMap::new();
        labels.insert("claudulhu.ec2-instance-id".to_string(), instance_id.clone());
        labels.insert("claudulhu.child-name".to_string(), child_name.clone());
        if let Err(e) = k8s::label_node(&state.kube_client, &node_name, &labels).await {
            error!("[rulyeh/create_container] label node failed: {e}");
        }

        let mut ns = std::collections::HashMap::new();
        ns.insert("claudulhu.child-name".to_string(), child_name.clone());
        node_selector = Some(ns);
        ec2_instance_id = Some(instance_id);
    }

    info!("[rulyeh/create_container] creating {child_name} port={noise_port} git={} remote={remote}", git_url.as_deref().unwrap_or("(none)"));

    let params = k8s::CreateChildParams {
        name:              &child_name,
        git_url:           git_url.as_deref(),
        noise_port,
        api_key:           &api_key,
        gh_token:          gh_token.as_deref(),
        pub_host:          &pub_host,
        rulyeh_url:        &rulyeh_url,
        startup_script:    startup_script.as_deref(),
        startup_prompt:    startup_prompt.as_deref(),
        node_selector,
        remote,
        instance_id:       ec2_instance_id.as_deref(),
        noise_private_key: &noise_private_key,
    };

    match k8s::create_child_resources(&state.kube_client, &params).await {
        Ok(_) => {
            info!("[rulyeh/create_container] created {child_name}");
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.poll_trigger.notify_one();
            format!("Created child '{child_name}' on NodePort {noise_port}.")
        }
        Err(e) => {
            error!("[rulyeh/create_container] failed: {e:#}");
            format!("error: {e:#}")
        }
    }
}

async fn exec_terminate_container(state: Arc<AppState>, input: serde_json::Value) -> String {
    let name = match input.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return "error: missing 'name' field".to_string(),
    };

    let (remote, instance_id) = {
        let containers = state.containers.lock().unwrap();
        containers.iter()
            .find(|c| c.name == name)
            .map(|c| (c.remote, c.instance_id.clone()))
            .unwrap_or((false, None))
    };

    let node_name = if remote {
        k8s::find_node_for_child(&state.kube_client, &name).await
    } else {
        None
    };

    match k8s::delete_child_resources(
        &state.kube_client,
        &name,
        node_name.as_deref(),
        instance_id.as_deref(),
    ).await {
        Ok(_) => {
            state.poll_trigger.notify_one();
            format!("Terminated '{name}' and deleted all resources.")
        }
        Err(e) => format!("error: {e}"),
    }
}

async fn exec_restart_all_containers(state: Arc<AppState>) -> String {
    match k8s::restart_deployments(&state.kube_client, &[]).await {
        Ok(restarted) if restarted.is_empty() => {
            "No deployments found to restart.".to_string()
        }
        Ok(restarted) => {
            state.poll_trigger.notify_one();
            format!("Rollout restart triggered for: {}.", restarted.join(", "))
        }
        Err(e) => format!("error: {e}"),
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
    let is_dev   = std::env::var("CLAUDULHU_DEV").as_deref() == Ok("1");
    let key_file = std::env::var("NOISE_KEY_FILE").unwrap_or_else(|_| NOISE_KEY_FILE.to_string());

    if args.get(1).map(|s| s.as_str()) == Some("--print-pubkey") {
        let pubkey = if is_dev {
            DEV_PUBKEY_BASE32.to_string()
        } else {
            let (_, public) = load_or_generate_keypair(&key_file);
            to_base32(&public)
        };
        println!("{pubkey}");
        return;
    }

    let (static_private, static_public) = if is_dev {
        warn!("[rulyeh] DEV MODE: using fixed dev keypair");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
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
    let rulyeh_name = std::env::var("RULYEH_NAME").unwrap_or_else(|_| "rulyeh".to_string());
    let rulyeh_url  = format!("http://{}:{}", rulyeh_name, http_port);

    info!("[rulyeh] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port} public_host={public_host}");

    let kube_client = match k8s::build_client().await {
        Ok(c) => { info!("[rulyeh] K8s client initialized"); c }
        Err(e) => {
            error!("[rulyeh] failed to initialize K8s client: {e}");
            std::process::exit(1);
        }
    };

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();
    let messages = load_messages();
    info!("[rulyeh] loaded {} message(s) from history", messages.len());

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
        rulyeh_url,
        kube_client,
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
        .route("/clear",            post(clear_handler))
        .route("/containers",       get(containers_handler))
        .route("/containers/start", post(start_container_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    info!("[rulyeh] HTTP listening on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    axum::serve(listener, app).await.unwrap();
}
