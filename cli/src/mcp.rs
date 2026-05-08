use std::collections::HashMap;

use anyhow::{Context, Result};
use octo_k8s_ops::k8s;
use serde::{Deserialize, Serialize};

const MCP_PATH: &str = "/data/mcp.json";

/// Expand a `${VAR}` reference from the host environment.
/// If the variable is not set, warn and return the original string unexpanded.
fn expand_host_var(v: &str) -> String {
    if v.starts_with("${") && v.ends_with('}') {
        let var = &v[2..v.len() - 1];
        match std::env::var(var) {
            Ok(val) => val,
            Err(_) => {
                eprintln!("warning: ${{{var}}} not set in local environment — storing unexpanded");
                v.to_string()
            }
        }
    } else {
        v.to_string()
    }
}

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

async fn running_pod(agent: &str) -> Result<String> {
    let client = k8s::build_client().await?;
    k8s::get_running_pod(&client, agent).await
}

pub async fn list(agent: &str) -> Result<()> {
    let pod_name = running_pod(agent).await?;
    let configs  = read_config(&pod_name).await?;
    if configs.is_empty() {
        println!("No MCP servers configured in '{agent}'.");
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
    agent: &str,
    name: &str,
    command: &str,
    args: &[String],
    env_pairs: &[String],
) -> Result<()> {
    let pod_name = running_pod(agent).await?;
    let mut configs = read_config(&pod_name).await?;

    if configs.iter().any(|c| c.name == name) {
        anyhow::bail!("MCP server '{name}' already exists in '{agent}'");
    }

    let mut env = HashMap::new();
    for pair in env_pairs {
        let (k, v) = pair.split_once('=')
            .with_context(|| format!("invalid env pair '{pair}': expected KEY=VALUE"))?;
        env.insert(k.to_string(), expand_host_var(v));
    }

    configs.push(McpServerConfig {
        name:    name.to_string(),
        command: command.to_string(),
        args:    args.to_vec(),
        env,
        url:     None,
        headers: HashMap::new(),
    });

    println!("→ writing config to '{agent}'");
    write_config(&pod_name, &configs).await?;

    let connected_marker  = format!("[mcp] '{name}' connected");
    let spawn_fail_marker = format!("[mcp] failed to spawn '{name}'");
    let init_fail_marker  = format!("[mcp] '{name}' initialize failed");
    let no_tools_marker   = format!("[mcp] warning: server '{name}' advertised no tools");

    println!("→ waiting for MCP server to connect (up to 60s)...");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let logs = loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let logs = tokio::process::Command::new("kubectl")
            .args(["logs", "-n", k8s::NAMESPACE, &format!("deployment/{agent}"), "--since=75s"])
            .output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let done = logs.contains(&connected_marker)
            || logs.contains(&no_tools_marker)
            || logs.contains(&spawn_fail_marker)
            || logs.contains(&init_fail_marker);
        if done || tokio::time::Instant::now() >= deadline {
            break logs;
        }
    };

    // Print any [mcp] lines relevant to this server so the user can see what happened.
    for line in logs.lines() {
        if line.contains("[mcp]") && (line.contains(&format!("'{name}'")) || line.contains("hot-reload")) {
            println!("  {line}");
        }
    }

    let success = logs.contains(&connected_marker) || logs.contains(&no_tools_marker);

    if !success {
        configs.retain(|c| c.name != name);
        let current_pod_name = running_pod(agent).await.unwrap_or(pod_name);
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
        anyhow::bail!("MCP server '{name}' did not confirm connection within timeout — entry not saved. Run `octo logs {agent}` to investigate.");
    }

    Ok(())
}

pub async fn import_from_file(agent: &str, path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read '{}'", path.display()))?;
    let entries: Vec<McpServerConfig> = serde_json::from_str(&text)
        .context("failed to parse JSON — expected an array of MCP server objects")?;

    if entries.is_empty() {
        println!("No entries found in '{}'.", path.display());
        return Ok(());
    }

    let pod_name = running_pod(agent).await?;

    // Expand ${VAR} references from the host environment before writing to the agent.
    let resolved: Vec<McpServerConfig> = entries.into_iter().map(|mut e| {
        e.env     = e.env    .into_iter().map(|(k, v)| (k, expand_host_var(&v))).collect();
        e.headers = e.headers.into_iter().map(|(k, v)| (k, expand_host_var(&v))).collect();
        e
    }).collect();

    // Replace the entire config with the contents of the file.
    println!("Importing {} MCP server(s) into '{agent}' (replacing existing config)...", resolved.len());
    write_config(&pod_name, &resolved).await?;
    println!("✓ imported successfully.");
    Ok(())
}

pub async fn remove(agent: &str, name: &str) -> Result<()> {
    let pod_name = running_pod(agent).await?;
    let mut configs = read_config(&pod_name).await?;
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        anyhow::bail!("MCP server '{name}' not found in '{agent}'");
    }
    write_config(&pod_name, &configs).await?;
    println!("Removed MCP server '{name}' from '{agent}'.");
    Ok(())
}
