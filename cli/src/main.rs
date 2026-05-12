mod agents;
mod dockerd;
mod init;
mod mcp;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use octo_core::Config;

/// Mask all but the first 4 and last 4 chars of a secret string.
fn mask(s: &str) -> String {
    if s.len() <= 8 { return "*".repeat(s.len()); }
    format!("{}...{}", &s[..4], &s[s.len()-4..])
}

/// Validate the effective config that will be written to the lair env file.
fn validate_resolved_config(
    api_key:        &str,
    openai_api_key: Option<&str>,
    api_url:        Option<&str>,
    model:          Option<&str>,
) -> Result<(), String> {
    let api_key        = api_key.trim();
    let openai_api_key = openai_api_key.map(str::trim).filter(|s| !s.is_empty());
    let api_url        = api_url       .map(str::trim).filter(|s| !s.is_empty());
    let model          = model         .map(str::trim).filter(|s| !s.is_empty());

    match api_url {
        Some(url) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!("api_url must start with http:// or https:// (got: {url})"));
            }
            if openai_api_key.is_none() && api_key.is_empty() {
                return Err(
                    "OpenAI-compatible setup requires openai_api_key (or anthropic_api_key as a fallback) for the bearer token".into()
                );
            }
            if model.is_none() {
                return Err(
                    "OpenAI-compatible setup requires model to be set — the default 'claude-sonnet-4-6' won't work against OpenAI endpoints".into()
                );
            }
        }
        None => {
            if api_key.is_empty() {
                return Err(
                    "anthropic_api_key is required for the Anthropic backend (or set api_url for an OpenAI-compatible endpoint)".into()
                );
            }
        }
    }
    Ok(())
}

#[derive(Parser)]
#[command(name = "octo", about = "octo lair management CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap lair as a Docker container on this host
    Init {
        /// Anthropic API key
        #[arg(long, env = "ANTHROPIC_API_KEY")]
        anthropic_api_key: Option<String>,

        /// GitHub token (optional, for private repos)
        #[arg(long, env = "GH_TOKEN")]
        gh_token: Option<String>,

        /// Host port that publishes lair's Noise endpoint
        #[arg(long, default_value_t = 8443)]
        noise_port: u16,

        /// Host port that publishes lair's HTTP endpoint (127.0.0.1 only)
        #[arg(long, default_value_t = 8000)]
        http_port: u16,

        /// Container image tag for lair
        #[arg(long, default_value = dockerd::LAIR_DEFAULT_IMAGE)]
        image: String,

        /// Path to an mcp.json file to seed lair's MCP tool list
        #[arg(long)]
        mcp_config: Option<std::path::PathBuf>,

        /// Path to a config.json file (sets model, api_url, anthropic_api_key)
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },

    /// Manage child agents
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },

    /// Remove the lair container, every managed agent container, and all of
    /// lair's bind-mounted data on the host (irreversible)
    Destroy {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Pull the latest image and restart lair (and optionally agents)
    Reload {
        /// Specific agent containers to also restart (by name)
        #[arg(long, value_name = "NAME", num_args = 1..)]
        containers: Vec<String>,
        /// Reload lair + every managed agent
        #[arg(long, conflicts_with = "containers")]
        all: bool,
    },

    /// Show logs for a container (all containers if no name given)
    Logs {
        /// Container name (e.g. lair, lair-foo). Omit for all.
        name: Option<String>,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },

    /// Print the CLI version
    Version,

    /// Update the octo CLI to the latest release
    Update,

    /// Remove the octo binary and shell completions from this machine
    Uninstall {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Generate shell tab-completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },

    /// Manage MCP tools in a container
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// View or update operator config (model, API key, endpoint)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show the current operator config
    Show,

    /// Update one or more config values (~/.octo/config.json + ~/.octo/lair-env)
    Set {
        /// Claude model to use (e.g. claude-sonnet-4-6)
        #[arg(long)]
        model: Option<String>,

        /// Full OpenAI-compatible chat-completions URL
        #[arg(long)]
        api_url: Option<String>,

        /// Anthropic API key
        #[arg(long)]
        anthropic_api_key: Option<String>,

        /// API key for the OpenAI-compatible provider set via --api-url
        #[arg(long)]
        openai_api_key: Option<String>,

        /// GitHub token
        #[arg(long)]
        gh_token: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// List all managed child agents
    List,

    /// Start a stopped agent
    Start { name: String },

    /// Stop a running agent
    Stop  { name: String },

    /// Delete an agent and both of its named volumes (irreversible)
    Delete {
        name: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// List MCP servers configured in an agent
    List {
        /// Agent name (default: lair)
        #[arg(long, default_value = "lair")]
        agent: String,
    },

    /// Add an MCP server to an agent
    Add {
        /// Agent name (default: lair)
        #[arg(long, default_value = "lair")]
        agent: String,

        /// Logical name for the MCP server
        #[arg(long)]
        name: String,

        /// Command to run (e.g. npx)
        #[arg(long)]
        command: String,

        /// Arguments for the command (pass after --)
        #[arg(last = true)]
        args: Vec<String>,

        /// Environment variables in KEY=VALUE format
        #[arg(long)]
        env: Vec<String>,
    },

    /// Remove an MCP server from an agent
    Remove {
        /// Agent name (default: lair)
        #[arg(long, default_value = "lair")]
        agent: String,

        /// Name of the MCP server to remove
        name: String,
    },

    /// Add multiple MCP servers from a JSON file
    Import {
        /// Agent name (default: lair)
        #[arg(long, default_value = "lair")]
        agent: String,

        /// Path to a JSON file containing an array of MCP server objects
        file: std::path::PathBuf,
    },
}

fn remove_completions() {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };

    let files = [
        home.join(".local/share/bash-completion/completions/octo"),
        home.join(".zfunc/_octo"),
        home.join(".config/fish/completions/octo.fish"),
    ];
    for path in &files {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    let bashrc = home.join(".bashrc");
    if let Ok(content) = std::fs::read_to_string(&bashrc) {
        let cleaned = content
            .lines()
            .filter(|l| !l.contains("octo"))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = if content.ends_with('\n') { cleaned + "\n" } else { cleaned };
        let _ = std::fs::write(&bashrc, cleaned);
    }
}

async fn update() -> Result<()> {
    use std::env::consts::{ARCH, OS};
    use tokio::process::Command;

    let artifact = match (OS, ARCH) {
        ("linux",  "x86_64")  => "octo-linux-x86_64",
        ("linux",  "aarch64") => "octo-linux-aarch64",
        ("macos",  "x86_64")  => "octo-macos-x86_64",
        ("macos",  "aarch64") => "octo-macos-aarch64",
        _ => anyhow::bail!("unsupported platform: {OS}/{ARCH}"),
    };

    let api_output = Command::new("curl")
        .args(["-fsSL", "https://api.github.com/repos/georgebradford0/octo/releases/latest"])
        .output()
        .await?;
    anyhow::ensure!(api_output.status.success(), "failed to fetch release info");
    let api_json: serde_json::Value = serde_json::from_slice(&api_output.stdout)?;
    let latest_tag = api_json["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("unexpected release API response"))?;
    let latest_version = latest_tag.trim_start_matches('v');

    let current_version = env!("CARGO_PKG_VERSION");
    if latest_version == current_version {
        println!("Already up to date (v{current_version}).");
        return Ok(());
    }

    let url = format!("https://github.com/georgebradford0/octo/releases/latest/download/{artifact}");

    println!("Downloading {artifact}...");
    let status = Command::new("curl")
        .args(["-fsSL", &url, "-o", "/tmp/octo-update"])
        .status()
        .await?;
    anyhow::ensure!(status.success(), "download failed");

    let current = std::env::current_exe()?;
    let dest = current.to_str().unwrap_or("/usr/local/bin/octo");

    Command::new("chmod").args(["+x", "/tmp/octo-update"]).status().await?;

    let mv = Command::new("mv")
        .args(["/tmp/octo-update", dest])
        .status()
        .await?;
    if !mv.success() {
        let status = Command::new("sudo")
            .args(["mv", "/tmp/octo-update", dest])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "failed to install updated binary");
    }

    println!("Updated: v{current_version} → v{latest_version}");
    Ok(())
}

async fn uninstall(yes: bool) -> Result<()> {
    let current = std::env::current_exe()?;

    if !yes {
        use std::io::Write;
        print!("Remove {}? [y/N] ", current.display());
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return Ok(());
        }
    }

    remove_completions();

    let path = current.to_str().unwrap_or("");
    let removed = std::fs::remove_file(&current);
    if removed.is_err() {
        let status = tokio::process::Command::new("sudo")
            .args(["rm", "-f", path])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "failed to remove {path}");
    }

    println!("Removed {}.", path);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { anthropic_api_key, gh_token, noise_port, http_port, image, mcp_config, config } => {
            let cfg: Config = init::load_config(config.as_deref())?;

            let api_key = anthropic_api_key
                .or(cfg.anthropic_api_key.clone())
                .ok_or_else(|| anyhow::anyhow!(
                    "Anthropic API key is required: pass --anthropic-api-key, set ANTHROPIC_API_KEY, or include anthropic_api_key in --config or ~/.octo/config.json"
                ))?;

            let gh_token = gh_token.or(cfg.gh_token.clone());

            if let Err(e) = validate_resolved_config(
                &api_key,
                cfg.openai_api_key.as_deref(),
                cfg.api_url.as_deref(),
                cfg.model.as_deref(),
            ) {
                eprintln!("error: invalid config: {e}");
                std::process::exit(1);
            }

            init::run(init::InitOptions {
                api_key:        &api_key,
                gh_token:       gh_token.as_deref(),
                noise_port,
                http_port,
                image:          &image,
                mcp_config:     mcp_config.as_deref(),
                model:          cfg.model.as_deref(),
                api_url:        cfg.api_url.as_deref(),
                openai_api_key: cfg.openai_api_key.as_deref(),
            }).await?;
        }
        Command::Destroy { yes } => {
            if !yes {
                use std::io::Write;
                print!("This will remove lair, every managed agent container and named volume, and lair's host data dir. Type 'yes' to confirm: ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim() != "yes" {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let docker = dockerd::build_client()?;
            dockerd::ensure_docker_reachable(&docker).await?;

            // Tear down every managed agent container + volumes.
            for (name, _state) in dockerd::list_managed(&docker).await? {
                if name == dockerd::LAIR_CONTAINER_NAME { continue; }
                println!("Removing agent '{name}'...");
                let _ = dockerd::delete_agent_full(&docker, &name).await;
            }
            // Tear down lair itself.
            if let Err(e) = dockerd::remove_container_force(&docker, dockerd::LAIR_CONTAINER_NAME).await {
                eprintln!("warning: remove lair: {e:#}");
            }

            // Wipe host data dir. Files lair persisted from inside the container
            // (session/, agents.json, etc.) are owned by the container's root user
            // on the host, so a plain `std::fs::remove_dir_all` from the operator
            // shell would fail with EACCES. Delegate the actual unlinking to a
            // throwaway container that has the dir bind-mounted.
            let data_dir = dockerd::lair_data_dir();
            if data_dir.exists() {
                println!("Removing {}...", data_dir.display());
                if let Err(e) = dockerd::wipe_dir_via_container(
                    &docker,
                    dockerd::LAIR_DEFAULT_IMAGE,
                    &data_dir,
                ).await {
                    eprintln!("warning: wipe {} via container: {e:#}", data_dir.display());
                }
                if let Err(e) = std::fs::remove_dir(&data_dir) {
                    eprintln!("warning: remove empty {}: {e}", data_dir.display());
                }
            }
            // Wipe env file too.
            let env_file = dockerd::env_file_path();
            if env_file.exists() {
                let _ = std::fs::remove_file(&env_file);
            }
            remove_completions();
            println!("Done.");
        }
        Command::Agents { action } => {
            match action {
                AgentsAction::List => agents::list().await?,
                AgentsAction::Start  { name }      => {
                    let d = dockerd::build_client()?;
                    agents::start(&d, &name).await?;
                }
                AgentsAction::Stop   { name }      => {
                    let d = dockerd::build_client()?;
                    agents::stop(&d, &name).await?;
                }
                AgentsAction::Delete { name, yes } => {
                    let d = dockerd::build_client()?;
                    agents::delete(&d, &name, yes).await?;
                }
            }
        }
        Command::Reload { containers, all } => {
            let docker = dockerd::build_client()?;
            dockerd::ensure_docker_reachable(&docker).await?;

            // Pull the same image lair was started with (re-derive from cfg).
            let image = dockerd::LAIR_DEFAULT_IMAGE; // TODO: persist image tag at init time
            println!("Pulling {image}...");
            dockerd::pull_image(&docker, image).await?;

            // Always restart lair.
            println!("Restarting lair...");
            dockerd::restart_container(&docker, dockerd::LAIR_CONTAINER_NAME).await?;
            dockerd::wait_for_health(8000, std::time::Duration::from_secs(60)).await?;
            println!("lair ready.");

            // Optionally restart agents.
            let targets: Vec<String> = if all {
                dockerd::list_managed(&docker).await?
                    .into_iter()
                    .map(|(n, _)| n)
                    .filter(|n| n != dockerd::LAIR_CONTAINER_NAME)
                    .collect()
            } else {
                containers
            };
            for name in &targets {
                print!("  {name} ... ");
                use std::io::Write; std::io::stdout().flush().ok();
                if let Err(e) = dockerd::restart_container(&docker, name).await {
                    println!("error: {e:#}");
                } else {
                    println!("restarted.");
                }
            }
        }
        Command::Logs { name, follow } => {
            let docker = dockerd::build_client()?;
            dockerd::ensure_docker_reachable(&docker).await?;

            let names: Vec<String> = if let Some(n) = name {
                vec![n]
            } else {
                let mut list: Vec<String> = dockerd::list_managed(&docker).await?
                    .into_iter()
                    .map(|(n, _)| n)
                    .filter(|n| n != dockerd::LAIR_CONTAINER_NAME)
                    .collect();
                list.push(dockerd::LAIR_CONTAINER_NAME.to_string());
                list
            };

            let multi = names.len() > 1;
            for name in &names {
                if multi { println!("\n=== {name} ==="); }
                if let Err(e) = dockerd::stream_logs(&docker, name, follow && !multi).await {
                    eprintln!("[{name}] {e:#}");
                }
                if follow && multi { break; } // can't follow multiple at once
            }
        }
        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Command::Update => update().await?,
        Command::Uninstall { yes } => uninstall(yes).await?,
        Command::Completions { shell } => {
            generate(shell, &mut Cli::command(), "octo", &mut std::io::stdout());
        }
        Command::Mcp { action } => match action {
            McpAction::List   { agent }                       => mcp::list(&agent).await?,
            McpAction::Add    { agent, name, command, args, env } => {
                mcp::add(&agent, &name, &command, &args, &env).await?;
            }
            McpAction::Remove { agent, name }                 => mcp::remove(&agent, &name).await?,
            McpAction::Import { agent, file }                 => mcp::import_from_file(&agent, &file).await?,
        },
        Command::Config { action } => {
            match action {
                ConfigAction::Show => {
                    let cfg = octo_core::read_config();
                    println!("anthropic_api_key: {}", cfg.anthropic_api_key.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                    println!("openai_api_key:    {}", cfg.openai_api_key.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                    println!("gh_token:          {}", cfg.gh_token.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                    println!("model:             {}", cfg.model.as_deref().unwrap_or("(default)"));
                    println!("api_url:           {}", cfg.api_url.as_deref().unwrap_or("(Anthropic)"));
                }
                ConfigAction::Set { model, api_url, anthropic_api_key, openai_api_key, gh_token } => {
                    let mut cfg = octo_core::read_config();
                    if anthropic_api_key.is_some() { cfg.anthropic_api_key = anthropic_api_key; }
                    if openai_api_key.is_some()    { cfg.openai_api_key    = openai_api_key; }
                    if model.is_some()             { cfg.model             = model; }
                    if api_url.is_some()           { cfg.api_url           = api_url; }
                    if gh_token.is_some()          { cfg.gh_token          = gh_token; }
                    octo_core::write_config(&cfg);

                    // Rewrite the env file with the merged values so the next
                    // `octo reload` (or `docker restart lair`) picks them up.
                    if let Some(api_key) = cfg.anthropic_api_key.as_deref() {
                        // Reuse the existing NOISE_PRIVATE_KEY/PUBLIC_PORT from
                        // the current env file so we don't clobber lair's keypair.
                        let existing = std::fs::read_to_string(dockerd::env_file_path()).unwrap_or_default();
                        let noise_private_key = extract_env_value(&existing, "NOISE_PRIVATE_KEY")
                            .unwrap_or_default();
                        let public_port = extract_env_value(&existing, "PUBLIC_PORT")
                            .and_then(|v| v.parse::<u16>().ok())
                            .unwrap_or(8443);

                        let env_text = init::build_env_file(&init::EnvFileInput {
                            api_key,
                            gh_token:          cfg.gh_token.as_deref(),
                            model:             cfg.model.as_deref(),
                            api_url:           cfg.api_url.as_deref(),
                            openai_api_key:    cfg.openai_api_key.as_deref(),
                            noise_private_key: &noise_private_key,
                            public_port,
                        });
                        std::fs::write(dockerd::env_file_path(), &env_text)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let path = dockerd::env_file_path();
                            if let Ok(mut perms) = std::fs::metadata(&path).map(|m| m.permissions()) {
                                perms.set_mode(0o600);
                                let _ = std::fs::set_permissions(&path, perms);
                            }
                        }
                    }

                    println!("Config updated. Run `octo reload` to apply.");
                }
            }
        }
    }
    Ok(())
}

/// Pull a single `KEY=VALUE` line out of an env-file body.
fn extract_env_value(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for line in body.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}
