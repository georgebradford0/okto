//! A stand-in for lair's loopback management API.
//!
//! The CLI's `agents …` / `tasks …` subcommands POST/DELETE to
//! `http://127.0.0.1:<http_port>/…` (the port comes from `lair-launch.json`).
//! This binds an ephemeral loopback server, records every request lair would
//! have received, and replies with a scriptable status + JSON body — letting
//! the CLI's HTTP path be exercised end-to-end without a real container.

use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};

/// One request the CLI made to the management API.
#[derive(Clone, Debug)]
pub struct RecordedReq {
    pub method: String,
    pub path: String,
    /// Value of the `X-Okto-Token` header, if the CLI sent one.
    pub token: Option<String>,
}

struct MgmtState {
    requests: Mutex<Vec<RecordedReq>>,
    status: u16,
    body: Value,
}

/// A running mock management server. Drop it (or the owning fixture) to stop.
#[derive(Clone)]
pub struct MockMgmt {
    pub port: u16,
    state: Arc<MgmtState>,
}

impl MockMgmt {
    /// Start a server that replies `200 {}` to everything.
    pub async fn start() -> MockMgmt {
        Self::start_with(200, json!({})).await
    }

    /// Start a server that replies with the given status + JSON body to every
    /// request.
    pub async fn start_with(status: u16, body: Value) -> MockMgmt {
        let state = Arc::new(MgmtState {
            requests: Mutex::new(Vec::new()),
            status,
            body,
        });
        let app = Router::new()
            .fallback(handle)
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock mgmt listener");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockMgmt { port, state }
    }

    /// Every request the CLI has made so far, in order.
    pub fn requests(&self) -> Vec<RecordedReq> {
        self.state.requests.lock().unwrap().clone()
    }

    /// Convenience: did the CLI make a `<method> <path>` request?
    pub fn saw(&self, method: &str, path: &str) -> bool {
        self.requests()
            .iter()
            .any(|r| r.method == method && r.path == path)
    }
}

async fn handle(State(state): State<Arc<MgmtState>>, req: Request) -> impl IntoResponse {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let token = req
        .headers()
        .get("X-Okto-Token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    state
        .requests
        .lock()
        .unwrap()
        .push(RecordedReq { method, path, token });

    (
        StatusCode::from_u16(state.status).unwrap_or(StatusCode::OK),
        Json(state.body.clone()),
    )
}
