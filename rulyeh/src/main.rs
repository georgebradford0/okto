use std::{
    collections::HashMap,
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
    init_shell_env, load_or_generate_keypair, read_config, resolve_api_key, run_noise_proxy,
    send_message, to_base32, ApiMessage, AnthropicTool, ChatEvent, ContentBlock,
    DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
use futures_util::{SinkExt, StreamExt};
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

fn registry_path() -> PathBuf { data_dir().join("pubkey_registry.json") }

fn load_pubkey_registry() -> HashMap<String, String> {
    fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_pubkey_registry(registry: &HashMap<String, String>) {
    if let Ok(json) = serde_json::to_string(registry) {
        fs::write(registry_path(), json).ok();
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
}

fn messages_to_history(messages: &[ApiMessage], last_cost_usd: Option<f64>) -> Vec<HistMsg> {
    let mut result = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "user".to_string(), text, cost_usd: None }); }
            }
            "interrupted" => {
                result.push(HistMsg { role: "interrupted".to_string(), text: "interrupted".to_string(), cost_usd: None });
            }
            "assistant" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "assistant".to_string(), text, cost_usd: None }); }
                for block in &m.content {
                    if let ContentBlock::ToolUse { name, input, .. } = block {
                        let preview = input.as_object()
                            .and_then(|map| map.values().next())
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                let s = s.trim();
                                let limit = 60;
                                if s.len() <= limit { s.to_string() }
                                else {
                                    let b = s.char_indices().take_while(|(i, _)| *i <= limit).last().map(|(i, _)| i).unwrap_or(limit);
                                    format!("{}…", &s[..b])
                                }
                            });
                        let text = match preview {
                            Some(p) => format!("{name}({p})"),
                            None    => name.clone(),
                        };
                        result.push(HistMsg { role: "tool".to_string(), text, cost_usd: None });
                    }
                }
            }
            _ => {}
        }
    }
    // Attach cost to the last assistant message.
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
    messages:     Arc<Mutex<Vec<ApiMessage>>>,
    last_cost_usd: Mutex<Option<f64>>,
    system:       String,
    containers:   Arc<Mutex<Vec<ContainerInfo>>>,
    poll_trigger: Arc<Notify>,
    pubkey_b32:   String,
    public_host:  String,
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
    info!("[rulyeh/message_handler] model={model}");

    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: body.text }],
        });
        save_messages(&msgs);
    }

    let messages: Vec<ApiMessage> = state.messages.lock().unwrap().iter()
        .filter(|m| m.role != "interrupted")
        .cloned()
        .collect();

    info!("[rulyeh/message_handler] DEBUG conversation ({} messages): {}", messages.len(), serde_json::to_string(&messages).unwrap_or_default());
    match send_message(messages, &state.system, &model, &api_key, "/", None, Arc::new(AtomicBool::new(false)), &rulyeh_extra_tools(), rulyeh_extra_executor()).await {
        Ok((text, cost_usd, updated)) => {
            let elapsed = start.elapsed().as_millis();
            info!("[rulyeh/message_handler] done in {elapsed}ms cost=${cost_usd:.4} response=({} chars)", text.len());
            let mut msgs = state.messages.lock().unwrap();
            *msgs = updated;
            save_messages(&msgs);
            drop(msgs);
            *state.last_cost_usd.lock().unwrap() = Some(cost_usd);
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis();
            error!("[rulyeh/message_handler] error in {elapsed}ms: {e}");
            let mut msgs = state.messages.lock().unwrap();
            msgs.pop();
            save_messages(&msgs);
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

    info!("[rulyeh/handle_stream] DEBUG conversation ({} messages): {}", messages.len(), serde_json::to_string(&messages).unwrap_or_default());
    tokio::spawn(async move {
        match send_message(messages, &system, &model, &api_key, "/", Some(event_tx), aborted.clone(), &rulyeh_extra_tools(), rulyeh_extra_executor()).await {
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

    let result = tokio::process::Command::new("docker")
        .args(["start", &name])
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => {
            info!("[containers] started {name}, triggering re-poll");
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.poll_trigger.notify_one();
            (StatusCode::OK, Json(serde_json::json!({}))).into_response()
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if msg.is_empty() { "docker start failed".to_string() } else { msg };
            error!("[containers] docker start failed: {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": msg}))).into_response()
        }
        Err(e) => {
            error!("[containers] docker start error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
    }
}

// ── Container poller ──────────────────────────────────────────────────────────

async fn poll_containers(state: Arc<AppState>) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        match fetch_managed_containers(&state.public_host).await {
            Ok(mut new_containers) => {
                let mut registry = load_pubkey_registry();
                let mut dirty    = false;

                for c in &mut new_containers {
                    if let Some(pk) = registry.get(&c.id) {
                        c.pubkey = pk.clone();
                    } else if c.status == "running" {
                        info!("[containers] fetching pubkey for {}", c.name);
                        if let Some(pk) = fetch_pubkey_via_exec(&c.name).await {
                            c.pubkey = pk.clone();
                            registry.insert(c.id.clone(), pk);
                            dirty = true;
                        } else {
                            error!("[containers] pubkey fetch failed for {}", c.name);
                        }
                    }
                }

                if dirty { save_pubkey_registry(&registry); }

                let changed = {
                    let current = state.containers.lock().unwrap();
                    *current != new_containers
                };
                if changed {
                    let n = new_containers.len();
                    *state.containers.lock().unwrap() = new_containers;
                    info!("[containers] state changed: {n} container(s)");
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

async fn fetch_managed_containers(public_host: &str) -> anyhow::Result<Vec<ContainerInfo>> {
    let ids_out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("docker")
            .args(["ps", "-a", "--filter", "label=claudulhu.managed=1", "-q"])
            .output(),
    ).await.map_err(|_| anyhow::anyhow!("docker ps timed out"))?
    .map_err(|e| anyhow::anyhow!("docker ps failed: {e}"))?;

    let ids: Vec<&str> = std::str::from_utf8(&ids_out.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    if ids.is_empty() { return Ok(vec![]); }

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("inspect");
    for id in &ids { cmd.arg(id); }
    let inspect_out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await.map_err(|_| anyhow::anyhow!("docker inspect timed out"))?
        .map_err(|e| anyhow::anyhow!("docker inspect failed: {e}"))?;

    let inspect: Vec<serde_json::Value> = serde_json::from_slice(&inspect_out.stdout)?;
    let mut results = Vec::new();

    for c in inspect {
        let id     = c["Id"].as_str().unwrap_or("").chars().take(12).collect::<String>();
        let name   = c["Name"].as_str().unwrap_or("").trim_start_matches('/').to_string();
        let status = c["State"]["Status"].as_str().unwrap_or("unknown").to_string();

        let noise_port: u16 = c["Config"]["Env"]
            .as_array()
            .and_then(|env| {
                env.iter().find_map(|e| {
                    e.as_str()?.strip_prefix("NOISE_PORT=").and_then(|v| v.parse().ok())
                })
            })
            .unwrap_or(9100);

        let git_url = c["Config"]["Labels"]["claudulhu.git_url"]
            .as_str().unwrap_or("").to_string();

        results.push(ContainerInfo {
            id, name, git_url, status,
            host:   public_host.to_string(),
            port:   noise_port,
            pubkey: String::new(),
        });
    }

    Ok(results)
}

async fn fetch_pubkey_via_exec(container_name: &str) -> Option<String> {
    let fut = tokio::process::Command::new("docker")
        .args(["exec", container_name, "claudulhu-server", "--print-pubkey"])
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), fut).await.ok()?.ok()?;
    if !out.status.success() { return None; }
    let pk = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if pk.is_empty() { None } else { Some(pk) }
}

// ── System prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt(rulyeh_url: &str, public_host: &str) -> String {
    format!("\
You are the master control node for a fleet of claudulhu coding assistant containers.\n\n\
Standard child image: ghcr.io/georgebradford0/rulyeh:latest\n\
  with --entrypoint /usr/local/bin/docker-entrypoint-server.sh\n\n\
Child containers require:\n\
  --name rulyeh-<repo-name>\n\
  --network claudulhu-net  --label claudulhu.managed=1  --label claudulhu.git_url=<url>\n\
  NOISE_PORT set to a free port in 9100-9199; publish it with -p <port>:<port> so mobile clients can reach it\n\
  Named volumes for /data and /workspace\n\
  Env vars: ANTHROPIC_API_KEY, GIT_URL, GH_TOKEN (required), PUBLIC_HOST={public_host}, RULYEH_URL={rulyeh_url}\n\n\
Always pass RULYEH_URL={rulyeh_url} to every child container so it can message you back.\n\n\
GH_TOKEN is set in this environment and the gh CLI is available — use it for all GitHub operations.\n\n\
You have a message_child(container_name, text) tool to send a message to a specific child container's agent and receive its response. Use it to delegate coding tasks, query a child's state, or coordinate work across containers.\n\n\
Be concise and direct.")
}

// ── Child messaging tools ──────────────────────────────────────────────────────

fn message_child_tool() -> AnthropicTool {
    AnthropicTool {
        name: "message_child".to_string(),
        description: "Send a message to a child container's agent and wait for its response. \
                       Use this to delegate tasks or ask questions to a specific child container."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "container_name": {
                    "type": "string",
                    "description": "The name of the child container to message."
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

fn rulyeh_extra_tools() -> Vec<AnthropicTool> {
    vec![message_child_tool()]
}

fn rulyeh_extra_executor() -> Option<Arc<dyn Fn(String, serde_json::Value)
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
        Box::pin(async move {
            if name != "message_child" {
                return format!("unknown tool: {name}");
            }
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
            info!("[rulyeh/message_child] → POST {url} container={container_name} ({} chars): {preview}", text.len());
            let start = Instant::now();
            match client
                .post(&url)
                .json(&serde_json::json!({ "text": text }))
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let elapsed = start.elapsed().as_millis();
                    info!("[rulyeh/message_child] ← HTTP {status} in {elapsed}ms from {container_name}");
                    match resp.json::<serde_json::Value>().await {
                        Ok(body) => {
                            let result = body
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(no response text)")
                                .to_string();
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
        })
    }))
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

    let pubkey_b32   = to_base32(&static_public);
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
    let rulyeh_name = std::env::var("RULYEH_NAME")
        .unwrap_or_else(|_| "rulyeh".to_string());
    let rulyeh_url = format!("http://{}:{}", rulyeh_name, http_port);

    info!("[rulyeh] noise_pubkey={pubkey_b32} noise_port={noise_port} http_port={http_port} public_host={public_host}");

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();
    let messages = load_messages();
    info!("[rulyeh] loaded {} message(s) from history", messages.len());

    let poll_trigger = Arc::new(Notify::new());

    let state = Arc::new(AppState {
        messages:      Arc::new(Mutex::new(messages)),
        last_cost_usd: Mutex::new(None),
        system:        build_system_prompt(&rulyeh_url, &public_host),
        containers:    Arc::new(Mutex::new(Vec::new())),
        poll_trigger:  poll_trigger.clone(),
        pubkey_b32,
        public_host,
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
