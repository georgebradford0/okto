//! Local Docker daemon ops the `octo` CLI needs. Mirrors the operations lair's
//! own `docker.rs` does for child agents, scoped here to the lair container
//! itself plus a few read/write helpers against managed agent containers.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bollard::{
    container::{
        Config as ContainerConfig, CreateContainerOptions, ListContainersOptions,
        LogOutput, LogsOptions, RemoveContainerOptions, StartContainerOptions,
        StopContainerOptions, WaitContainerOptions,
    },
    image::CreateImageOptions,
    secret::{HostConfig, Mount, MountTypeEnum, PortBinding, RestartPolicy, RestartPolicyNameEnum},
    Docker,
};
use futures_util::stream::StreamExt;

pub const LAIR_CONTAINER_NAME:    &str = "lair";
pub const LAIR_DEFAULT_IMAGE:     &str = "ghcr.io/georgebradford0/lair:latest";
pub const LAIR_MANAGED_LABEL:     &str = "octo.managed";
pub const LAIR_MANAGED_LABEL_VAL: &str = "1";

/// `<lair_data_dir>` on the host — bind-mounted to `/data` inside the lair
/// container. Lair writes its keypairs, agents.json, mcp.json, and chat
/// history here, and the CLI reads them directly without `docker exec`.
pub fn lair_data_dir() -> PathBuf {
    std::env::var("OCTO_LAIR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".octo").join("lair"))
}

/// Operator's config dir (separate from `lair_data_dir`).
pub fn config_dir() -> PathBuf {
    home_dir().join(".octo")
}

/// Path of the `docker --env-file` consumed by the lair container.
pub fn env_file_path() -> PathBuf {
    config_dir().join("lair-env")
}

/// Path of the launch-config record used to recreate lair on `octo reload` and
/// `octo env set/unset` without re-prompting the operator for ports/image.
pub fn launch_config_path() -> PathBuf {
    config_dir().join("lair-launch.json")
}

/// What `octo init` decided about how to launch the lair container. Persisted
/// so subsequent commands that need to *recreate* (not just restart) lair —
/// because they changed something only `docker run` consumes, like
/// `--env-file` — don't have to ask the operator for ports/image again.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct LaunchRecord {
    pub image:           String,
    pub host_noise_port: u16,
    pub host_http_port:  u16,
}

pub fn write_launch(rec: &LaunchRecord) -> Result<()> {
    let path = launch_config_path();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let body = serde_json::to_string_pretty(rec).context("encode lair-launch.json")?;
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn read_launch() -> Option<LaunchRecord> {
    std::fs::read_to_string(launch_config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn build_client() -> Result<Docker> {
    Docker::connect_with_local_defaults()
        .context("connect to local Docker daemon — is Docker running?")
}

/// `docker info` style ping. Returns `Err` with a helpful message if the
/// daemon isn't reachable so `octo init` can fail loudly.
pub async fn ensure_docker_reachable(d: &Docker) -> Result<()> {
    d.ping().await.context(
        "Docker daemon is not reachable. Install Docker (https://docs.docker.com/get-docker/) \
         and make sure the daemon is running, or set DOCKER_HOST to a reachable endpoint.",
    )?;
    Ok(())
}

pub async fn pull_image(d: &Docker, image: &str) -> Result<()> {
    let opts = CreateImageOptions {
        from_image: image.to_string(),
        ..Default::default()
    };
    let mut stream = d.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        match item {
            Ok(_)  => {}
            Err(e) => return Err(e).with_context(|| format!("docker pull {image}")),
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct LairLaunch<'a> {
    pub image:           &'a str,
    pub host_noise_port: u16,
    pub host_http_port:  u16,
    pub data_dir:        &'a Path,
    pub env_file:        &'a Path,
    pub docker_socket:   &'a str,
    /// Host path to the operator's `config.json` (API keys, model,
    /// …). Bind-mounted read-only into the lair container at
    /// `/data/config.json` so lair reads secrets from the file rather than
    /// `--env`/`--env-file` — keeps them out of `docker inspect lair`.
    pub operator_config: &'a Path,
}

/// Create + start the lair container. Removes any pre-existing container with
/// the same name first.
pub async fn start_lair(d: &Docker, launch: &LairLaunch<'_>) -> Result<String> {
    let _ = d
        .remove_container(
            LAIR_CONTAINER_NAME,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await; // ignore "no such container"

    let env_text = std::fs::read_to_string(launch.env_file)
        .with_context(|| format!("read env file {}", launch.env_file.display()))?;
    let env: Vec<String> = env_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();

    let port_bindings = HashMap::from([
        (
            "9000/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip:   Some("0.0.0.0".to_string()),
                host_port: Some(launch.host_noise_port.to_string()),
            }]),
        ),
        (
            "8000/tcp".to_string(),
            Some(vec![PortBinding {
                // CLI-side `wait_for_health` uses this; bind to loopback so
                // it isn't exposed publicly.
                host_ip:   Some("127.0.0.1".to_string()),
                host_port: Some(launch.host_http_port.to_string()),
            }]),
        ),
    ]);
    let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
    exposed_ports.insert("9000/tcp".to_string(), HashMap::new());
    exposed_ports.insert("8000/tcp".to_string(), HashMap::new());

    let host_config = HostConfig {
        port_bindings: Some(port_bindings),
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        mounts: Some(vec![
            Mount {
                target:   Some("/var/run/docker.sock".to_string()),
                source:   Some(launch.docker_socket.to_string()),
                typ:      Some(MountTypeEnum::BIND),
                ..Default::default()
            },
            Mount {
                target:   Some("/data".to_string()),
                source:   Some(launch.data_dir.to_string_lossy().to_string()),
                typ:      Some(MountTypeEnum::BIND),
                ..Default::default()
            },
            // Overlay the operator's config.json into /data/config.json. Bind
            // mounts of single files preserve the host inode, so `octo config
            // set ...` (which truncates-in-place) is picked up live without a
            // lair restart.
            Mount {
                target:   Some("/data/config.json".to_string()),
                source:   Some(launch.operator_config.to_string_lossy().to_string()),
                typ:      Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };

    let labels = HashMap::from([
        (LAIR_MANAGED_LABEL.to_string(), LAIR_MANAGED_LABEL_VAL.to_string()),
        ("octo.role".to_string(),        "lair".to_string()),
    ]);

    let config = ContainerConfig::<String> {
        image: Some(launch.image.to_string()),
        env:   Some(env),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        host_config: Some(host_config),
        ..Default::default()
    };

    let created = d
        .create_container(
            Some(CreateContainerOptions {
                name:     LAIR_CONTAINER_NAME.to_string(),
                platform: None,
            }),
            config,
        )
        .await
        .context("docker create_container lair")?;

    d.start_container(LAIR_CONTAINER_NAME, None::<StartContainerOptions<String>>)
        .await
        .context("docker start_container lair")?;

    Ok(created.id)
}

pub async fn restart_container(d: &Docker, name: &str) -> Result<()> {
    if let Err(e) = d
        .stop_container(name, Some(StopContainerOptions { t: 10 }))
        .await
    {
        tracing::warn!("[dockerd] stop {name}: {e}");
    }
    d.start_container(name, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("docker start_container {name}"))?;
    Ok(())
}

pub async fn remove_container_force(d: &Docker, name: &str) -> Result<()> {
    d.remove_container(
        name,
        Some(RemoveContainerOptions {
            force: true,
            ..Default::default()
        }),
    )
    .await
    .with_context(|| format!("docker remove_container {name}"))?;
    Ok(())
}

/// Wait for `http://127.0.0.1:<port>/health` to return 200, up to `timeout`.
pub async fn wait_for_health(port: u16, timeout: std::time::Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("lair did not become ready within {:?}", timeout);
        }
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    }
}

/// Fetch the public IP via `https://api.ipify.org`. Same approach lair uses
/// internally — keeps the QR code's IP and lair's view consistent.
pub async fn detect_public_ip() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get("https://api.ipify.org")
        .send()
        .await
        .context("detect public IP via api.ipify.org")?;
    let body = resp.text().await.context("read ipify body")?;
    Ok(body.trim().to_string())
}

/// Pull a fixed window of logs from a managed container. Used by `octo mcp add`
/// to wait for the `[mcp] '...' connected` marker without holding a stream
/// open.
pub async fn logs_since(d: &Docker, name: &str, since_secs_ago: i64) -> Result<String> {
    let opts = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        follow: false,
        since:  since_secs_ago,
        tail:   "all".to_string(),
        ..Default::default()
    };
    let mut stream = d.logs(name, Some(opts));
    let mut out = String::new();
    while let Some(line) = stream.next().await {
        match line {
            Ok(LogOutput::StdOut { message })
            | Ok(LogOutput::StdErr { message })
            | Ok(LogOutput::Console { message }) => {
                out.push_str(&String::from_utf8_lossy(&message));
            }
            Ok(LogOutput::StdIn { .. }) => {}
            Err(e) => return Err(e).with_context(|| format!("docker logs {name}")),
        }
    }
    Ok(out)
}

/// Stream a container's logs to stdout. Used by `octo logs`.
pub async fn stream_logs(d: &Docker, name: &str, follow: bool) -> Result<()> {
    let opts = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        follow,
        tail:   "1000".to_string(),
        ..Default::default()
    };
    let mut stream = d.logs(name, Some(opts));
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    while let Some(line) = stream.next().await {
        match line {
            Ok(LogOutput::StdOut { message })
            | Ok(LogOutput::StdErr { message })
            | Ok(LogOutput::Console { message }) => {
                handle.write_all(&message).ok();
            }
            Ok(LogOutput::StdIn { .. }) => {}
            Err(e) => {
                eprintln!("[{name}] log stream error: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// All containers labelled `octo.managed=1`, including stopped ones.
pub async fn list_managed(d: &Docker) -> Result<Vec<(String, String)>> {
    let mut filters: HashMap<String, Vec<String>> = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{LAIR_MANAGED_LABEL}={LAIR_MANAGED_LABEL_VAL}")],
    );
    let containers = d
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        }))
        .await
        .context("docker list_containers")?;
    Ok(containers
        .into_iter()
        .filter_map(|c| {
            let name = c.names
                .and_then(|ns| ns.into_iter().next())
                .map(|n| n.trim_start_matches('/').to_string())?;
            let state = c.state.unwrap_or_default();
            Some((name, state))
        })
        .collect())
}

pub async fn start_named(d: &Docker, name: &str) -> Result<()> {
    d.start_container(name, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("docker start_container {name}"))?;
    Ok(())
}

pub async fn stop_named(d: &Docker, name: &str) -> Result<()> {
    d.stop_container(name, Some(StopContainerOptions { t: 10 }))
        .await
        .with_context(|| format!("docker stop_container {name}"))?;
    Ok(())
}

/// Empty the contents of a host directory by running a throwaway container
/// that has the dir bind-mounted. Lair writes session/agents/etc. into its
/// bind-mounted `/data` as the container's root user, so those files end up
/// owned by host root and the operator's user can't unlink them directly. The
/// container shares the same uid (root) and *can*, so we delegate.
///
/// Leaves `host_dir` itself in place but empty; callers can `remove_dir` it
/// afterwards (which doesn't need root, since the dir itself was created by
/// the host operator).
pub async fn wipe_dir_via_container(d: &Docker, image: &str, host_dir: &Path) -> Result<()> {
    let host_config = HostConfig {
        mounts: Some(vec![Mount {
            target: Some("/wipe".to_string()),
            source: Some(host_dir.to_string_lossy().to_string()),
            typ:    Some(MountTypeEnum::BIND),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let config = ContainerConfig::<String> {
        image:      Some(image.to_string()),
        entrypoint: Some(vec!["/bin/sh".to_string()]),
        // `find -delete` implies -depth so it removes contents before parents,
        // and -mindepth 1 spares /wipe itself.
        cmd:        Some(vec!["-c".to_string(), "find /wipe -mindepth 1 -delete".to_string()]),
        host_config: Some(host_config),
        ..Default::default()
    };

    let created = d
        .create_container(None::<CreateContainerOptions<String>>, config)
        .await
        .context("docker create_container (wipe)")?;

    d.start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("docker start_container (wipe)")?;

    let mut waiter = d.wait_container(
        &created.id,
        Some(WaitContainerOptions { condition: "not-running".to_string() }),
    );
    while let Some(item) = waiter.next().await {
        if let Err(e) = item {
            tracing::warn!("[dockerd] wait wipe container: {e}");
            break;
        }
    }

    let _ = d
        .remove_container(
            &created.id,
            Some(RemoveContainerOptions { force: true, ..Default::default() }),
        )
        .await;

    Ok(())
}

/// Remove an agent container and both of its named volumes
/// (`agent-<name>-data`, `agent-<name>-workspace`).
pub async fn delete_agent_full(d: &Docker, name: &str) -> Result<()> {
    let _ = d
        .remove_container(
            name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    for suffix in ["data", "workspace"] {
        let vol = format!("agent-{name}-{suffix}");
        if let Err(e) = d.remove_volume(&vol, None).await {
            tracing::warn!("[dockerd] remove_volume {vol}: {e}");
        }
    }
    Ok(())
}
