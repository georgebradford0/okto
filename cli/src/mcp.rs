use std::collections::HashMap;

use anyhow::{Context, Result};
use claudulhu_k8s_ops::k8s;
use serde::{Deserialize, Serialize};

const MCP_PATH: &str = "/data/mcp.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct McpServerConfig {
    name:    String,
    command: String,
    #[serde(default)]
    args:    Vec<String>,
    #[serde(default)]
    env:     HashMap<String, String>,
}

async fn read_config(pod: &str) -> Result<Vec<McpServerConfig>> {
    let raw = k8s::exec_in_pod(pod, &["cat", MCP_PATH]).await;
    match raw {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).context("parse mcp.json")
        }
        _ => Ok(vec![]),
    }
}

async fn write_config(pod: &str, configs: &[McpServerConfig]) -> Result<()> {
    let json = serde_json::to_string_pretty(configs)?;
    k8s::write_pod_file(pod, MCP_PATH, &json).await
}

async fn get_pod(container: &str) -> Result<String> {
    let client = k8s::build_client().await?;
    k8s::get_running_pod(&client, container).await
}

pub async fn list(container: &str) -> Result<()> {
    let pod     = get_pod(container).await?;
    let configs = read_config(&pod).await?;
    if configs.is_empty() {
        println!("No MCP servers configured in '{container}'.");
        return Ok(());
    }
    for c in &configs {
        let args = if c.args.is_empty() {
            String::new()
        } else {
            format!(" {}", c.args.join(" "))
        };
        println!("{}: {}{}", c.name, c.command, args);
        for k in c.env.keys() {
            println!("    {k}");
        }
    }
    Ok(())
}

pub async fn add(
    container: &str,
    name: &str,
    command: &str,
    args: &[String],
    env_pairs: &[String],
) -> Result<()> {
    let pod = get_pod(container).await?;
    let mut configs = read_config(&pod).await?;

    if configs.iter().any(|c| c.name == name) {
        anyhow::bail!("MCP server '{name}' already exists in '{container}'");
    }

    let mut env = HashMap::new();
    for pair in env_pairs {
        let (k, v) = pair.split_once('=')
            .with_context(|| format!("invalid env pair '{pair}': expected KEY=VALUE"))?;
        env.insert(k.to_string(), v.to_string());
    }

    configs.push(McpServerConfig {
        name:    name.to_string(),
        command: command.to_string(),
        args:    args.to_vec(),
        env,
    });

    println!("→ writing config to '{container}'");
    write_config(&pod, &configs).await?;

    println!("→ waiting for hot-reload watcher (~2s)...");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    println!("→ spawning MCP server process...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    println!("→ checking pod logs...");
    let logs = tokio::process::Command::new("kubectl")
        .args(["logs", "-n", k8s::NAMESPACE, &pod, "--since=15s"])
        .output().await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    // Print any [mcp] lines relevant to this server so the user can see what happened.
    for line in logs.lines() {
        if line.contains("[mcp]") && (line.contains(&format!("'{name}'")) || line.contains("hot-reload")) {
            println!("  {line}");
        }
    }

    let connected_marker  = format!("[mcp] '{name}' connected");
    let spawn_fail_marker = format!("[mcp] failed to spawn '{name}'");
    let init_fail_marker  = format!("[mcp] '{name}' initialize failed");
    let no_tools_marker   = format!("[mcp] warning: server '{name}' advertised no tools");

    let success = logs.contains(&connected_marker) || logs.contains(&no_tools_marker);

    if !success {
        configs.retain(|c| c.name != name);
        write_config(&pod, &configs).await?;
    }

    if logs.contains(&connected_marker) {
        println!("✓ MCP server '{name}' connected successfully.");
    } else if logs.contains(&no_tools_marker) {
        println!("⚠ MCP server '{name}' connected but advertised no tools.");
    } else if logs.contains(&spawn_fail_marker) {
        anyhow::bail!("MCP server '{name}' failed to spawn — command not found or not executable.");
    } else if logs.contains(&init_fail_marker) {
        anyhow::bail!("MCP server '{name}' process started but MCP handshake failed.");
    } else {
        anyhow::bail!("MCP server '{name}' did not confirm connection within timeout — entry not saved. Run `claudulhu logs {container}` to investigate.");
    }

    Ok(())
}

pub async fn import_from_file(container: &str, path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let entries: Vec<McpServerConfig> = serde_json::from_str(&text)
        .context("failed to parse JSON — expected an array of MCP server objects")?;

    if entries.is_empty() {
        println!("No entries found in '{}'.", path.display());
        return Ok(());
    }

    println!("Importing {} MCP server(s) into '{container}'...", entries.len());
    let mut failed = 0usize;
    for entry in &entries {
        let env_pairs: Vec<String> = entry.env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if let Err(e) = add(container, &entry.name, &entry.command, &entry.args, &env_pairs).await {
            eprintln!("✗ '{}': {e}", entry.name);
            failed += 1;
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} of {} server(s) failed to import", entries.len());
    }
    Ok(())
}

pub async fn remove(container: &str, name: &str) -> Result<()> {
    let pod = get_pod(container).await?;
    let mut configs = read_config(&pod).await?;
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        anyhow::bail!("MCP server '{name}' not found in '{container}'");
    }
    write_config(&pod, &configs).await?;
    println!("Removed MCP server '{name}' from '{container}'.");
    Ok(())
}
