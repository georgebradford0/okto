//! Spawn the real `lair --role agent` binary as a black box for e2e tests.
//!
//! A child agent binds a *plaintext* HTTP/WS server on `127.0.0.1:<AGENT_PORT>`
//! (no Noise — lair reaches local children on loopback), so the test client
//! talks to it directly: raw HTTP over a `TcpStream` for `/worktrees`, and a
//! plaintext WebSocket for `/stream`. Each fixture gets its own temp data +
//! workspace dirs and is pointed at a `MockLlm`; the process is killed on drop.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::lair_proc::{free_port, lair_binary};
use super::mock_llm::{MockLlm, Turn};

/// A running `lair --role agent` process under test.
pub struct AgentProcess {
    pub http_port: u16,
    pub agent_dir: PathBuf,
    pub mock: MockLlm,
    child: tokio::process::Child,
    log_path: PathBuf,
    _tempdir: tempfile::TempDir,
}

impl AgentProcess {
    /// Start an agent whose workspace is a fresh git repo (one commit on the
    /// default branch), so the worktree tools are enabled.
    pub async fn start_with_repo(turns: Vec<Turn>) -> anyhow::Result<AgentProcess> {
        Self::spawn(turns, true).await
    }

    /// Start an agent whose workspace is a plain (non-git) dir — the worktree
    /// tools must NOT be offered.
    pub async fn start_without_repo(turns: Vec<Turn>) -> anyhow::Result<AgentProcess> {
        Self::spawn(turns, false).await
    }

    async fn spawn(turns: Vec<Turn>, with_repo: bool) -> anyhow::Result<AgentProcess> {
        let bin = lair_binary().await;
        let mock = MockLlm::start(turns).await?;

        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        // Layout mirrors production: <agent_dir>/{workspace,data}.
        let agent_dir = root.join("agent");
        let workspace = agent_dir.join("workspace");
        let data_dir = agent_dir.join("data");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&data_dir)?;

        if with_repo {
            init_git_repo(&workspace)?;
        }

        let http_port = free_port();
        let log_path = root.join("agent.log");
        let log = std::fs::File::create(&log_path)?;
        let log_err = log.try_clone()?;

        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("--role")
            .arg("agent")
            .env("HOME", &root)
            .env("OKTO_HOME", &root)
            .env("OKTO_DATA_DIR", &data_dir)
            .env("WORKSPACE_DIR", &workspace)
            .env("AGENT_PORT", http_port.to_string())
            // Local child: skip the bootstrap script and shell-env sourcing.
            .env("OKTO_LOCAL_CHILD", "1")
            .env("OKTO_SKIP_SHELL_ENV", "1")
            .env("ANTHROPIC_API_URL", mock.url())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("MODEL", "claude-test")
            .env("OKTO_RELAY_URL", "http://127.0.0.1:1")
            .env("RUST_LOG", "warn")
            .env_remove("OPENAI_API_URL")
            .env_remove("OPENAI_API_KEY")
            // No GIT_URL: ensure_workspace leaves the (already-laid-out) dir as is.
            .env_remove("GIT_URL")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true);
        let child = cmd.spawn().context("spawn lair --role agent")?;

        let mut agent = AgentProcess {
            http_port,
            agent_dir,
            mock,
            child,
            log_path,
            _tempdir: tempdir,
        };
        agent.wait_ready().await?;
        Ok(agent)
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let deadline = Duration::from_secs(30);
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "agent exited early with {status}\n--- agent.log ---\n{}",
                    self.log()
                );
            }
            if let Ok((200, _)) = self.http_get("/health").await {
                return Ok(());
            }
            if start.elapsed() > deadline {
                anyhow::bail!(
                    "agent did not become ready within {deadline:?}\n--- agent.log ---\n{}",
                    self.log()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Plaintext HTTP GET against the agent's loopback server. (status, body).
    pub async fn http_get(&self, path: &str) -> anyhow::Result<(u16, String)> {
        plain_http(self.http_port, "GET", path).await
    }

    /// Plaintext HTTP POST with a JSON body against the agent's loopback server.
    pub async fn http_post_json(&self, path: &str, body: &Value) -> anyhow::Result<(u16, String)> {
        plain_http_body(self.http_port, "POST", path, &body.to_string()).await
    }

    /// `GET /worktrees` parsed as a JSON array.
    pub async fn worktrees(&self) -> anyhow::Result<Vec<Value>> {
        let (status, body) = self.http_get("/worktrees").await?;
        anyhow::ensure!(status == 200, "GET /worktrees -> {status}: {body}");
        Ok(serde_json::from_str::<Vec<Value>>(&body)?)
    }

    /// Open a plaintext chat WebSocket to the agent's `/stream`.
    pub async fn chat(&self) -> anyhow::Result<ChatWs> {
        let url = format!("ws://127.0.0.1:{}/stream", self.http_port);
        let (ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .context("plaintext websocket upgrade to agent /stream")?;
        Ok(ChatWs { ws })
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Absolute path under the agent dir (e.g. `worktrees/feature-x`).
    pub fn agent_path(&self, rel: &str) -> PathBuf {
        self.agent_dir.join(rel)
    }
}

/// `git init` a repo with one commit so it has a default branch + HEAD to
/// branch worktrees from. Configures a throwaway identity so the commit lands
/// regardless of the host's global git config.
fn init_git_repo(dir: &std::path::Path) -> anyhow::Result<()> {
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "git {args:?} failed");
        Ok(())
    };
    run(&["init", "-q"])?;
    run(&["config", "user.email", "test@okto.local"])?;
    run(&["config", "user.name", "okto test"])?;
    run(&["config", "commit.gpgsign", "false"])?;
    std::fs::write(dir.join("README.md"), "# test repo\n")?;
    run(&["add", "-A"])?;
    run(&["commit", "-q", "-m", "init"])?;
    Ok(())
}

/// A plaintext chat WebSocket to the agent's `/stream`. Mirrors
/// `tunnel::ChatWs` but over a direct TCP connection (no Noise).
pub struct ChatWs {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl ChatWs {
    pub async fn send_user_message(&mut self, text: &str) -> anyhow::Result<()> {
        let frame = json!({"type":"user_message","text":text}).to_string();
        self.ws.send(Message::Text(frame)).await?;
        Ok(())
    }

    /// Read the next server event as JSON, auto-answering pings. Bounded.
    pub async fn next_event(&mut self) -> anyhow::Result<Option<Value>> {
        tokio::time::timeout(Duration::from_secs(20), self.next_event_inner())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for next chat event"))?
    }

    async fn next_event_inner(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            match self.ws.next().await {
                None => return Ok(None),
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t)?;
                    if v.get("type").and_then(|x| x.as_str()) == Some("ping") {
                        let id = v.get("id").cloned().unwrap_or(json!(0));
                        self.ws
                            .send(Message::Text(json!({"type":"pong","id":id}).to_string()))
                            .await
                            .ok();
                        continue;
                    }
                    return Ok(Some(v));
                }
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    }

    /// Wait for the `ready` frame the agent sends on stream open.
    pub async fn wait_ready(&mut self) -> anyhow::Result<()> {
        while let Some(ev) = self.next_event().await? {
            if ev.get("type").and_then(|x| x.as_str()) == Some("ready") {
                return Ok(());
            }
        }
        anyhow::bail!("stream closed before ready")
    }

    /// Drain events until a terminal frame, returning all events seen.
    pub async fn collect_turn(&mut self) -> anyhow::Result<Vec<Value>> {
        let mut events = Vec::new();
        while let Some(ev) = self.next_event().await? {
            let ty = ev.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string();
            events.push(ev);
            if matches!(ty.as_str(), "done" | "interrupted" | "error") {
                break;
            }
        }
        Ok(events)
    }
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// One-shot plaintext HTTP request against `127.0.0.1:port`. Reads the response
/// by `Content-Length`. Bounded so a stuck connection errors instead of hangs.
async fn plain_http(port: u16, method: &str, path: &str) -> anyhow::Result<(u16, String)> {
    tokio::time::timeout(HTTP_TIMEOUT, plain_http_inner(port, method, path, ""))
        .await
        .map_err(|_| anyhow::anyhow!("HTTP {method} {path} timed out after {HTTP_TIMEOUT:?}"))?
}

async fn plain_http_body(port: u16, method: &str, path: &str, body: &str) -> anyhow::Result<(u16, String)> {
    tokio::time::timeout(HTTP_TIMEOUT, plain_http_inner(port, method, path, body))
        .await
        .map_err(|_| anyhow::anyhow!("HTTP {method} {path} timed out after {HTTP_TIMEOUT:?}"))?
}

async fn plain_http_inner(port: u16, method: &str, path: &str, body: &str) -> anyhow::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: agent\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(req.as_bytes()).await?;

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before HTTP headers");
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("no status line in: {head:?}"))?;
    let content_len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    while buf.len() < header_end + content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_len]).to_string();
    Ok((status, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
