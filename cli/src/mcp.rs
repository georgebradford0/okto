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

    write_config(&pod, &configs).await?;
    println!("Added MCP server '{name}' to '{container}'.");
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
