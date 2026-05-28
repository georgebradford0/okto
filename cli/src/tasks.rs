//! `okto tasks …` subcommands.
//!
//! `list` reads `tasks.json` files from disk directly so it works whether or
//! not lair is running. `stop` POSTs to lair's mgmt API
//! (`/tasks/:id/cancel` for lair-local tasks, `/agents/:name/tasks/:id/cancel`
//! proxied through to the child agent).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, error, info};

use crate::service;

/// Minimal local mirror of `okto_core::background::TaskRecord` — we only need
/// the fields the CLI displays. Defining it here avoids widening `okto_core`'s
/// public surface for a CLI-internal use case.
#[derive(Debug, Deserialize)]
struct TaskRow {
    task_id:       String,
    command:       String,
    status:        String,
    started_at:    u64,
    #[serde(default)]
    completed_at:  Option<u64>,
}

fn lair_tasks_path() -> PathBuf {
    service::lair_data_dir().join("session").join("tasks.json")
}

fn agent_tasks_path(name: &str) -> PathBuf {
    service::agents_dir().join(name).join("data").join("session").join("tasks.json")
}

fn read_tasks(path: &std::path::Path) -> Vec<TaskRow> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new(); };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        error!("[tasks] could not parse {}: {e}", path.display());
        Vec::new()
    })
}

fn format_ts(epoch_secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(epoch_secs);
    let now = std::time::SystemTime::now();
    match now.duration_since(dt) {
        Ok(d) => {
            let s = d.as_secs();
            if      s < 60        { format!("{s}s ago") }
            else if s < 3600      { format!("{}m ago", s / 60) }
            else if s < 86_400    { format!("{}h ago", s / 3600) }
            else                  { format!("{}d ago", s / 86_400) }
        }
        Err(_) => "future?".to_string(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn print_rows(label: &str, tasks: &[TaskRow]) {
    if tasks.is_empty() { return; }
    for t in tasks {
        let elapsed = match t.completed_at {
            Some(_) => format!("({})", format_ts(t.started_at)),
            None    => format_ts(t.started_at),
        };
        println!(
            "{:<18} {:<22} {:<10} {:<14} {}",
            t.task_id,
            label,
            t.status.to_lowercase(),
            elapsed,
            truncate(t.command.lines().next().unwrap_or(""), 50),
        );
    }
}

pub async fn list(agent: Option<&str>) -> Result<()> {
    let header = || println!(
        "{:<18} {:<22} {:<10} {:<14} {}",
        "TASK ID", "AGENT", "STATUS", "STARTED", "COMMAND",
    );

    match agent {
        Some(a) => {
            let rows = read_tasks(&agent_tasks_path(a));
            if rows.is_empty() {
                println!("No tasks for agent '{a}'.");
                return Ok(());
            }
            header();
            println!("{}", "-".repeat(80));
            print_rows(a, &rows);
        }
        None => {
            // Aggregate: lair + every agent in the registry.
            let lair_rows = read_tasks(&lair_tasks_path());
            let agents = crate::service::lair_data_dir()
                .parent()
                .map(|p| p.join("agents"))
                .filter(|p| p.exists())
                .map(|p| std::fs::read_dir(p).ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>())
                .unwrap_or_default();
            let mut total = lair_rows.len();
            let mut per_agent: Vec<(String, Vec<TaskRow>)> = Vec::new();
            for name in &agents {
                let rows = read_tasks(&agent_tasks_path(name));
                total += rows.len();
                per_agent.push((name.clone(), rows));
            }
            if total == 0 {
                println!("No tasks.");
                return Ok(());
            }
            header();
            println!("{}", "-".repeat(80));
            print_rows("lair", &lair_rows);
            for (name, rows) in &per_agent {
                print_rows(name, rows);
            }
        }
    }
    Ok(())
}

// ── stop ─────────────────────────────────────────────────────────────────────

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

const TOKEN_HEADER: &str = "X-Okto-Token";

fn mgmt_request(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match service::read_mgmt_token() {
        Some(t) => builder.header(TOKEN_HEADER, t),
        None    => builder,
    }
}

pub async fn stop(id: &str, agent: Option<&str>) -> Result<()> {
    let url = match agent {
        Some(a) => format!("{}/agents/{}/tasks/{}/cancel", service::lair_http_url(), a, id),
        None    => format!("{}/tasks/{}/cancel", service::lair_http_url(), id),
    };
    debug!("[tasks] POST {url}");
    let resp = mgmt_request(http_client().post(&url)).send().await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        error!("[tasks] stop '{id}' failed: lair returned {status}: {body}");
        anyhow::bail!("lair returned {status}: {body}");
    }
    let body: serde_json::Value = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));
    let fired = body.get("fired").and_then(|v| v.as_bool()).unwrap_or(false);
    if fired {
        info!("[tasks] cancelled '{id}'");
        println!("Stopped task '{id}'.");
    } else {
        println!("Task '{id}' not running (already finished, or wrong id).");
    }
    Ok(())
}
