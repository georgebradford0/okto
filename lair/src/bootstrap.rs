//! Pre-flight work that used to live in the `docker-entrypoint*.sh` shell
//! scripts. Both roles call into this module before they bind their HTTP
//! listener: detecting the advertised public host, running the operator's
//! `bootstrap.sh`, optionally cloning a git repo (agent role only), and
//! rendering the connection QR code.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Resolve the host that the QR code advertises to mobile clients.
///
/// Precedence:
/// 1. `PUBLIC_HOST` env var (operator override; trusted as-is).
/// 2. Dev mode (`OKTO_DEV=1`) → `127.0.0.1`.
/// 3. Auto-detect via `https://api.ipify.org` with a 5s timeout.
///
/// In production the cluster always has internet egress, so step 3 is the
/// usual path on a fresh boot.
pub async fn resolve_public_host(role_log_prefix: &str) -> Result<String> {
    if let Ok(host) = std::env::var("PUBLIC_HOST") {
        if !host.is_empty() {
            info!("[{role_log_prefix}] PUBLIC_HOST override: {host}");
            return Ok(host);
        }
    }
    if std::env::var("OKTO_DEV").as_deref() == Ok("1") {
        info!("[{role_log_prefix}] DEV mode: using PUBLIC_HOST=127.0.0.1");
        return Ok("127.0.0.1".to_string());
    }
    debug!("[{role_log_prefix}] auto-detecting public IP via api.ipify.org");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build reqwest client for ipify")?;
    let resp = client.get("https://api.ipify.org").send().await
        .context("auto-detect public IP via api.ipify.org (set PUBLIC_HOST to override)")?;
    let host = resp.text().await
        .context("read api.ipify.org response body")?
        .trim()
        .to_string();
    if host.is_empty() {
        anyhow::bail!("api.ipify.org returned an empty body");
    }
    info!("[{role_log_prefix}] detected public IP: {host}");
    Ok(host)
}

/// Run the operator-managed bootstrap script at `$OKTO_HOME/bootstrap.sh`
/// (`~/.okto/bootstrap.sh` on the host, `/data/bootstrap.sh` in the container),
/// if it exists, as a bash script. This is the single startup-customization
/// hook: install ad-hoc tools, fetch credentials, set git config, etc.
///
/// Because every local agent runs inside lair's own container, anything this
/// script installs into the shared filesystem (`apt-get install`, `npm i -g`,
/// `uv tool install`, …) lands on every agent's `PATH`. So only the container's
/// entrypoint process runs it — lair (`--role lair`), or a standalone remote
/// agent (`--role agent` as its own container's entrypoint). Locally-spawned
/// child agents inherit the result and never re-run it.
///
/// A missing file is a no-op. Failure aborts boot, since whatever follows
/// likely depends on the script having succeeded.
pub async fn run_bootstrap_script(role_log_prefix: &str) -> Result<()> {
    let okto_home = std::env::var("OKTO_HOME").unwrap_or_else(|_| "/data".to_string());
    let script = Path::new(&okto_home).join("bootstrap.sh");
    if !script.exists() {
        debug!("[{role_log_prefix}] no bootstrap script at {}; skipping", script.display());
        return Ok(());
    }
    info!("[{role_log_prefix}] running bootstrap script {}...", script.display());
    let status = Command::new("bash").arg(&script).status().await
        .with_context(|| format!("spawn bootstrap script {}", script.display()))?;
    if !status.success() {
        error!("[{role_log_prefix}] bootstrap script exited with {status}");
        anyhow::bail!("bootstrap script {} exited with {status}", script.display());
    }
    info!("[{role_log_prefix}] bootstrap script complete");
    Ok(())
}

/// Ensure `workspace` exists on disk, optionally cloning a git repo into it.
///
/// - `git_url = None` + empty workspace: just `mkdir -p`. Agent runs as a
///   generic workspace; no git involvement.
/// - `git_url = None` + `.git` already present: the workspace was populated
///   externally (typically by lair over SSH during remote-agent registration).
///   Set the git user identity and treat the agent as repo-bound.
/// - `git_url = Some(url)`: clone (or fetch if the workspace already has a
///   .git dir), set user identity, and install a git credential.helper if a
///   `gh_token` is provided. Mirrors the bash entrypoint 1:1.
///
/// Returns `true` if a git repo is present in `workspace` after the call,
/// `false` if the agent runs without one. This is the signal the caller
/// uses to pick a repo-aware vs. generic system prompt.
pub async fn ensure_workspace(
    workspace: &Path,
    git_url:   Option<&str>,
    gh_token:  Option<&str>,
) -> Result<bool> {
    std::fs::create_dir_all(workspace)
        .with_context(|| format!("create workspace {}", workspace.display()))?;

    debug!("[bootstrap] ensure_workspace at {} (git_url={})", workspace.display(), git_url.unwrap_or("(none)"));
    let Some(url) = git_url.map(str::trim).filter(|s| !s.is_empty()) else {
        // No GIT_URL — but the workspace may have been populated by lair
        // out-of-band (remote-agent flow: lair clones via SSH after the
        // container is up). Detect that and surface it as a repo so the
        // system prompt is correct.
        if workspace.join(".git").is_dir() {
            info!(
                "[agent] no GIT_URL set, but workspace at {} already contains a .git — \
                 treating as repo-bound",
                workspace.display(),
            );
            let workspace_str = workspace.to_string_lossy().to_string();
            let user_name  = std::env::var("GIT_USER_NAME").unwrap_or_else(|_| "okto".to_string());
            let user_email = std::env::var("GIT_USER_EMAIL").unwrap_or_else(|_| "okto@localhost".to_string());
            run_git(&["-C", &workspace_str, "config", "user.name",  &user_name]).await?;
            run_git(&["-C", &workspace_str, "config", "user.email", &user_email]).await?;
            return Ok(true);
        }
        info!("[agent] no GIT_URL set — running as a generic agent in {}", workspace.display());
        return Ok(false);
    };

    // For HTTPS clones we splice the token into the URL: `https://x:<token>@host/...`.
    // The entrypoint script used a sed regex; we do the same shape in Rust.
    let clone_url: String = if url.starts_with("https://") {
        let token = gh_token.filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("GH_TOKEN is required for HTTPS git URLs"))?;
        let rest = url.trim_start_matches("https://");
        // Drop any existing `user[:pass]@` segment before splicing in the token.
        let rest = match rest.find('@') {
            Some(i) => &rest[i + 1..],
            None    => rest,
        };
        format!("https://x-token:{token}@{rest}")
    } else {
        url.to_string()
    };

    let workspace_str = workspace.to_string_lossy().to_string();
    let dot_git = workspace.join(".git");

    if dot_git.exists() {
        info!("[agent] updating existing repo at {workspace_str}");
        run_git(&["-C", &workspace_str, "remote", "set-url", "origin", &clone_url]).await?;
        run_git(&["-C", &workspace_str, "fetch", "--all"]).await?;
    } else {
        info!("[agent] cloning {url} into {workspace_str}");
        run_git(&["clone", &clone_url, &workspace_str]).await?;
    }

    let user_name  = std::env::var("GIT_USER_NAME").unwrap_or_else(|_| "okto".to_string());
    let user_email = std::env::var("GIT_USER_EMAIL").unwrap_or_else(|_| "okto@localhost".to_string());
    run_git(&["-C", &workspace_str, "config", "user.name",  &user_name]).await?;
    run_git(&["-C", &workspace_str, "config", "user.email", &user_email]).await?;

    if let Some(token) = gh_token.filter(|t| !t.is_empty()) {
        // Inline credential helper so `gh` and `git push` over HTTPS can authenticate
        // without re-prompting. Token is interpolated into the helper script verbatim.
        let helper = format!("!f() {{ echo username=x-token; echo password={token}; }}; f");
        run_git(&["-C", &workspace_str, "config", "credential.helper", &helper]).await?;
    }

    Ok(true)
}

async fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git").args(args).status().await
        .with_context(|| format!("spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        error!("[bootstrap] `git {}` exited with {status}", args.join(" "));
        anyhow::bail!("`git {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Print the connection QR code (host:port:pubkey) using the same format the
/// previous shell entrypoints used: `2:<host>:<port>:<pubkey_base32>`. Called
/// by each role after its HTTP listener has bound, so the user can never scan
/// before the server can accept the connection.
pub fn print_qr(role_log_prefix: &str, host: &str, port: u16, pubkey_b32: &str) {
    let qr_data = format!("2:{host}:{port}:{pubkey_b32}");
    let code = match qrcode::QrCode::new(&qr_data) {
        Ok(c) => c,
        Err(e) => {
            warn!("[{role_log_prefix}] could not render QR code ({e}); QR data: {qr_data}");
            return;
        }
    };
    let image = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .build();
    println!();
    println!("[{role_log_prefix}] Scan this QR code with the app to connect:");
    println!();
    println!("{image}");
    println!();
}
