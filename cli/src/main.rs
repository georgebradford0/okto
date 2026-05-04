mod containers;
mod init;
mod mcp;

use claudulhu_k8s_ops;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};

#[derive(Parser)]
#[command(name = "claudulhu", about = "claudulhu cluster management CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap rulyeh on a Kubernetes cluster
    Init {
        /// Anthropic API key
        #[arg(long, env = "ANTHROPIC_API_KEY")]
        api_key: String,

        /// GitHub token (optional, for private repos)
        #[arg(long, env = "GH_TOKEN")]
        gh_token: Option<String>,

        /// NodePort to expose rulyeh's Noise endpoint (default: 30900)
        #[arg(long, default_value_t = 30900)]
        noise_port: u16,

        /// Port advertised in the QR code (default: 8443).
        /// A socat proxy is automatically configured to forward this port to
        /// the NodePort. Set to the same value as --noise-port to skip the proxy.
        #[arg(long, default_value_t = 8443)]
        public_port: u16,

        /// Path to an mcp.json file to seed rulyeh's MCP tool list on first startup
        #[arg(long)]
        mcp_config: Option<std::path::PathBuf>,
    },

    /// Manage child containers
    Containers {
        #[command(subcommand)]
        action: ContainersAction,
    },

    /// Delete the entire claudulhu namespace and all data (irreversible)
    Destroy {
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Update image to latest and restart all pods (rulyeh + all child containers)
    Restart,

    /// Show logs for a container (all containers if no name given)
    Logs {
        /// Deployment name (e.g. rulyeh, my-repo). Omit for all.
        name: Option<String>,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },

    /// Print the CLI version
    Version,

    /// Update the claudulhu CLI to the latest release
    Update,

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

    /// Rollout-restart one or more containers (all managed containers if none specified)
    Restart {
        names: Vec<String>,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// List MCP servers configured in a container
    List {
        /// Container name (default: rulyeh)
        #[arg(long, default_value = "rulyeh")]
        container: String,
    },

    /// Add an MCP server to a container
    Add {
        /// Container name (default: rulyeh)
        #[arg(long, default_value = "rulyeh")]
        container: String,

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

    /// Remove an MCP server from a container
    Remove {
        /// Container name (default: rulyeh)
        #[arg(long, default_value = "rulyeh")]
        container: String,

        /// Name of the MCP server to remove
        name: String,
    },

    /// Add multiple MCP servers from a JSON file
    Import {
        /// Container name (default: rulyeh)
        #[arg(long, default_value = "rulyeh")]
        container: String,

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
        home.join(".local/share/bash-completion/completions/claudulhu"),
        home.join(".zfunc/_claudulhu"),
        home.join(".config/fish/completions/claudulhu.fish"),
    ];
    for path in &files {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Remove the `. .../claudulhu` source line added to ~/.bashrc.
    let bashrc = home.join(".bashrc");
    if let Ok(content) = std::fs::read_to_string(&bashrc) {
        let cleaned = content
            .lines()
            .filter(|l| !l.contains("claudulhu"))
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
        ("linux",  "x86_64")  => "claudulhu-linux-x86_64",
        ("linux",  "aarch64") => "claudulhu-linux-aarch64",
        ("macos",  "x86_64")  => "claudulhu-macos-x86_64",
        ("macos",  "aarch64") => "claudulhu-macos-aarch64",
        _ => anyhow::bail!("unsupported platform: {OS}/{ARCH}"),
    };

    let url = format!(
        "https://github.com/georgebradford0/claudulhu/releases/latest/download/{artifact}"
    );

    println!("Downloading latest {artifact}...");
    let status = Command::new("curl")
        .args(["-fsSL", &url, "-o", "/tmp/claudulhu-update"])
        .status()
        .await?;
    anyhow::ensure!(status.success(), "download failed");

    // Determine current binary path.
    let current = std::env::current_exe()?;
    let dest = current.to_str().unwrap_or("/usr/local/bin/claudulhu");

    Command::new("chmod").args(["+x", "/tmp/claudulhu-update"]).status().await?;

    // Try direct move, fall back to sudo.
    let mv = Command::new("mv")
        .args(["/tmp/claudulhu-update", dest])
        .status()
        .await?;
    if !mv.success() {
        let status = Command::new("sudo")
            .args(["mv", "/tmp/claudulhu-update", dest])
            .status()
            .await?;
        anyhow::ensure!(status.success(), "failed to install updated binary");
    }

    println!("Updated to latest release.");
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
                print!("This will delete the entire claudulhu namespace and all PVC data. Type 'yes' to confirm: ");
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
            use claudulhu_k8s_ops::k8s;
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
            ContainersAction::Restart { names } => containers::restart(&names).await?,
        },
        Command::Restart => {
            use claudulhu_k8s_ops::k8s;
            let client = k8s::build_client().await?;
            let updated = k8s::update_and_restart_all(&client).await?;
            if updated.is_empty() {
                println!("Nothing restarted.");
            } else {
                println!("Updated and restarted: {}", updated.join(", "));
            }
        }
        Command::Logs { name, follow } => {
            use claudulhu_k8s_ops::k8s;
            let client = k8s::build_client().await?;

            // Build list of deployment names to show logs for.
            let names: Vec<String> = if let Some(n) = name {
                vec![n]
            } else {
                let mut list = k8s::list_managed_deployments(&client).await?
                    .into_iter().map(|c| c.name).collect::<Vec<_>>();
                list.push("rulyeh".to_string());
                list
            };

            let multi = names.len() > 1;
            for deployment in &names {
                let pod_name = match k8s::get_running_pod(&client, deployment).await {
                    Ok(p)  => p,
                    Err(e) => { eprintln!("[{deployment}] {e}"); continue; }
                };

                if multi { println!("\n=== {deployment} ==="); }

                let mut args = vec!["logs", "-n", k8s::NAMESPACE, &pod_name];
                if follow { args.push("-f"); }

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
        Command::Completions { shell } => {
            generate(shell, &mut Cli::command(), "claudulhu", &mut std::io::stdout());
        }
        Command::Mcp { action } => match action {
            McpAction::List { container } => mcp::list(&container).await?,
            McpAction::Add { container, name, command, args, env } => {
                mcp::add(&container, &name, &command, &args, &env).await?;
            }
            McpAction::Remove { container, name } => mcp::remove(&container, &name).await?,
            McpAction::Import { container, file } => mcp::import_from_file(&container, &file).await?,
        },
    }
    Ok(())
}
