//! Spawn the real `okto` CLI binary as a black box for e2e tests.
//!
//! Each fixture gets its own temp `HOME` (so `~/.okto/...` resolves into the
//! tempdir) and the CLI is run as a child process. Tests assert on the
//! process's stdout / stderr / exit code and on the files the command leaves
//! behind under `~/.okto`. No docker and no network are required by the
//! commands these tests exercise — anything that shells out to docker or the
//! lair container is either avoided or pointed at a `MockMgmt` server.

use std::path::PathBuf;

use tokio::sync::OnceCell;

/// Built once per test process: ensures the `okto` binary is compiled and
/// returns its path. Mirrors `lair_proc::lair_binary`.
static OKTO_BIN: OnceCell<PathBuf> = OnceCell::const_new();

async fn okto_binary() -> PathBuf {
    OKTO_BIN
        .get_or_init(|| async {
            let release = std::env::current_exe()
                .map(|p| p.to_string_lossy().contains("/release/"))
                .unwrap_or(false);
            let mut cmd = tokio::process::Command::new(env!("CARGO"));
            cmd.args(["build", "-p", "okto", "--bin", "okto"]);
            if release {
                cmd.arg("--release");
            }
            let status = cmd.status().await.expect("run `cargo build -p okto`");
            assert!(status.success(), "`cargo build -p okto` failed");

            // current_exe: target/<profile>/deps/<test>-<hash>
            let exe = std::env::current_exe().expect("current_exe");
            let profile_dir = exe
                .ancestors()
                .nth(2)
                .expect("target/<profile> dir")
                .to_path_buf();
            profile_dir.join("okto")
        })
        .await
        .clone()
}

/// The captured result of one `okto` invocation.
pub struct CliOutput {
    /// Process exit code (`None` if killed by a signal).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    /// True when the process exited 0.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Panic with a diagnostic dump unless the process exited 0.
    pub fn assert_ok(&self) -> &Self {
        assert!(
            self.ok(),
            "expected exit 0, got {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr,
        );
        self
    }

    /// Panic with a diagnostic dump unless the process exited non-zero.
    pub fn assert_err(&self) -> &Self {
        assert!(
            !self.ok(),
            "expected a non-zero exit, got 0\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr,
        );
        self
    }
}

/// A temp-`HOME` fixture for running the CLI.
pub struct OktoCli {
    pub home: PathBuf,
    _tempdir: tempfile::TempDir,
}

impl OktoCli {
    /// Create an empty fixture. `~/.okto` is created so seeding helpers can
    /// drop files straight in.
    pub fn new() -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let home = tempdir.path().to_path_buf();
        std::fs::create_dir_all(home.join(".okto")).expect("create ~/.okto");
        OktoCli { home, _tempdir: tempdir }
    }

    /// `<home>/.okto`.
    pub fn okto_dir(&self) -> PathBuf {
        self.home.join(".okto")
    }

    /// `<home>/.okto/lair`.
    pub fn lair_dir(&self) -> PathBuf {
        self.okto_dir().join("lair")
    }

    /// `<home>/.okto/agents`.
    pub fn agents_dir(&self) -> PathBuf {
        self.okto_dir().join("agents")
    }

    /// Write a file at `rel` (relative to `home`), creating parent dirs.
    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.home.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, contents)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    /// Write raw bytes at `rel` (relative to `home`), creating parent dirs.
    pub fn write_bytes(&self, rel: &str, contents: &[u8]) {
        let path = self.home.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, contents)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    /// Read a file at `rel` (relative to `home`); empty string if absent.
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.home.join(rel)).unwrap_or_default()
    }

    /// True if a file at `rel` (relative to `home`) exists.
    pub fn exists(&self, rel: &str) -> bool {
        self.home.join(rel).exists()
    }

    /// Write a `lair-launch.json` so commands that read the launch record
    /// (e.g. `qr`, and the management-API base URL used by `agents`/`tasks`)
    /// resolve the given http port.
    pub fn write_launch(&self, noise_port: u16, http_port: u16) {
        self.write(
            ".okto/lair-launch.json",
            &format!(
                "{{\"noise_port\":{noise_port},\"http_port\":{http_port},\"image\":null}}"
            ),
        );
    }

    /// Run `okto <args>` against this fixture's HOME. Strips any okto env the
    /// developer's shell may have set so the run is hermetic.
    pub async fn run(&self, args: &[&str]) -> CliOutput {
        let bin = okto_binary().await;
        let out = tokio::process::Command::new(&bin)
            .args(args)
            .env("HOME", &self.home)
            // Force config/data resolution to land under the fixture's HOME.
            .env_remove("OKTO_HOME")
            .env_remove("OKTO_DATA_DIR")
            .env_remove("OKTO_AGENTS_DIR")
            .env_remove("OKTO_LAIR_IMAGE")
            // Quiet tracing so stderr only carries genuine user-facing output.
            .env("OKTO_LOG", "error")
            .output()
            .await
            .expect("run okto");
        CliOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

impl Default for OktoCli {
    fn default() -> Self {
        Self::new()
    }
}
