//! `octo mcp …` — manage the per-container `mcp.json`.
//!
//! For lair: write directly to `<lair_data_dir>/mcp.json` (bind-mounted into
//! the container, hot-reloaded by lair's MCP poller).
//!
//! For an agent: child volumes are Docker-named (not bind-mounted), so
//! manipulate the file via `docker cp` against the running container.

use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, Result};
use bollard::Docker;
use serde::{Deserialize, Serialize};

use crate::dockerd;

const MCP_PATH_IN_CONTAINER: &str = "/data/mcp.json";
const LAIR_AGENT_NAME:       &str = "lair";

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

fn lair_mcp_path() -> PathBuf {
    dockerd::lair_data_dir().join("mcp.json")
}

async fn read_mcp(d: &Docker, agent: &str) -> Result<Vec<McpServerConfig>> {
    let text = if agent == LAIR_AGENT_NAME {
        match std::fs::read_to_string(lair_mcp_path()) {
            Ok(t) if !t.trim().is_empty() => t,
            _ => return Ok(Vec::new()),
        }
    } else {
        // `docker cp <agent>:/data/mcp.json -` streams the file as a tar.
        // Simpler: docker exec cat.
        use bollard::exec::{CreateExecOptions, StartExecResults};
        use futures_util::StreamExt;
        let exec = d
            .create_exec(
                agent,
                CreateExecOptions {
                    cmd: Some(vec!["cat", MCP_PATH_IN_CONTAINER]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("create exec in {agent}"))?;
        let mut out = String::new();
        if let StartExecResults::Attached { mut output, .. } =
            d.start_exec(&exec.id, None).await.with_context(|| format!("start exec in {agent}"))?
        {
            while let Some(item) = output.next().await {
                if let Ok(msg) = item {
                    out.push_str(&format!("{msg}"));
                }
            }
        }
        if out.trim().is_empty() { return Ok(Vec::new()); }
        out
    };
    serde_json::from_str(&text).context("parse mcp.json")
}

async fn write_mcp(d: &Docker, agent: &str, configs: &[McpServerConfig]) -> Result<()> {
    let json = serde_json::to_string_pretty(configs)?;
    if agent == LAIR_AGENT_NAME {
        let path = lair_mcp_path();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        // mode 0600 — env / header values are resolved literals and contain
        // secret material (API keys, bearer tokens).
        crate::init::write_secret_file(&path, &json)?;
    } else {
        use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
        use bollard::container::DownloadFromContainerOptions;
        let _ = d.download_from_container(agent, None::<DownloadFromContainerOptions<String>>);

        // Pipe the JSON in via `sh -c "cat > /data/mcp.json"`.
        let exec = d
            .create_exec(
                agent,
                CreateExecOptions {
                    cmd: Some(vec!["sh", "-c", &format!("cat > {MCP_PATH_IN_CONTAINER}")]),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("create exec in {agent}"))?;
        let started = d
            .start_exec(
                &exec.id,
                Some(StartExecOptions { detach: false, ..Default::default() }),
            )
            .await
            .with_context(|| format!("start exec in {agent}"))?;
        if let StartExecResults::Attached { mut input, .. } = started {
            use tokio::io::AsyncWriteExt;
            input.write_all(json.as_bytes()).await.context("write mcp.json via stdin")?;
            input.shutdown().await.ok();
        }
    }
    Ok(())
}

pub async fn list(agent: &str) -> Result<()> {
    let docker = dockerd::build_client()?;
    let configs = read_mcp(&docker, agent).await?;
    if configs.is_empty() {
        println!("No MCP servers configured in '{agent}'.");
        return Ok(());
    }
    for c in &configs {
        let args = if c.args.is_empty() { String::new() } else { format!(" {}", c.args.join(" ")) };
        println!("{}: {}{}", c.name, c.command, args);
        for k in c.env.keys() {
            println!("    {k}");
        }
    }
    Ok(())
}

pub async fn add(
    agent: &str,
    name:  &str,
    command: &str,
    args: &[String],
    env_pairs: &[String],
) -> Result<()> {
    let docker = dockerd::build_client()?;
    let mut configs = read_mcp(&docker, agent).await?;

    if configs.iter().any(|c| c.name == name) {
        anyhow::bail!("MCP server '{name}' already exists in '{agent}'");
    }

    // Expand `${VAR}` against the operator's shell env at write time and
    // bake the value in. Lair can't see the host env, so deferring
    // resolution would mean the MCP server gets a literal "${VAR}".
    let mut env = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    for pair in env_pairs {
        let (k, v) = pair.split_once('=')
            .with_context(|| format!("invalid env pair '{pair}': expected KEY=VALUE"))?;
        match crate::init::expand_host_env(v) {
            Ok(resolved) => { env.insert(k.to_string(), resolved); }
            Err(var)     => missing.push(var),
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        anyhow::bail!(
            "env var(s) not set in this shell: {}. Export them and re-run, or pass literal values.",
            missing.join(", "),
        );
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
    write_mcp(&docker, agent, &configs).await?;

    let connected_marker  = format!("[mcp] '{name}' connected");
    let spawn_fail_marker = format!("[mcp] failed to spawn '{name}'");
    let init_fail_marker  = format!("[mcp] '{name}' initialize failed");
    let no_tools_marker   = format!("[mcp] warning: server '{name}' advertised no tools");

    println!("→ waiting for MCP server to connect (up to 60s)...");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let logs = loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let logs = dockerd::logs_since(&docker, agent, 75).await.unwrap_or_default();
        let done = logs.contains(&connected_marker)
            || logs.contains(&no_tools_marker)
            || logs.contains(&spawn_fail_marker)
            || logs.contains(&init_fail_marker);
        if done || tokio::time::Instant::now() >= deadline {
            break logs;
        }
    };

    for line in logs.lines() {
        if line.contains("[mcp]") && (line.contains(&format!("'{name}'")) || line.contains("hot-reload")) {
            println!("  {line}");
        }
    }

    let success = logs.contains(&connected_marker) || logs.contains(&no_tools_marker);

    if !success {
        configs.retain(|c| c.name != name);
        write_mcp(&docker, agent, &configs).await?;
    }

    if logs.contains(&connected_marker) {
        println!("MCP server '{name}' connected successfully.");
    } else if logs.contains(&no_tools_marker) {
        println!("MCP server '{name}' connected but advertised no tools.");
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
        .with_context(|| format!("read {}", path.display()))?;
    let entries: Vec<McpServerConfig> = serde_json::from_str(&text)
        .context("parse JSON — expected an array of MCP server objects")?;
    if entries.is_empty() {
        println!("No entries found in '{}'.", path.display());
        return Ok(());
    }

    let docker = dockerd::build_client()?;
    // Expand `${VAR}` against the operator's shell env at write time. Any
    // ref that doesn't resolve aborts the import with all missing vars
    // listed at once.
    let mut missing: Vec<String> = Vec::new();
    let resolved: Vec<McpServerConfig> = entries.into_iter().map(|mut e| {
        let expand_map = |m: HashMap<String, String>, missing: &mut Vec<String>| -> HashMap<String, String> {
            m.into_iter().filter_map(|(k, v)| {
                match crate::init::expand_host_env(&v) {
                    Ok(resolved) => Some((k, resolved)),
                    Err(var)     => { missing.push(var); None }
                }
            }).collect()
        };
        e.env     = expand_map(e.env,     &mut missing);
        e.headers = expand_map(e.headers, &mut missing);
        e
    }).collect();

    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        anyhow::bail!(
            "env var(s) not set in this shell: {}. Export them and re-run, or inline the values in '{}'.",
            missing.join(", "),
            path.display(),
        );
    }

    println!("Importing {} MCP server(s) into '{agent}' (replacing existing config)...", resolved.len());
    write_mcp(&docker, agent, &resolved).await?;
    println!("Imported successfully.");
    Ok(())
}

pub async fn remove(agent: &str, name: &str) -> Result<()> {
    let docker = dockerd::build_client()?;
    let mut configs = read_mcp(&docker, agent).await?;
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        anyhow::bail!("MCP server '{name}' not found in '{agent}'");
    }
    write_mcp(&docker, agent, &configs).await?;
    println!("Removed MCP server '{name}' from '{agent}'.");
    Ok(())
}
