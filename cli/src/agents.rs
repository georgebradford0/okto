//! `okto agents …` subcommands.
//!
//! The CLI talks to lair's loopback management API (`http://127.0.0.1:8000`)
//! for start/stop/delete. List reads the registry file directly so the CLI
//! still works when lair isn't running.

use std::path::PathBuf;

use anyhow::{Context, Result};
use okto_core::Registry;
use tracing::{debug, error, info};

use crate::service;

fn registry_path() -> PathBuf {
    service::lair_data_dir().join("agents.json")
}

/// Resolve a user-supplied agent reference to its route-safe `slug` (the
/// identifier lair's management API is keyed on). Accepts either the slug
/// itself (exact match) or a unique display `name`. Reads the on-disk
/// registry directly, so it works whether or not lair is running. Falls back
/// to the reference verbatim if the registry file is absent or unreadable, so
/// a stale-but-running lair can still be addressed by slug.
pub(crate) fn resolve_slug(reference: &str) -> String {
    let path = registry_path();
    let Ok(reg) = Registry::load(path) else { return reference.to_string(); };
    if reg.get(reference).is_some() {
        return reference.to_string();
    }
    let mut by_name = reg.list().iter().filter(|a| a.name == reference);
    match (by_name.next(), by_name.next()) {
        (Some(only), None) => only.slug.clone(),
        // No match (let lair return a clean 404) or ambiguous display name
        // (prefer the verbatim reference so the user sees lair's error).
        _ => reference.to_string(),
    }
}

pub async fn list() -> Result<()> {
    let path = registry_path();
    if !path.exists() {
        // The registry file is created lazily when the first agent is
        // deployed, so its absence means "no agents", not "lair is down".
        if service::is_running() {
            println!("No agents.");
        } else {
            println!("No agents (lair is not running — run `okto init` to start it).");
        }
        return Ok(());
    }
    let reg = Registry::load(path).context("load agent registry")?;
    let agents = reg.list();
    if agents.is_empty() {
        println!("No agents.");
        return Ok(());
    }
    println!("{:<24} {:<24} {:<8} {:<8} {:<6} {:<8} {}", "ID", "NAME", "KIND", "STATUS", "PORT", "PID", "HOST");
    println!("{}", "-".repeat(96));
    for a in agents {
        println!(
            "{:<24} {:<24} {:<8} {:<8} {:<6} {:<8} {}",
            a.slug,
            a.name,
            if a.is_remote() { "remote" } else { "local" },
            a.status.as_wire_str(),
            a.port,
            a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string()),
            a.host.as_deref().unwrap_or("127.0.0.1"),
        );
    }
    Ok(())
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Header name lair expects on every state-mutating management endpoint.
/// Token lives in `~/.okto/lair/.mgmt-token` (chmod 0600); regenerated on
/// the next `okto init` if missing. Header name stays `X-Okto-Token` because
/// lair (out of scope for this rename) still gates on that exact name.
const TOKEN_HEADER: &str = "X-Okto-Token";

fn mgmt_request(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match service::read_mgmt_token() {
        Some(t) => builder.header(TOKEN_HEADER, t),
        None    => builder, // lair is running with auth disabled — request still goes through
    }
}

pub async fn start(name: &str) -> Result<()> {
    let slug = resolve_slug(name);
    let url = format!("{}/agents/{}/start", service::lair_http_url(), slug);
    debug!("[agents] POST {url}");
    let resp = mgmt_request(http_client().post(&url)).send().await
        .with_context(|| format!("POST {url}"))?;
    debug!("[agents] POST {url} -> {}", resp.status());
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        error!("[agents] start '{name}' failed: lair returned {status}: {body}");
        anyhow::bail!("lair returned {status}: {body}");
    }
    info!("[agents] agent '{name}' started");
    println!("Started '{name}'.");
    Ok(())
}

pub async fn stop(name: &str) -> Result<()> {
    let slug = resolve_slug(name);
    let url = format!("{}/agents/{}/stop", service::lair_http_url(), slug);
    debug!("[agents] POST {url}");
    let resp = mgmt_request(http_client().post(&url)).send().await
        .with_context(|| format!("POST {url}"))?;
    debug!("[agents] POST {url} -> {}", resp.status());
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        error!("[agents] stop '{name}' failed: lair returned {status}: {body}");
        anyhow::bail!("lair returned {status}: {body}");
    }
    info!("[agents] agent '{name}' stopped");
    println!("Stopped '{name}'.");
    Ok(())
}

pub async fn delete(name: &str, yes: bool) -> Result<()> {
    if !yes {
        use std::io::Write;
        print!("Delete '{name}' and its data + workspace dirs? This is irreversible. [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }
    let slug = resolve_slug(name);
    let url = format!("{}/agents/{}", service::lair_http_url(), slug);
    debug!("[agents] DELETE {url}");
    let resp = mgmt_request(http_client().delete(&url)).send().await
        .with_context(|| format!("DELETE {url}"))?;
    debug!("[agents] DELETE {url} -> {}", resp.status());
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        error!("[agents] delete '{name}' failed: lair returned {status}: {body}");
        anyhow::bail!("lair returned {status}: {body}");
    }
    info!("[agents] agent '{name}' deleted");
    println!("Deleted '{name}'.");
    Ok(())
}
