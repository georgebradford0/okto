mod containers;
mod init;
mod mcp;

use octo_k8s_ops;

use anyhow::Result;
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
        api_key: String,

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
    },

    /// Manage child containers
    Containers {
        #[command(subcommand)]
        action: ContainersAction,
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
enum ContainersAction {
    /// List all managed child containers
    List,

    /// Create a new child container
    Create {
        /// Git repository URL
        #[arg(long)]
        git_url: Option<String>,

        /// Container name (auto-derived from repo if omitted)
        #[arg(long)]
        name: Option<String>,

        /// NodePort to assign (auto-assigned if omitted)
        #[arg(long)]
        noise_port: Option<u16>,
    },

    /// Scale a stopped container up to 1 replica
    Start {
        name: String,
    },

    /// Scale a running container down to 0 replicas
    Stop {
        name: String,
    },

    /// Delete a container and all its data (irreversible)
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
        Command::Init { api_key, gh_token, noise_port, public_port, mcp_config } => {
            init::run(&api_key, gh_token.as_deref(), noise_port, public_port, mcp_config.as_deref()).await?;
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
        Command::Containers { action } => match action {
            ContainersAction::List => containers::list().await?,
            ContainersAction::Create { git_url, name, noise_port } => {
                containers::create(git_url.as_deref(), name.as_deref(), noise_port).await?;
            }
            ContainersAction::Start { name } => containers::start(&name).await?,
            ContainersAction::Stop  { name } => containers::stop(&name).await?,
            ContainersAction::Delete { name, yes } => containers::delete(&name, yes).await?,
        },
        Command::Reload { containers, all } => {
            use octo_k8s_ops::k8s;
            let client = k8s::build_client().await?;
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
                println!("Restarted: {}", updated.join(", "));
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
    }
    Ok(())
}
