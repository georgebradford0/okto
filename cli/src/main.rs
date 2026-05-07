mod containers;
mod init;
mod mcp;

use octo_k8s_ops;

use anyhow::Result;

/// Mask all but the first 4 and last 4 chars of a secret string.
fn mask(s: &str) -> String {
    if s.len() <= 8 { return "*".repeat(s.len()); }
    format!("{}...{}", &s[..4], &s[s.len()-4..])
}
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

#[derive(Parser)]
#[command(name = "octo", about = "octo cluster management CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap lair on a Kubernetes cluster
    Init {
        /// Anthropic API key
        #[arg(long, env = "ANTHROPIC_API_KEY")]
        api_key: Option<String>,

        /// GitHub token (optional, for private repos)
        #[arg(long, env = "GH_TOKEN")]
        gh_token: Option<String>,

        /// NodePort to expose lair's Noise endpoint (default: 30900)
        #[arg(long, default_value_t = 30900)]
        noise_port: u16,

        /// Port advertised in the QR code (default: 8443).
        /// A socat proxy is automatically configured to forward this port to
        /// the NodePort. Set to the same value as --noise-port to skip the proxy.
        #[arg(long, default_value_t = 8443)]
        public_port: u16,

        /// Path to an mcp.json file to seed lair's MCP tool list on first startup
        #[arg(long)]
        mcp_config: Option<std::path::PathBuf>,

        /// Path to a config.json file (sets model, base_url, api_key)
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },

    /// Manage child pods
    Pods {
        #[command(subcommand)]
        action: PodsAction,
    },

    /// Delete the entire octo namespace and all data (irreversible)
    Destroy {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Reload containers (default: lair only)
    Reload {
        /// Specific child containers to reload (by name)
        #[arg(long, value_name = "NAME", num_args = 1..)]
        containers: Vec<String>,
        /// Reload all containers (lair + all managed children)
        #[arg(long, conflicts_with = "containers")]
        all: bool,
    },

    /// Show logs for a container (all containers if no name given)
    Logs {
        /// Deployment name (e.g. lair, my-repo). Omit for all.
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

    /// Run kubectl get for octo resources
    Get {
        #[command(subcommand)]
        resource: GetResource,
    },

    /// View or update cluster config (model, API key, endpoint)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current lair-secrets config values
    Show,

    /// Update one or more config values in lair-secrets (and ~/.octo/config.json)
    Set {
        /// Claude model to use (e.g. claude-opus-4-5)
        #[arg(long)]
        model: Option<String>,

        /// OpenAI-compatible base URL (leave empty to use Anthropic)
        #[arg(long)]
        base_url: Option<String>,

        /// Anthropic API key
        #[arg(long)]
        api_key: Option<String>,

        /// GitHub token
        #[arg(long)]
        gh_token: Option<String>,
    },
}

#[derive(Subcommand)]
enum GetResource {
    /// Get pods
    Pods,
    /// Get deployments
    Deployments,
    /// Get services
    Services,
    /// Get persistent volume claims
    Pvc,
    /// Get secrets
    Secrets,
}

#[derive(Subcommand)]
enum PodsAction {
    /// List all managed child pods
    List,

    /// Create a new child pod
    Create {
        /// Git repository URL
        #[arg(long)]
        git_url: Option<String>,

        /// Pod name (auto-derived from repo if omitted)
        #[arg(long)]
        name: Option<String>,

        /// NodePort to assign (auto-assigned if omitted)
        #[arg(long)]
        noise_port: Option<u16>,
    },

    /// Scale a stopped pod up to 1 replica
    Start {
        name: String,
    },

    /// Scale a running pod down to 0 replicas
    Stop {
        name: String,
    },

    /// Delete a pod and all its data (irreversible)
    Delete {
        name: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// List MCP servers configured in a pod
    List {
        /// Pod name (default: lair)
        #[arg(long, default_value = "lair")]
        pod: String,
    },

    /// Add an MCP server to a pod
    Add {
        /// Pod name (default: lair)
        #[arg(long, default_value = "lair")]
        pod: String,

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

    /// Remove an MCP server from a pod
    Remove {
        /// Pod name (default: lair)
        #[arg(long, default_value = "lair")]
        pod: String,

        /// Name of the MCP server to remove
        name: String,
    },

    /// Add multiple MCP servers from a JSON file
    Import {
        /// Pod name (default: lair)
        #[arg(long, default_value = "lair")]
        pod: String,

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

    // Remove the `. .../octo` source line added to ~/.bashrc.
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

    // Fetch the latest release tag from GitHub API.
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

    let url = format!(
        "https://github.com/georgebradford0/octo/releases/latest/download/{artifact}"
    );

    println!("Downloading {artifact}...");
    let status = Command::new("curl")
        .args(["-fsSL", &url, "-o", "/tmp/octo-update"])
        .status()
        .await?;
    anyhow::ensure!(status.success(), "download failed");

    // Determine current binary path.
    let current = std::env::current_exe()?;
    let dest = current.to_str().unwrap_or("/usr/local/bin/octo");

    Command::new("chmod").args(["+x", "/tmp/octo-update"]).status().await?;

    // Try direct move, fall back to sudo.
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

    // Try direct removal, fall back to sudo.
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
        Command::Init { api_key, gh_token, noise_port, public_port, mcp_config, config } => {
            // Read config file if provided.
            let cfg_json: Option<serde_json::Value> = match &config {
                None => None,
                Some(path) => {
                    if !path.exists() {
                        eprintln!("error: config file not found: {}", path.display());
                        std::process::exit(1);
                    }
                    let text = std::fs::read_to_string(path)
                        .map_err(|e| anyhow::anyhow!("failed to read config file {}: {e}", path.display()))?;
                    Some(serde_json::from_str(&text)
                        .map_err(|e| anyhow::anyhow!("invalid JSON in config file {}: {e}", path.display()))?)
                }
            };

            // api_key: --api-key flag > config file > error
            let resolved_api_key = api_key
                .or_else(|| cfg_json.as_ref().and_then(|c| c["api_key"].as_str().map(str::to_string)))
                .ok_or_else(|| anyhow::anyhow!(
                    "API key is required: pass --api-key, set ANTHROPIC_API_KEY, or include api_key in --config"
                ))?;

            let model    = cfg_json.as_ref().and_then(|c| c["model"].as_str().map(str::to_string));
            let base_url = cfg_json.as_ref().and_then(|c| c["base_url"].as_str().map(str::to_string));

            init::run(
                &resolved_api_key,
                gh_token.as_deref(),
                noise_port,
                public_port,
                mcp_config.as_deref(),
                model.as_deref(),
                base_url.as_deref(),
            ).await?;
        }
        Command::Destroy { yes } => {
            if !yes {
                use std::io::Write;
                print!("This will delete the entire octo namespace and all PVC data. Type 'yes' to confirm: ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim() != "yes" {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            remove_completions();
            use std::io::Write;
            use std::time::Instant;
            use octo_k8s_ops::k8s;
            let client = k8s::build_client().await?;

            println!("Deleting namespace '{}'...", k8s::NAMESPACE);
            k8s::delete_namespace(&client).await?;
            println!("Waiting for all pods and PVCs to terminate...");
            let start = Instant::now();
            while k8s::namespace_exists(&client).await {
                print!("\r  Still terminating... {}s", start.elapsed().as_secs());
                std::io::stdout().flush()?;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            println!("\rNamespace removed.                              ");
            println!("Done. All resources removed.");
        }
        Command::Pods { action } => match action {
            PodsAction::List => containers::list().await?,
            PodsAction::Create { git_url, name, noise_port } => {
                containers::create(git_url.as_deref(), name.as_deref(), noise_port).await?;
            }
            PodsAction::Start { name } => containers::start(&name).await?,
            PodsAction::Stop  { name } => containers::stop(&name).await?,
            PodsAction::Delete { name, yes } => containers::delete(&name, yes).await?,
        },
        Command::Reload { containers, all } => {
            use octo_k8s_ops::k8s;
            let client = k8s::build_client().await?;

            // Sync ~/.octo/config.json into lair-secrets before restarting so
            // pods pick up the latest model/endpoint/api-key on the new image.
            match k8s::read_lair_secrets(&client).await {
                Ok(current) => {
                    let local = octo_k8s_ops::read_config();
                    let api_key  = local.api_key .unwrap_or(current.api_key);
                    let model    = local.model   .or(current.model);
                    let base_url = local.base_url.or(current.base_url);
                    k8s::upsert_secret(
                        &client,
                        &api_key,
                        current.gh_token.as_deref(),
                        &current.noise_private_key,
                        current.mcp_config_json.as_deref(),
                        model.as_deref(),
                        base_url.as_deref(),
                    ).await?;
                    println!("Config synced to lair-secrets.");
                }
                Err(e) => eprintln!("Warning: could not sync config ({e}); proceeding with reload."),
            }

            let updated = if all {
                k8s::update_and_restart_all(&client).await?
            } else if !containers.is_empty() {
                let names: Vec<&str> = containers.iter().map(|s| s.as_str()).collect();
                k8s::restart_deployments(&client, &names).await?
            } else {
                k8s::restart_deployments(&client, &["lair"]).await?
            };
            if updated.is_empty() {
                println!("Nothing restarted.");
            } else {
                println!("Restarting {} ...", updated.join(", "));
                for name in &updated {
                    let old_ver = k8s::get_deployment_version(&client, name).await
                        .unwrap_or_else(|| "unknown".to_string());
                    print!("  {name}: {old_ver} → ? ... ");
                    std::io::Write::flush(&mut std::io::stdout())?;
                    k8s::wait_for_deployment_ready(&client, name, 120).await?;
                    let new_ver = k8s::get_deployment_version(&client, name).await
                        .unwrap_or_else(|| "unknown".to_string());
                    println!("{new_ver} ready.");
                }
            }
        }
        Command::Logs { name, follow } => {
            use octo_k8s_ops::k8s;
            let client = k8s::build_client().await?;

            // Build list of deployment names to show logs for.
            let names: Vec<String> = if let Some(n) = name {
                vec![n]
            } else {
                let mut list = k8s::list_managed_deployments(&client).await?
                    .into_iter().map(|c| c.name).collect::<Vec<_>>();
                list.push("lair".to_string());
                list
            };

            let multi = names.len() > 1;
            for deployment in &names {
                let (pod_name, is_running) = match k8s::get_running_pod(&client, deployment).await {
                    Ok(p)  => (p, true),
                    Err(_) => match k8s::get_any_pod(&client, deployment).await {
                        Ok((p, phase)) => {
                            eprintln!("[{deployment}] pod is {phase} (not Running) — showing available logs:");
                            (p, false)
                        }
                        Err(e) => { eprintln!("[{deployment}] {e}"); continue; }
                    }
                };

                if multi { println!("\n=== {deployment} ==="); }

                let mut args = vec!["logs", "-n", k8s::NAMESPACE, &pod_name];
                if follow && is_running { args.push("-f"); }

                let status = tokio::process::Command::new("kubectl")
                    .args(&args)
                    .status().await?;
                if !status.success() {
                    eprintln!("[{deployment}] kubectl logs exited with {status}");
                }

                // Can't follow multiple pods simultaneously; warn and move on.
                if follow && multi { break; }
            }
        }
        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Command::Update => update().await?,
        Command::Uninstall { yes } => uninstall(yes).await?,
        Command::Completions { shell } => {
            generate(shell, &mut Cli::command(), "octo", &mut std::io::stdout());
        }
        Command::Get { resource } => {
            let kind = match resource {
                GetResource::Pods        => "pods",
                GetResource::Deployments => "deployments",
                GetResource::Services    => "services",
                GetResource::Pvc         => "pvc",
                GetResource::Secrets     => "secrets",
            };
            tokio::process::Command::new("kubectl")
                .args(["get", kind, "-n", octo_k8s_ops::k8s::NAMESPACE])
                .status()
                .await?;
        }
        Command::Mcp { action } => match action {
            McpAction::List { pod } => mcp::list(&pod).await?,
            McpAction::Add { pod, name, command, args, env } => {
                mcp::add(&pod, &name, &command, &args, &env).await?;
            }
            McpAction::Remove { pod, name } => mcp::remove(&pod, &name).await?,
            McpAction::Import { pod, file } => mcp::import_from_file(&pod, &file).await?,
        },
        Command::Config { action } => {
            use octo_k8s_ops::k8s;
            let client = k8s::build_client().await?;
            match action {
                ConfigAction::Show => {
                    let s = k8s::read_lair_secrets(&client).await?;
                    println!("api_key:  {}", mask(&s.api_key));
                    println!("model:    {}", s.model.as_deref().unwrap_or("(default)"));
                    println!("base_url: {}", s.base_url.as_deref().unwrap_or("(Anthropic)"));
                    println!("gh_token: {}", s.gh_token.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                }
                ConfigAction::Set { model, base_url, api_key, gh_token } => {
                    let current = k8s::read_lair_secrets(&client).await?;
                    let new_api_key  = api_key .unwrap_or(current.api_key);
                    let new_gh_token = gh_token.or(current.gh_token);
                    let new_model    = model   .or(current.model);
                    let new_base_url = base_url.or(current.base_url);

                    k8s::upsert_secret(
                        &client,
                        &new_api_key,
                        new_gh_token.as_deref(),
                        &current.noise_private_key,
                        current.mcp_config_json.as_deref(),
                        new_model.as_deref(),
                        new_base_url.as_deref(),
                    ).await?;

                    // Also persist to ~/.octo/config.json so reload picks it up.
                    let mut local = octo_k8s_ops::read_config();
                    local.api_key  = Some(new_api_key);
                    local.model    = new_model;
                    local.base_url = new_base_url;
                    octo_k8s_ops::write_config(&local);

                    println!("Config updated in lair-secrets and ~/.octo/config.json.");
                }
            }
        }
    }
    Ok(())
}
