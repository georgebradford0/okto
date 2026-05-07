use std::collections::HashMap;

use anyhow::{Context, Result};
use octo_k8s_ops::k8s;
use serde::{Deserialize, Serialize};

const MCP_PATH: &str = "/data/mcp.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct McpServerConfig {
    name:    String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    args:    Vec<String>,
    #[serde(default)]
    env:     HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url:     Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    headers: HashMap<String, String>,
}

async fn read_config(pod_name: &str) -> Result<Vec<McpServerConfig>> {
    let raw = k8s::exec_in_pod(pod_name, &["cat", MCP_PATH]).await;
    match raw {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).context("parse mcp.json")
        }
        _ => Ok(vec![]),
    }
}

async fn write_config(pod_name: &str, configs: &[McpServerConfig]) -> Result<()> {
    let json = serde_json::to_string_pretty(configs)?;
    k8s::write_pod_file(pod_name, MCP_PATH, &json).await
}

async fn running_pod(pod: &str) -> Result<String> {
    let client = k8s::build_client().await?;
    k8s::get_running_pod(&client, pod).await
}

pub async fn list(pod: &str) -> Result<()> {
    let pod_name = running_pod(pod).await?;
    let configs  = read_config(&pod_name).await?;
    if configs.is_empty() {
        println!("No MCP servers configured in '{pod}'.");
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
    pod: &str,
    name: &str,
    command: &str,
    args: &[String],
    env_pairs: &[String],
) -> Result<()> {
    let pod_name = running_pod(pod).await?;
    let mut configs = read_config(&pod_name).await?;

    if configs.iter().any(|c| c.name == name) {
        anyhow::bail!("MCP server '{name}' already exists in '{pod}'");
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
        url:     None,
        headers: HashMap::new(),
    });

    println!("→ writing config to '{pod}'");
    write_config(&pod_name, &configs).await?;

    let connected_marker  = format!("[mcp] '{name}' connected");
    let spawn_fail_marker = format!("[mcp] failed to spawn '{name}'");
    let init_fail_marker  = format!("[mcp] '{name}' initialize failed");
    let no_tools_marker   = format!("[mcp] warning: server '{name}' advertised no tools");

    println!("→ waiting for MCP server to connect (up to 60s)...");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut logs = String::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        logs = tokio::process::Command::new("kubectl")
            .args(["logs", "-n", k8s::NAMESPACE, &format!("deployment/{pod}"), "--since=75s"])
            .output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let done = logs.contains(&connected_marker)
            || logs.contains(&no_tools_marker)
            || logs.contains(&spawn_fail_marker)
            || logs.contains(&init_fail_marker);
        if done || tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    // Print any [mcp] lines relevant to this server so the user can see what happened.
    for line in logs.lines() {
        if line.contains("[mcp]") && (line.contains(&format!("'{name}'")) || line.contains("hot-reload")) {
            println!("  {line}");
        }
    }

    let success = logs.contains(&connected_marker) || logs.contains(&no_tools_marker);

    if !success {
        configs.retain(|c| c.name != name);
        let current_pod_name = running_pod(pod).await.unwrap_or(pod_name);
        write_config(&current_pod_name, &configs).await?;
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
        anyhow::bail!("MCP server '{name}' did not confirm connection within timeout — entry not saved. Run `octo logs {pod}` to investigate.");
    }

    Ok(())
}

pub async fn import_from_file(pod: &str, path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let entries: Vec<McpServerConfig> = serde_json::from_str(&text)
        .context("failed to parse JSON — expected an array of MCP server objects")?;

    if entries.is_empty() {
        println!("No entries found in '{}'.", path.display());
        return Ok(());
    }

    // Remove any existing entries whose names match the import file so that
    // add() sees them as new — enabling upsert semantics on re-import.
    let pod_name = running_pod(pod).await?;
    let mut existing = read_config(&pod_name).await?;
    let import_names: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.name.as_str()).collect();
    let updating: Vec<&str> = existing.iter()
        .filter(|c| import_names.contains(c.name.as_str()))
        .map(|c| c.name.as_str())
        .collect();
    if !updating.is_empty() {
        println!("Updating existing server(s): {}", updating.join(", "));
        existing.retain(|c| !import_names.contains(c.name.as_str()));
        write_config(&pod_name, &existing).await?;
    }

    println!("Importing {} MCP server(s) into '{pod}'...", entries.len());
    let mut failed = 0usize;
    for entry in &entries {
        let env_pairs: Vec<String> = entry.env.iter()
            .map(|(k, v)| {
                let resolved = if v.starts_with("${") && v.ends_with('}') {
                    let var = &v[2..v.len() - 1];
                    match std::env::var(var) {
                        Ok(val) => val,
                        Err(_) => {
                            eprintln!("warning: ${{{var}}} not set in local environment — storing unexpanded");
                            v.clone()
                        }
                    }
                } else {
                    v.clone()
                };
                format!("{k}={resolved}")
            })
            .collect();
        if let Err(e) = add(pod, &entry.name, &entry.command, &entry.args, &env_pairs).await {
            eprintln!("✗ '{}': {e}", entry.name);
            failed += 1;
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} of {} server(s) failed to import", entries.len());
    }
    Ok(())
}

pub async fn remove(pod: &str, name: &str) -> Result<()> {
    let pod_name = running_pod(pod).await?;
    let mut configs = read_config(&pod_name).await?;
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        anyhow::bail!("MCP server '{name}' not found in '{pod}'");
    }
    write_config(&pod_name, &configs).await?;
    println!("Removed MCP server '{name}' from '{pod}'.");
    Ok(())
}
