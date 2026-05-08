//! Pre-flight work that used to live in the `docker-entrypoint*.sh` shell
//! scripts. Both roles call into this module before they bind their HTTP
//! listener: detecting the advertised public host, running the operator's
//! `STARTUP_SCRIPT`, optionally cloning a git repo (agent role only), and
//! rendering the connection QR code.

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

/// Resolve the host that the QR code advertises to mobile clients.
///
/// Precedence:
/// 1. `PUBLIC_HOST` env var (operator override; trusted as-is).
/// 2. Dev mode (`OCTO_DEV=1`) → `127.0.0.1`.
/// 3. Auto-detect via `https://api.ipify.org` with a 5s timeout.
///
/// In production the cluster always has internet egress, so step 3 is the
/// usual path on a fresh boot.
pub async fn resolve_public_host(role_log_prefix: &str) -> Result<String> {
    if let Ok(host) = std::env::var("PUBLIC_HOST") {
        if !host.is_empty() {
            return Ok(host);
        }
    }
    if std::env::var("OCTO_DEV").as_deref() == Ok("1") {
        info!("[{role_log_prefix}] DEV mode: using PUBLIC_HOST=127.0.0.1");
        return Ok("127.0.0.1".to_string());
    }
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

/// Run the operator-provided `STARTUP_SCRIPT` env var as a bash snippet, if set.
/// Used to install ad-hoc tools / fetch credentials at container boot. Failure
/// is surfaced as an error since whatever follows likely depends on the script
/// having succeeded.
pub async fn run_startup_script(role_log_prefix: &str) -> Result<()> {
    let Ok(script) = std::env::var("STARTUP_SCRIPT") else { return Ok(()); };
    if script.is_empty() { return Ok(()); }
    info!("[{role_log_prefix}] running STARTUP_SCRIPT...");
    let status = Command::new("bash").arg("-c").arg(&script).status().await
        .context("spawn STARTUP_SCRIPT bash")?;
    if !status.success() {
        anyhow::bail!("STARTUP_SCRIPT exited with {status}");
    }
    info!("[{role_log_prefix}] STARTUP_SCRIPT complete");
    Ok(())
}

/// Ensure `workspace` exists on disk, optionally cloning a git repo into it.
///
/// - `git_url = None`: just `mkdir -p workspace`. The agent runs there as a
///   general-purpose workspace; no git involvement.
/// - `git_url = Some(url)`: clone (or fetch if the workspace already has a
///   .git dir), set the configured user identity, and install a git
///   credential.helper if a `gh_token` is provided. Mirrors the bash
///   entrypoint 1:1, including the HTTPS URL token-rewrite.
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

    let Some(url) = git_url.map(str::trim).filter(|s| !s.is_empty()) else {
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

    let user_name  = std::env::var("GIT_USER_NAME").unwrap_or_else(|_| "octo".to_string());
    let user_email = std::env::var("GIT_USER_EMAIL").unwrap_or_else(|_| "octo@localhost".to_string());
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
