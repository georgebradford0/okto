mod agents;
mod init;
mod mcp;
mod qr;
mod service;
mod ssh;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use octo_core::Config;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

fn mask(s: &str) -> String {
    if s.len() <= 8 { return "*".repeat(s.len()); }
    format!("{}...{}", &s[..4], &s[s.len()-4..])
}

fn validate_resolved_config(cfg: &Config) -> Result<(), String> {
    let anthropic = cfg.anthropic_api_key.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let openai    = cfg.openai_api_key   .as_deref().map(str::trim).filter(|s| !s.is_empty());
    let api_url   = cfg.api_url          .as_deref().map(str::trim).filter(|s| !s.is_empty());
    let model     = cfg.model            .as_deref().map(str::trim).filter(|s| !s.is_empty());

    if anthropic.is_none() && openai.is_none() {
        return Err("at least one of anthropic_api_key or openai_api_key is required".into());
    }
    if model.is_none() {
        return Err("model is required (pass --model or set it in ~/.octo/config.json)".into());
    }
    if let Some(url) = api_url {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("api_url must start with http:// or https:// (got: {url})"));
        }
        if openai.is_none() && anthropic.is_none() {
            return Err("api_url is set so an API key is required".into());
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
    /// Bootstrap lair as a docker container on this host.
    ///
    /// Refuses to run if `~/.octo/config.json` already exists. On first run,
    /// prompts for the API keys / model interactively, writes config.json,
    /// pulls the lair image, then `docker run`s it.
    Init {
        /// Extra env var passed to the lair container via `docker --env-file`.
        /// Inherited by every child agent process lair spawns. Repeatable.
        #[arg(long = "env", short = 'e', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
        env: Vec<String>,

        /// Port that lair publishes its Noise endpoint on (host side)
        #[arg(long, default_value_t = service::LAIR_DEFAULT_NOISE_PORT)]
        noise_port: u16,

        /// Port that lair binds its HTTP / management API on (loopback only)
        #[arg(long, default_value_t = service::LAIR_DEFAULT_HTTP_PORT)]
        http_port: u16,

        /// Lair image reference. Defaults to `$OCTO_LAIR_IMAGE` or
        /// `ghcr.io/georgebradford0/lair:latest`.
        #[arg(long)]
        image: Option<String>,

        /// Path to an mcp.json file to seed lair's MCP tool list
        #[arg(long)]
        mcp_config: Option<std::path::PathBuf>,
    },

    /// Manage child agents
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },

    /// Stop lair, remove every managed agent, and wipe lair's host data dir
    Destroy {
        #[arg(short, long)]
        yes: bool,
    },

    /// Restart lair (and optionally agents) — picks up env / binary changes
    Reload {
        /// Specific agent names to also restart
        #[arg(long, value_name = "NAME", num_args = 1..)]
        agents: Vec<String>,
        /// Restart lair + every managed agent
        #[arg(long, conflicts_with = "agents")]
        all: bool,
    },

    /// Print the QR code mobile clients scan to connect to this lair
    Qr {
        /// Override the advertised host (defaults to PUBLIC_HOST from
        /// `octo env`, then the auto-detected public IP)
        #[arg(long)]
        host: Option<String>,
    },

    /// Show logs for lair or a named agent (lair by default)
    Logs {
        /// Agent name (e.g. lair, lair-foo). Omit for lair.
        name: Option<String>,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },

    /// Print the CLI version
    Version,

    /// Update the octo CLI to the latest release
    Update,

    /// Manage the lair docker image on this host
    Lair {
        #[command(subcommand)]
        action: LairAction,
    },

    /// Remove the octo binary and shell completions from this machine
    Uninstall {
        #[arg(short, long)]
        yes: bool,
    },

    Completions {
        shell: Shell,
    },

    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage extra env vars passed to lair (KEY=VALUE pairs persisted to
    /// ~/.octo/lair-env). Changes auto-restart lair.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },

    /// Manage the lair container's SSH identity.
    Ssh {
        #[command(subcommand)]
        action: SshAction,
    },
}

#[derive(Subcommand)]
enum SshAction {
    /// Print the lair container's SSH public key (the one every agent in
    /// the container uses for outbound SSH). Register this once on each
    /// external service (Prime Intellect, GitHub, GPU pods, etc.).
    Pubkey,
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Set {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        api_url: Option<String>,
        #[arg(long)]
        anthropic_api_key: Option<String>,
        #[arg(long)]
        openai_api_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum EnvAction {
    Show,
    Set { vars: Vec<String> },
    Unset { keys: Vec<String> },
}

#[derive(Subcommand)]
enum LairAction {
    /// Pull the latest lair image and restart the container
    Update {
        /// Image reference to pull. Defaults to the image recorded by `octo init`,
        /// then `$OCTO_LAIR_IMAGE`, then `ghcr.io/georgebradford0/lair:latest`.
        #[arg(long)]
        image: Option<String>,
    },

    /// Print the version of the running lair binary
    Version,
}

#[derive(Subcommand)]
enum AgentsAction {
    List,
    Start { name: String },
    Stop  { name: String },
    Delete {
        name: String,
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum McpAction {
    List {
        #[arg(long, default_value = "lair")]
        agent: String,
    },
    Add {
        #[arg(long, default_value = "lair")]
        agent: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        command: String,
        #[arg(last = true)]
        args: Vec<String>,
        #[arg(long)]
        env: Vec<String>,
    },
    Remove {
        #[arg(long, default_value = "lair")]
        agent: String,
        name: String,
    },
    Import {
        #[arg(long, default_value = "lair")]
        agent: String,
        file: std::path::PathBuf,
    },
}

/// Generate shell completion scripts at the canonical locations for bash,
/// zsh, and fish, and wire `~/.bashrc` to source the bash one. Idempotent —
/// safe to re-run on every `octo init`. Always overwrites existing files so
/// stale completions (e.g. for subcommands that have since been renamed or
/// removed) get refreshed.
fn install_completions() {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => {
            warn!("[cli] HOME unset; skipping completion install");
            return;
        }
    };

    // Bash — `~/.octo/octorc`, sourced from `~/.bashrc`.
    let octorc = home.join(".octo/octorc");
    if let Some(parent) = octorc.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Bash, &mut Cli::command(), "octo", &mut script);
    if let Err(e) = std::fs::write(&octorc, &script) {
        warn!("[cli] could not write {}: {e}", octorc.display());
        eprintln!("warning: could not write completions to {}: {e}", octorc.display());
    } else {
        debug!("[cli] wrote bash completions to {}", octorc.display());
        println!("Wrote bash completions to {}.", octorc.display());
    }

    // Zsh — `~/.zfunc/_octo`. Always (re)written so removed/renamed
    // subcommands from earlier installs don't linger.
    let zfunc = home.join(".zfunc/_octo");
    if let Some(parent) = zfunc.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Zsh, &mut Cli::command(), "octo", &mut script);
    if let Err(e) = std::fs::write(&zfunc, &script) {
        warn!("[cli] could not write {}: {e}", zfunc.display());
        eprintln!("warning: could not write completions to {}: {e}", zfunc.display());
    } else {
        debug!("[cli] wrote zsh completions to {}", zfunc.display());
        println!("Wrote zsh completions to {}.", zfunc.display());
    }

    // Fish — `~/.config/fish/completions/octo.fish`. Auto-loaded by fish
    // when present in this directory; no rc-file edit needed.
    let fish = home.join(".config/fish/completions/octo.fish");
    if let Some(parent) = fish.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Fish, &mut Cli::command(), "octo", &mut script);
    if let Err(e) = std::fs::write(&fish, &script) {
        warn!("[cli] could not write {}: {e}", fish.display());
        eprintln!("warning: could not write completions to {}: {e}", fish.display());
    } else {
        debug!("[cli] wrote fish completions to {}", fish.display());
        println!("Wrote fish completions to {}.", fish.display());
    }

    // Wire `~/.bashrc` to source the bash file (idempotent).
    let bashrc = home.join(".bashrc");
    let source_line = "source \"$HOME/.octo/octorc\"";
    let existing = std::fs::read_to_string(&bashrc).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == source_line) {
        return;
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("\n# Octo completions\n");
    updated.push_str(source_line);
    updated.push('\n');
    if let Err(e) = std::fs::write(&bashrc, updated) {
        warn!("[cli] could not update {}: {e}", bashrc.display());
        eprintln!("warning: could not update {}: {e}", bashrc.display());
        return;
    }
    println!("Added completion source to {}.", bashrc.display());
}

fn remove_completions() {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };
    let files = [
        home.join(".octo/octorc"),
        home.join(".local/share/bash-completion/completions/octo"),
        home.join(".zfunc/_octo"),
        home.join(".config/fish/completions/octo.fish"),
    ];
    for path in &files {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    // Drop any octo-related lines (the `# Octo completions` comment and its
    // `source` line, plus any legacy completion sources). Case-insensitive so
    // the capitalized comment is caught alongside the lowercased paths.
    let bashrc = home.join(".bashrc");
    if let Ok(content) = std::fs::read_to_string(&bashrc) {
        let cleaned = content
            .lines()
            .filter(|l| !l.to_lowercase().contains("octo"))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = if content.ends_with('\n') { cleaned + "\n" } else { cleaned };
        let _ = std::fs::write(&bashrc, cleaned);
    }
}

/// Regenerate shell completions in any of the canonical locations that
/// already contain an `octo` completion file. Silent on locations that don't
/// exist — we don't create new files (the user may have opted out of
/// completions at install time). Shells out to the freshly-installed binary
/// at `bin` rather than calling `clap_complete` in-process, so we pick up
/// any new subcommands.
async fn refresh_completions(bin: &std::path::Path) {
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return,
    };
    let targets: &[(&str, std::path::PathBuf)] = &[
        ("bash", home.join(".octo/octorc")),
        ("bash", home.join(".local/share/bash-completion/completions/octo")),
        ("zsh",  home.join(".zfunc/_octo")),
        ("fish", home.join(".config/fish/completions/octo.fish")),
    ];
    for (shell, path) in targets {
        if !path.exists() { continue; }
        debug!("[cli] running `octo completions {shell}` to refresh {}", path.display());
        let out = match tokio::process::Command::new(bin)
            .args(["completions", shell])
            .output().await
        {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                warn!("[cli] `octo completions {shell}` exited with {}; leaving {} untouched", o.status, path.display());
                eprintln!(
                    "warning: `octo completions {shell}` exited with {}; leaving {} untouched",
                    o.status, path.display(),
                );
                continue;
            }
            Err(e) => {
                warn!("[cli] could not run `octo completions {shell}`: {e}; leaving {} untouched", path.display());
                eprintln!(
                    "warning: could not run `octo completions {shell}`: {e}; leaving {} untouched",
                    path.display(),
                );
                continue;
            }
        };
        match std::fs::write(path, &out) {
            Ok(_)  => {
                debug!("[cli] wrote {shell} completions to {}", path.display());
                println!("Refreshed {shell} completions at {}", path.display());
            }
            Err(e) => {
                error!("[cli] could not write completions file {}: {e}", path.display());
                eprintln!("warning: could not write {}: {e}", path.display());
            }
        }
    }
}

async fn update() -> Result<()> {
    use std::env::consts::{ARCH, OS};
    use tokio::process::Command;

    info!("[cli] update starting ({OS}/{ARCH})");
    let artifact = match (OS, ARCH) {
        ("linux",  "x86_64")  => "octo-linux-x86_64",
        ("linux",  "aarch64") => "octo-linux-aarch64",
        _ => {
            error!("[cli] update: unsupported platform {OS}/{ARCH}");
            anyhow::bail!("unsupported platform: {OS}/{ARCH}");
        }
    };

    debug!("[cli] fetching latest release metadata from github API");
    let api_output = Command::new("curl")
        .args(["-fsSL", "https://api.github.com/repos/georgebradford0/octo/releases/latest"])
        .output()
        .await?;
    debug!("[cli] release metadata curl exited with {}", api_output.status);
    if !api_output.status.success() {
        error!("[cli] update: failed to fetch release info (curl status {})", api_output.status);
    }
    anyhow::ensure!(api_output.status.success(), "failed to fetch release info");
    let api_json: serde_json::Value = serde_json::from_slice(&api_output.stdout)?;
    let latest_tag = api_json["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("unexpected release API response"))?;
    let latest_version = latest_tag.trim_start_matches('v');

    let current_version = env!("CARGO_PKG_VERSION");
    let current_exe = std::env::current_exe()?;
    let current_exe_str = current_exe.to_str().unwrap_or("/usr/local/bin/octo");
    if latest_version == current_version {
        info!("[cli] update: already on latest (v{current_version})");
        println!("Already up to date (v{current_version}).");
        // Still reconcile completions in case they were left stale by an
        // older `octo update` that predated the refresh logic.
        refresh_completions(std::path::Path::new(current_exe_str)).await;
        return Ok(());
    }

    let url = format!("https://github.com/georgebradford0/octo/releases/latest/download/{artifact}");

    println!("Downloading {artifact}...");
    debug!("[cli] downloading {url}");
    let status = Command::new("curl")
        .args(["-fsSL", &url, "-o", "/tmp/octo-update"])
        .status()
        .await?;
    debug!("[cli] download curl exited with {status}");
    if !status.success() {
        error!("[cli] update: download failed (curl status {status})");
    }
    anyhow::ensure!(status.success(), "download failed");

    let dest = current_exe_str;

    debug!("[cli] chmod +x /tmp/octo-update");
    Command::new("chmod").args(["+x", "/tmp/octo-update"]).status().await?;

    debug!("[cli] installing updated binary to {dest}");
    let mv = Command::new("mv")
        .args(["/tmp/octo-update", dest])
        .status()
        .await?;
    if !mv.success() {
        warn!("[cli] update: `mv` to {dest} failed; retrying with sudo");
        let status = Command::new("sudo")
            .args(["mv", "/tmp/octo-update", dest])
            .status()
            .await?;
        if !status.success() {
            error!("[cli] update: failed to install updated binary to {dest} (sudo mv status {status})");
        }
        anyhow::ensure!(status.success(), "failed to install updated binary");
    }

    refresh_completions(std::path::Path::new(dest)).await;

    info!("[cli] update complete: v{current_version} -> v{latest_version}");
    println!("Updated: v{current_version} → v{latest_version}");
    Ok(())
}

async fn update_lair(image_override: Option<String>) -> Result<()> {
    info!("[cli] lair update starting");
    service::ensure_docker_present()?;

    let launch = service::read_launch();
    let prior_image = launch.as_ref().and_then(|l| l.image.clone());
    let image = match image_override {
        Some(i) if !i.is_empty() => i,
        _ => service::resolve_image(prior_image.as_deref()),
    };
    debug!("[cli] lair update resolved image: {image}");

    println!("Pulling {image}...");
    service::docker_pull(&image)?;

    // Persist the (possibly new) image reference so subsequent reloads keep
    // using it without --image being repassed.
    if let Some(mut rec) = launch {
        rec.image = Some(image.clone());
        service::write_launch(&rec)?;
    }

    if service::is_running() {
        init::restart_lair("lair update").await?;
        info!("[cli] lair update complete (container restarted on {image})");
    } else if service::read_launch().is_some() {
        info!("[cli] lair update: image pulled; lair not running, will apply on next reload");
        println!("lair is not running; new image will be used on next `octo reload`.");
    } else {
        info!("[cli] lair update: image pulled; lair not initialized");
        println!("lair has not been initialized; run `octo init` to start it.");
    }
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
    debug!("[cli] uninstall: removing binary {path}");
    let removed = std::fs::remove_file(&current);
    if removed.is_err() {
        warn!("[cli] uninstall: direct removal of {path} failed; retrying with sudo");
        let status = tokio::process::Command::new("sudo")
            .args(["rm", "-f", path])
            .status()
            .await?;
        if !status.success() {
            error!("[cli] uninstall: failed to remove {path} (sudo rm status {status})");
        }
        anyhow::ensure!(status.success(), "failed to remove {path}");
    }

    info!("[cli] uninstall complete ({path} removed)");
    println!("Removed {}.", path);
    Ok(())
}

async fn stream_logs(name: &str, follow: bool) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    if name == "lair" {
        return service::stream_lair_logs(follow).await;
    }
    let path = service::agents_dir().join(name).join("agent.log");
    if !path.exists() {
        anyhow::bail!("no log file at {}", path.display());
    }
    let mut f = std::fs::File::open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // Print the last 1MB.
    let len = f.metadata()?.len();
    let offset = len.saturating_sub(1024 * 1024);
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok();
    print!("{buf}");
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    if !follow { return Ok(()); }

    let mut pos = len;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let new_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
        if new_len > pos {
            let mut f = std::fs::File::open(&path)?;
            f.seek(SeekFrom::Start(pos))?;
            let mut buf = String::new();
            f.read_to_string(&mut buf).ok();
            print!("{buf}");
            std::io::stdout().flush().ok();
            pos = new_len;
        } else if new_len < pos {
            // Log was truncated; reset.
            pos = new_len;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the tracing subscriber before anything else so library
    // (`octo-core`) and CLI `tracing::*` calls have somewhere to land. Quiet
    // by default (`warn`) so normal `octo` runs are unchanged; opt into
    // diagnostics via `OCTO_LOG=debug` (or `RUST_LOG=debug`). Always stderr,
    // never stdout, so user-facing `println!` output stays clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_env("OCTO_LOG")
                .or_else(|_| EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init { env, noise_port, http_port, image, mcp_config } => {
            info!("[cli] init starting (noise_port={noise_port}, http_port={http_port})");
            let extra_env = init::parse_extra_env(&env)?;

            let config_path = octo_core::config_path();
            let config_exists = config_path.exists();

            // Pre-flight: validate any --mcp-config file BEFORE we prompt or
            // write anything. A broken mcp file used to fail after config.json
            // was written, leaving `octo init` refusing to re-run and lair
            // never started.
            let mcp_seed = match mcp_config.as_deref() {
                Some(p) => Some(init::McpSeed {
                    source: p.to_path_buf(),
                    json:   init::load_seed_mcp_config(p)?,
                }),
                None => None,
            };

            if config_exists {
                debug!("[init] reusing existing config at {}", config_path.display());
                println!(
                    "{} exists — reusing it. (Edit via `octo config set …` or `octo destroy` to start over.)",
                    config_path.display(),
                );
                let cfg = octo_core::read_config();
                if let Err(e) = validate_resolved_config(&cfg) {
                    error!("[init] existing config {} is invalid: {e}", config_path.display());
                    eprintln!("error: existing {} is invalid: {e}", config_path.display());
                    eprintln!("Edit it directly or run `octo config set ...` and re-run `octo init`.");
                    std::process::exit(1);
                }
            } else {
                println!("{} not found — let's configure octo.\n", config_path.display());

                let anthropic = init::prompt("Anthropic API key (Enter to skip):       ")?;
                let openai    = init::prompt("OpenAI API key (Enter to skip):          ")?;
                let api_url   = init::prompt("API URL (Enter for Anthropic default):   ")?;
                let model     = init::prompt("Model (e.g. claude-sonnet-4-6):          ")?;

                let to_opt = |s: String| {
                    let s = s.trim().to_string();
                    if s.is_empty() { None } else { Some(s) }
                };
                let cfg = Config {
                    anthropic_api_key: to_opt(anthropic),
                    openai_api_key:    to_opt(openai),
                    api_url:           to_opt(api_url),
                    model:             to_opt(model),
                    ..Default::default()
                };

                if let Err(e) = validate_resolved_config(&cfg) {
                    error!("[init] invalid config supplied interactively: {e}");
                    eprintln!("\nerror: invalid config: {e}");
                    std::process::exit(1);
                }

                octo_core::write_config(&cfg);
                debug!("[init] wrote config file {}", config_path.display());
                println!("\nWrote {}.", config_path.display());
            }

            install_completions();

            init::run(init::InitOptions {
                noise_port,
                http_port,
                mcp_seed,
                extra_env:  &extra_env,
                image:      image.as_deref(),
            }).await?;
            info!("[cli] init complete");
        }

        Command::Destroy { yes } => {
            info!("[cli] destroy starting");
            if !yes {
                use std::io::Write;
                print!("This will stop lair, terminate every agent, and wipe ~/.octo/lair and ~/.octo/agents. Type 'yes' to confirm: ");
                std::io::stdout().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if input.trim() != "yes" {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            // Best-effort: ask lair to terminate every agent first so it cleans
            // up child processes too. Ignore errors — we'll wipe dirs anyway.
            if service::is_running() {
                let path = service::lair_data_dir().join("agents.json");
                if let Ok(reg) = octo_core::Registry::load(path) {
                    for a in reg.list() {
                        println!("Terminating '{}'...", a.name);
                        let _ = agents::delete(&a.name, true).await;
                    }
                }
            }
            service::stop_lair_if_running();
            for dir in [service::lair_data_dir(), service::agents_dir()] {
                if dir.exists() {
                    debug!("[cli] removing directory {}", dir.display());
                    println!("Removing {}...", dir.display());
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
            let env_file = service::env_file_path();
            if env_file.exists() {
                debug!("[cli] removing env file {}", env_file.display());
                let _ = std::fs::remove_file(&env_file);
            }
            let launch = service::launch_config_path();
            if launch.exists() {
                debug!("[cli] removing launch record {}", launch.display());
                let _ = std::fs::remove_file(&launch);
            }
            remove_completions();
            info!("[cli] destroy complete");
            println!("Done.");
        }

        Command::Agents { action } => {
            match action {
                AgentsAction::List => agents::list().await?,
                AgentsAction::Start  { name }      => agents::start(&name).await?,
                AgentsAction::Stop   { name }      => agents::stop(&name).await?,
                AgentsAction::Delete { name, yes } => agents::delete(&name, yes).await?,
            }
        }

        Command::Reload { agents: agent_targets, all } => {
            info!("[cli] reload starting (all={all})");
            init::restart_lair("reload").await?;

            let names: Vec<String> = if all {
                let path = service::lair_data_dir().join("agents.json");
                match octo_core::Registry::load(path) {
                    Ok(r)  => r.list().iter().map(|a| a.name.clone()).collect(),
                    Err(_) => Vec::new(),
                }
            } else {
                agent_targets
            };
            for name in &names {
                print!("  {name} ... ");
                use std::io::Write; std::io::stdout().flush().ok();
                // Stop + start via lair's management API.
                if let Err(e) = agents::stop(name).await {
                    error!("[cli] reload: failed to stop agent '{name}': {e:#}");
                    println!("stop error: {e:#}");
                    continue;
                }
                if let Err(e) = agents::start(name).await {
                    error!("[cli] reload: failed to start agent '{name}': {e:#}");
                    println!("start error: {e:#}");
                    continue;
                }
                info!("[cli] agent '{name}' restarted");
                println!("restarted.");
            }
            info!("[cli] reload complete");
        }

        Command::Qr { host } => {
            qr::print(host).await?;
        }

        Command::Logs { name, follow } => {
            let target = name.unwrap_or_else(|| "lair".to_string());
            stream_logs(&target, follow).await?;
        }

        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Command::Update => update().await?,
        Command::Lair { action } => match action {
            LairAction::Update { image } => update_lair(image).await?,
            LairAction::Version => println!("{}", service::lair_binary_version()?),
        },
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
                    println!("model:             {}", cfg.model.as_deref().unwrap_or("(default)"));
                    println!("api_url:           {}", cfg.api_url.as_deref().unwrap_or("(Anthropic)"));
                }
                ConfigAction::Set { model, api_url, anthropic_api_key, openai_api_key } => {
                    let mut cfg = octo_core::read_config();
                    if anthropic_api_key.is_some() {
                        debug!("[cli] config set: updating anthropic_api_key");
                        cfg.anthropic_api_key = anthropic_api_key;
                    }
                    if openai_api_key.is_some() {
                        debug!("[cli] config set: updating openai_api_key");
                        cfg.openai_api_key = openai_api_key;
                    }
                    if model.is_some() {
                        debug!("[cli] config set: updating model");
                        cfg.model = model;
                    }
                    if api_url.is_some() {
                        debug!("[cli] config set: updating api_url");
                        cfg.api_url = api_url;
                    }
                    octo_core::write_config(&cfg);
                    info!("[cli] config updated at {}", octo_core::config_path().display());
                    println!("Config updated.");
                }
            }
        }
        Command::Env { action } => {
            let path = service::env_file_path();
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))
                .unwrap_or_default();
            let mut entries = init::parse_env_file(&text);

            match action {
                EnvAction::Show => {
                    let operator: Vec<_> = entries.iter()
                        .filter(|(k, _)| !init::MANAGED_ENV_KEYS.contains(&k.as_str()))
                        .collect();
                    if operator.is_empty() {
                        println!("(no operator env vars set — use `octo env set KEY=VALUE`)");
                    } else {
                        for (k, v) in operator {
                            println!("{k}={}", mask(v));
                        }
                    }
                }
                EnvAction::Set { vars } => {
                    let new_pairs = init::parse_extra_env(&vars)?;
                    if new_pairs.is_empty() {
                        anyhow::bail!("no KEY=VALUE pairs supplied");
                    }
                    for (k, v) in new_pairs {
                        // Never log the value — operator env vars may hold secrets.
                        debug!("[cli] env set: upserting key '{k}'");
                        if let Some(slot) = entries.iter_mut().find(|(ek, _)| ek == &k) {
                            slot.1 = v;
                        } else {
                            entries.push((k, v));
                        }
                    }
                    debug!("[cli] writing env file {} ({} entries)", path.display(), entries.len());
                    init::write_secret_file(&path, &init::serialize_env_file(&entries))?;
                    init::restart_lair("env set").await?;
                }
                EnvAction::Unset { keys } => {
                    if keys.is_empty() {
                        anyhow::bail!("no keys supplied");
                    }
                    for k in &keys {
                        if init::MANAGED_ENV_KEYS.contains(&k.as_str()) {
                            anyhow::bail!("'{k}' is managed by octo and can't be unset");
                        }
                    }
                    let before = entries.len();
                    entries.retain(|(k, _)| !keys.contains(k));
                    if entries.len() == before {
                        println!("No matching keys to remove.");
                    } else {
                        debug!("[cli] env unset: removed {} key(s); writing {}", before - entries.len(), path.display());
                        init::write_secret_file(&path, &init::serialize_env_file(&entries))?;
                        init::restart_lair("env unset").await?;
                    }
                }
            }
        }
        Command::Ssh { action } => match action {
            SshAction::Pubkey => ssh::pubkey().await?,
        },
    }
    Ok(())
}
