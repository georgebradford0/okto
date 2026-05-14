//! `octo agents …` subcommands.
//!
//! The CLI talks to lair's loopback management API (`http://127.0.0.1:8000`)
//! for start/stop/delete. List reads the registry file directly so the CLI
//! still works when lair isn't running.

use std::path::PathBuf;

use anyhow::{Context, Result};
use octo_core::Registry;

use crate::service;

fn registry_path() -> PathBuf {
    service::lair_data_dir().join("agents.json")
}

pub async fn list() -> Result<()> {
    let path = registry_path();
    if !path.exists() {
        println!("No agents (lair hasn't been started yet — no registry at {}).", path.display());
        return Ok(());
    }
    let reg = Registry::load(path).context("load agent registry")?;
    let agents = reg.list();
    if agents.is_empty() {
        println!("No agents.");
        return Ok(());
    }
    println!("{:<28} {:<8} {:<8} {:<6} {:<8} {}", "NAME", "KIND", "STATUS", "PORT", "PID", "HOST");
    println!("{}", "-".repeat(80));
    for a in agents {
        println!(
            "{:<28} {:<8} {:<8} {:<6} {:<8} {}",
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
/// Token lives in `~/.octo/lair/.mgmt-token` (chmod 0600); regenerated on
/// the next `octo init` if missing.
const TOKEN_HEADER: &str = "X-Octo-Token";

fn mgmt_request(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match service::read_mgmt_token() {
        Some(t) => builder.header(TOKEN_HEADER, t),
        None    => builder, // lair is running with auth disabled — request still goes through
    }
}

pub async fn start(name: &str) -> Result<()> {
    let url = format!("{}/agents/{}/start", service::lair_http_url(), name);
    let resp = mgmt_request(http_client().post(&url)).send().await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        anyhow::bail!("lair returned {status}: {body}");
    }
    println!("Started '{name}'.");
    Ok(())
}

pub async fn stop(name: &str) -> Result<()> {
    let url = format!("{}/agents/{}/stop", service::lair_http_url(), name);
    let resp = mgmt_request(http_client().post(&url)).send().await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        anyhow::bail!("lair returned {status}: {body}");
    }
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
    let url = format!("{}/agents/{}", service::lair_http_url(), name);
    let resp = mgmt_request(http_client().delete(&url)).send().await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        anyhow::bail!("lair returned {status}: {body}");
    }
    println!("Deleted '{name}'.");
    Ok(())
}
