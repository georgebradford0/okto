use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::State,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use claudulhu_core::{
    init_shell_env, load_or_generate_keypair, read_config, resolve_api_key, run_noise_proxy,
    send_message, to_base32, ApiMessage, ContentBlock,
    DEV_PUBKEY_BASE32, DEV_STATIC_PRIVATE, DEV_STATIC_PUBLIC,
};
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
}

fn messages_to_history(messages: &[ApiMessage]) -> Vec<HistMsg> {
    let mut result = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "user" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "user".to_string(), text }); }
            }
            "assistant" => {
                let text: String = m.content.iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect();
                if !text.is_empty() { result.push(HistMsg { role: "assistant".to_string(), text }); }
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
                        result.push(HistMsg { role: "tool".to_string(), text });
                    }
                }
            }
            _ => {}
        }
    }
    result
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages:     Arc<Mutex<Vec<ApiMessage>>>,
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
    let msgs = messages_to_history(&state.messages.lock().unwrap());
    Json(serde_json::json!({ "messages": msgs }))
}

#[derive(Deserialize)]
struct PostMessage { text: String }

async fn message_handler(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<PostMessage>,
) -> impl IntoResponse {
    let api_key = match resolve_api_key() {
        Some(k) => k,
        None    => return (StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": "no API key configured"}))).into_response(),
    };
    let model = read_config().model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());

    {
        let mut msgs = state.messages.lock().unwrap();
        msgs.push(ApiMessage {
            role:    "user".to_string(),
            content: vec![ContentBlock::Text { text: body.text }],
        });
        save_messages(&msgs);
    }

    let messages = state.messages.lock().unwrap().clone();

    match send_message(messages, &state.system, &model, &api_key, "/").await {
        Ok((text, cost_usd, updated)) => {
            let mut msgs = state.messages.lock().unwrap();
            *msgs = updated;
            save_messages(&msgs);
            (StatusCode::OK, Json(serde_json::json!({ "text": text, "cost_usd": cost_usd }))).into_response()
        }
        Err(e) => {
            let mut msgs = state.messages.lock().unwrap();
            msgs.pop();
            save_messages(&msgs);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response()
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
            println!("[containers] started {name}, triggering re-poll");
            tokio::time::sleep(Duration::from_secs(3)).await;
            state.poll_trigger.notify_one();
            (StatusCode::OK, Json(serde_json::json!({}))).into_response()
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if msg.is_empty() { "docker start failed".to_string() } else { msg };
            eprintln!("[containers] docker start failed: {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": msg}))).into_response()
        }
        Err(e) => {
            eprintln!("[containers] docker start error: {e}");
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
                        println!("[containers] fetching pubkey for {}", c.name);
                        if let Some(pk) = fetch_pubkey_via_exec(&c.name).await {
                            c.pubkey = pk.clone();
                            registry.insert(c.id.clone(), pk);
                            dirty = true;
                        } else {
                            eprintln!("[containers] pubkey fetch failed for {}", c.name);
                        }
                    }
                }

                if dirty { save_pubkey_registry(&registry); }

                let changed = {
                    let current = state.containers.lock().unwrap();
                    *current != new_containers
                };
                if changed {
                    *state.containers.lock().unwrap() = new_containers;
                    println!("[containers] state changed");
                }
            }
            Err(e) => eprintln!("[containers] poll error: {e}"),
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

const RULYEH_SYSTEM_PROMPT: &str = "\
You are the master control node for a fleet of claudulhu coding assistant containers.\n\n\
Standard child image: ghcr.io/georgebradford0/rulyeh:latest\n\
  with --entrypoint /usr/local/bin/docker-entrypoint-server.sh\n\n\
Child containers require:\n\
  --name <repo-name>  (derive from the repo, no prefix)\n\
  --network claudulhu-net  --label claudulhu.managed=1  --label claudulhu.git_url=<url>\n\
  NOISE_PORT set to a free port in 9100-9199\n\
  Named volumes for /data and /workspace\n\
  Env vars: ANTHROPIC_API_KEY, GIT_URL, GH_TOKEN (required), PUBLIC_HOST\n\n\
GH_TOKEN is set in this environment and the gh CLI is available — use it for all GitHub operations.\n\n\
Be concise and direct.";

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
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
        println!("[claudulhu-rulyeh] !! DEV MODE: using fixed dev keypair");
        (DEV_STATIC_PRIVATE.to_vec(), DEV_STATIC_PUBLIC.to_vec())
    } else {
        load_or_generate_keypair(&key_file)
    };

    let pubkey_b32   = to_base32(&static_public);
    let noise_port: u16 = std::env::var("NOISE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(9000);
    let http_port:  u16 = 8000;
    let public_host = std::env::var("PUBLIC_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

    println!("[claudulhu-rulyeh] Noise public key: {pubkey_b32}");

    tokio::spawn(run_noise_proxy(static_private, noise_port, http_port));

    let dir = data_dir();
    fs::create_dir_all(&dir).ok();
    let messages = load_messages();
    println!("[claudulhu-rulyeh] loaded {} message(s) from history", messages.len());

    let poll_trigger = Arc::new(Notify::new());

    let state = Arc::new(AppState {
        messages:     Arc::new(Mutex::new(messages)),
        system:       RULYEH_SYSTEM_PROMPT.to_string(),
        containers:   Arc::new(Mutex::new(Vec::new())),
        poll_trigger: poll_trigger.clone(),
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
        .route("/clear",            post(clear_handler))
        .route("/containers",       get(containers_handler))
        .route("/containers/start", post(start_container_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("127.0.0.1:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind HTTP port");
    println!("[claudulhu-rulyeh] HTTP on {addr} (Noise proxy on 0.0.0.0:{noise_port})");

    axum::serve(listener, app).await.unwrap();
}
