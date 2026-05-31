//! Spawn the real `lair --role lair` binary as a black box for e2e tests.
//!
//! Each fixture gets its own temp `OKTO_HOME`/data dir and ephemeral ports, is
//! pointed at a `MockLlm` via `ANTHROPIC_API_URL`, and is killed on drop.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::OnceCell;

use super::mock_llm::{MockLlm, Turn};
use super::tunnel;

/// Built once per test process: ensures the lair binary is compiled and returns
/// its path.
static LAIR_BIN: OnceCell<PathBuf> = OnceCell::const_new();

async fn lair_binary() -> PathBuf {
    LAIR_BIN
        .get_or_init(|| async {
            let release = std::env::current_exe()
                .map(|p| p.to_string_lossy().contains("/release/"))
                .unwrap_or(false);
            // Build the lair bin from this test process (the workspace build
            // lock is already released by the time tests run).
            let mut cmd = tokio::process::Command::new(env!("CARGO"));
            cmd.args(["build", "-p", "lair", "--bin", "lair"]);
            if release {
                cmd.arg("--release");
            }
            let status = cmd.status().await.expect("run `cargo build -p lair`");
            assert!(status.success(), "`cargo build -p lair` failed");

            // current_exe: target/<profile>/deps/<test>-<hash>
            let exe = std::env::current_exe().expect("current_exe");
            let profile_dir = exe
                .ancestors()
                .nth(2)
                .expect("target/<profile> dir")
                .to_path_buf();
            profile_dir.join("lair")
        })
        .await
        .clone()
}

/// Grab a currently-free loopback port. Inherently racy (the port is released
/// before lair binds it) but fine for tests.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running lair process under test.
pub struct LairProcess {
    pub noise_port: u16,
    pub http_port: u16,
    pub home: PathBuf,
    pub mock: MockLlm,
    child: tokio::process::Child,
    log_path: PathBuf,
    _tempdir: tempfile::TempDir,
}

impl LairProcess {
    /// Start lair with the given scripted model turns. Waits until `/health`
    /// answers over the Noise tunnel.
    pub async fn start(turns: Vec<Turn>) -> anyhow::Result<LairProcess> {
        let bin = lair_binary().await;
        let mock = MockLlm::start(turns).await?;

        let tempdir = tempfile::tempdir()?;
        let home = tempdir.path().to_path_buf();
        let data_dir = home.join("lair");
        let agents_dir = home.join("agents");
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&agents_dir)?;

        let noise_port = free_port();
        let http_port = free_port();
        let log_path = home.join("lair.log");
        let log = std::fs::File::create(&log_path)?;
        let log_err = log.try_clone()?;

        let child = tokio::process::Command::new(&bin)
            .arg("--role")
            .arg("lair")
            // Dev keypair → the test client knows the server's static pubkey,
            // and dev mode keeps public-host resolution offline.
            .env("OKTO_DEV", "1")
            .env("PUBLIC_HOST", "127.0.0.1")
            .env("HOME", &home)
            .env("OKTO_HOME", &home)
            .env("OKTO_DATA_DIR", &data_dir)
            .env("OKTO_AGENTS_DIR", &agents_dir)
            .env("NOISE_PORT", noise_port.to_string())
            .env("PUBLIC_PORT", noise_port.to_string())
            .env("OKTO_HTTP_PORT", http_port.to_string())
            .env("ANTHROPIC_API_URL", mock.url())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("MODEL", "claude-test")
            // Keep notifications from reaching the real relay.
            .env("OKTO_RELAY_URL", "http://127.0.0.1:1")
            .env("RUST_LOG", "warn")
            // Don't inherit a developer's ANTHROPIC/OPENAI env into the child.
            .env_remove("OPENAI_API_URL")
            .env_remove("OPENAI_API_KEY")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .context("spawn lair process")?;

        let mut lair = LairProcess {
            noise_port,
            http_port,
            home,
            mock,
            child,
            log_path,
            _tempdir: tempdir,
        };
        lair.wait_ready().await?;
        Ok(lair)
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let deadline = Duration::from_secs(30);
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "lair exited early with {status}\n--- lair.log ---\n{}",
                    self.log()
                );
            }
            if let Ok((200, _)) = tunnel::http_get(self.noise_port, "/health").await {
                return Ok(());
            }
            if start.elapsed() > deadline {
                anyhow::bail!(
                    "lair did not become ready within {deadline:?}\n--- lair.log ---\n{}",
                    self.log()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// HTTP GET over the tunnel against lair's own routes.
    pub async fn http_get(&self, path: &str) -> anyhow::Result<(u16, String)> {
        tunnel::http_get(self.noise_port, path).await
    }

    /// HTTP POST (empty body) over the tunnel against lair's own routes.
    pub async fn http_post(&self, path: &str) -> anyhow::Result<(u16, String)> {
        tunnel::http_post(self.noise_port, path).await
    }

    /// Open a chat WebSocket to lair's top-level `/stream`.
    pub async fn chat(&self) -> anyhow::Result<tunnel::ChatWs> {
        tunnel::ChatWs::connect(self.noise_port, "/stream").await
    }

    /// Current contents of the captured process log (stdout+stderr).
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Path under the fixture's data dir for assertions on persisted state.
    pub fn data_path(&self, rel: &str) -> PathBuf {
        self.home.join("lair").join(rel)
    }
}
