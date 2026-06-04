mod agents;
mod init;
mod mcp;
mod qr;
mod service;
mod ssh;
mod tasks;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use okto_core::Config;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

fn mask(s: &str) -> String {
    if s.len() <= 8 { return "*".repeat(s.len()); }
    format!("{}...{}", &s[..4], &s[s.len()-4..])
}

/// Resolve a `TEXT | @PATH` value. A leading `@` means "read the rest of the
/// string as a file path"; anything else is taken verbatim. An empty result
/// is mapped to `None`, which the caller treats as "clear this field".
fn resolve_text_or_at_path(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let text = if let Some(path) = trimmed.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("read {path}"))?
    } else {
        raw.to_string()
    };
    let text = text.trim().to_string();
    Ok((!text.is_empty()).then_some(text))
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
        return Err("model is required (pass --model or set it in ~/.okto/config.json)".into());
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
#[command(name = "okto", about = "okto lair management CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap lair as a docker container on this host.
    ///
    /// On first run (no `~/.okto/config.json`) prompts for the API keys / model
    /// interactively and writes config.json. If a config already exists it is
    /// reused as-is (no prompts). Either way, pulls the lair image and
    /// `docker run`s it — so `init` is safe to re-run to (re)start lair.
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

        /// Lair image reference. Defaults to `$OKTO_LAIR_IMAGE` or
        /// `ghcr.io/georgebradford0/lair:latest`.
        #[arg(long)]
        image: Option<String>,

        /// Path to an mcp.json file to seed lair's MCP tool list
        #[arg(long)]
        mcp_config: Option<std::path::PathBuf>,

        /// Free-form text appended to lair's built-in system prompt.
        /// Use `@<path>` to read from a file. Stored verbatim in
        /// `~/.okto/config.json` as `system_prompt_append` and re-read on
        /// every turn, so subsequent edits to the file take effect without
        /// restarting the container.
        #[arg(long, value_name = "TEXT|@PATH")]
        system_prompt_append: Option<String>,

        /// Disable push notifications end-to-end. Persists `OKTO_RELAY_URL=`
        /// (explicit empty) into `~/.okto/lair-env`, which (a) drops the
        /// `send_notification` and `ask_question` tools from the LLM's tool
        /// list in both lair and child agents and (b) causes the mobile
        /// client to skip registering for pushes (lair's `/info` advertises
        /// an empty relay URL). To re-enable later: `okto env unset
        /// OKTO_RELAY_URL && okto reload`.
        #[arg(long)]
        disable_push: bool,

        /// How long (seconds) to wait for `/health` after `docker run`. Bump
        /// this when your `~/.okto/bootstrap.sh` does heavy work like
        /// `apt-get install` of large packages, especially on a fresh image
        /// pull when the apt cache is cold. Default 180s.
        #[arg(long, value_name = "SECS", default_value_t = service::DEFAULT_READY_TIMEOUT_SECS)]
        ready_timeout: u64,
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

    /// Restart lair to update env / config
    Reload {
        /// Restart only these specific agents (defaults to every managed agent)
        #[arg(long, value_name = "NAME", num_args = 1..)]
        agents: Vec<String>,
        /// Upsert an env var into `~/.okto/lair-env` before restarting.
        /// Existing keys are overwritten in place; new keys are appended.
        /// Repeatable.
        #[arg(long = "env", short = 'e', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
        env: Vec<String>,
        /// How long (seconds) to wait for `/health` after restart. Default
        /// 180s — bump it for heavy `bootstrap.sh` installs.
        #[arg(long, value_name = "SECS", default_value_t = service::DEFAULT_READY_TIMEOUT_SECS)]
        ready_timeout: u64,
    },

    /// Print the QR code mobile clients scan to connect to this lair
    Qr {
        /// Override the advertised host (defaults to PUBLIC_HOST from
        /// `okto env`, then the auto-detected public IP)
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

    /// Update the okto CLI to the latest release
    Update,

    /// Manage the lair docker image on this host
    Lair {
        #[command(subcommand)]
        action: LairAction,
    },

    /// Remove the okto binary and shell completions from this machine
    Uninstall {
        #[arg(short, long)]
        yes: bool,
    },

    /// Print shell-completion script for the given shell (bash, zsh, fish,
    /// elvish, powershell) to stdout. Source the output or write it to your
    /// shell's completions dir to enable tab-completion for `okto` commands
    /// and flags. `okto init` installs the right script automatically when it
    /// can detect your shell.
    Completions {
        shell: Shell,
    },

    /// Manage the MCP (Model Context Protocol) servers lair and its agents
    /// load. Each server appears as a set of additional tools in the LLM's
    /// tool list. Subcommands cover listing what's configured, adding/removing
    /// servers, and importing a JSON config from disk. Defaults to lair;
    /// pass `--agent <name>` on subcommands that support it to target a child
    /// agent instead.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Read or edit `~/.okto/config.json` — the operator credentials and
    /// model settings lair uses (Anthropic / OpenAI API keys, model, optional
    /// `api_url`, `system_prompt_append`). Lair re-reads the file on every
    /// turn, so changes apply live without restarting the container.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage extra env vars passed to lair (KEY=VALUE pairs persisted to
    /// ~/.okto/lair-env). Changes auto-restart lair.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },

    /// Manage the lair container's SSH identity.
    Ssh {
        #[command(subcommand)]
        action: SshAction,
    },

    /// View and stop background tasks running in lair or child agents.
    Tasks {
        #[command(subcommand)]
        action: TasksAction,
    },
}

#[derive(Subcommand)]
enum TasksAction {
    /// List background tasks. Defaults to lair + every known agent.
    List {
        /// Restrict to this agent's tasks (otherwise aggregates across the fleet).
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
    /// Stop a running background task by id.
    Stop {
        /// Task id (e.g. `bg-abc12345`).
        id: String,
        /// Target a specific agent. Omit for a lair-local task.
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
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
        /// Replace `system_prompt_append` in `~/.okto/config.json`. Use
        /// `@<path>` to read from a file, or an empty string ("") to clear it.
        #[arg(long, value_name = "TEXT|@PATH")]
        system_prompt_append: Option<String>,
        /// Input-token price in USD per 1M tokens for OpenAI-compatible
        /// backends (config key `cost_input1M`). Set together with
        /// `--cost-output1m` to get per-turn cost; pass a negative value to
        /// clear. Ignored for Anthropic, which uses built-in pricing.
        #[arg(long, value_name = "USD_PER_1M", allow_hyphen_values = true)]
        cost_input1m: Option<f64>,
        /// Output-token price in USD per 1M tokens for OpenAI-compatible
        /// backends (config key `cost_output1M`). See `--cost-input1m`.
        #[arg(long, value_name = "USD_PER_1M", allow_hyphen_values = true)]
        cost_output1m: Option<f64>,
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
        /// Image reference to pull. Defaults to the image recorded by `okto init`,
        /// then `$OKTO_LAIR_IMAGE`, then `ghcr.io/georgebradford0/lair:latest`.
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
/// safe to re-run on every `okto init`. Always overwrites existing files so
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

    // Bash — `~/.okto/oktorc`, sourced from `~/.bashrc`.
    let oktorc = home.join(".okto/oktorc");
    if let Some(parent) = oktorc.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Bash, &mut Cli::command(), "okto", &mut script);
    if let Err(e) = std::fs::write(&oktorc, &script) {
        warn!("[cli] could not write {}: {e}", oktorc.display());
        eprintln!("warning: could not write completions to {}: {e}", oktorc.display());
    } else {
        debug!("[cli] wrote bash completions to {}", oktorc.display());
        println!("Wrote bash completions to {}.", oktorc.display());
    }

    // Zsh — `~/.zfunc/_okto`. Always (re)written so removed/renamed
    // subcommands from earlier installs don't linger.
    let zfunc = home.join(".zfunc/_okto");
    if let Some(parent) = zfunc.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Zsh, &mut Cli::command(), "okto", &mut script);
    if let Err(e) = std::fs::write(&zfunc, &script) {
        warn!("[cli] could not write {}: {e}", zfunc.display());
        eprintln!("warning: could not write completions to {}: {e}", zfunc.display());
    } else {
        debug!("[cli] wrote zsh completions to {}", zfunc.display());
        println!("Wrote zsh completions to {}.", zfunc.display());
    }

    // Fish — `~/.config/fish/completions/okto.fish`. Auto-loaded by fish
    // when present in this directory; no rc-file edit needed.
    let fish = home.join(".config/fish/completions/okto.fish");
    if let Some(parent) = fish.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut script = Vec::new();
    generate(Shell::Fish, &mut Cli::command(), "okto", &mut script);
    if let Err(e) = std::fs::write(&fish, &script) {
        warn!("[cli] could not write {}: {e}", fish.display());
        eprintln!("warning: could not write completions to {}: {e}", fish.display());
    } else {
        debug!("[cli] wrote fish completions to {}", fish.display());
        println!("Wrote fish completions to {}.", fish.display());
    }

    // Wire `~/.bashrc` to source the bash file (idempotent).
    let bashrc = home.join(".bashrc");
    let source_line = "source \"$HOME/.okto/oktorc\"";
    let existing = std::fs::read_to_string(&bashrc).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == source_line) {
        return;
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("\n# Okto completions\n");
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
        home.join(".okto/oktorc"),
        home.join(".local/share/bash-completion/completions/okto"),
        home.join(".zfunc/_okto"),
        home.join(".config/fish/completions/okto.fish"),
    ];
    for path in &files {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    // Drop any okto-related lines (the `# Okto completions` comment and its
    // `source` line, plus any legacy completion sources). Case-insensitive so
    // the capitalized comment is caught alongside the lowercased paths.
    let bashrc = home.join(".bashrc");
    if let Ok(content) = std::fs::read_to_string(&bashrc) {
        let cleaned = content
            .lines()
            .filter(|l| !l.to_lowercase().contains("okto"))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = if content.ends_with('\n') { cleaned + "\n" } else { cleaned };
        let _ = std::fs::write(&bashrc, cleaned);
    }
}

/// Regenerate shell completions in any of the canonical locations that
/// already contain an `okto` completion file. Silent on locations that don't
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
        ("bash", home.join(".okto/oktorc")),
        ("bash", home.join(".local/share/bash-completion/completions/okto")),
        ("zsh",  home.join(".zfunc/_okto")),
        ("fish", home.join(".config/fish/completions/okto.fish")),
    ];
    for (shell, path) in targets {
        if !path.exists() { continue; }
        debug!("[cli] running `okto completions {shell}` to refresh {}", path.display());
        let out = match tokio::process::Command::new(bin)
            .args(["completions", shell])
            .output().await
        {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                warn!("[cli] `okto completions {shell}` exited with {}; leaving {} untouched", o.status, path.display());
                eprintln!(
                    "warning: `okto completions {shell}` exited with {}; leaving {} untouched",
                    o.status, path.display(),
                );
                continue;
            }
            Err(e) => {
                warn!("[cli] could not run `okto completions {shell}`: {e}; leaving {} untouched", path.display());
                eprintln!(
                    "warning: could not run `okto completions {shell}`: {e}; leaving {} untouched",
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
        ("linux",  "x86_64")  => "okto-linux-x86_64",
        ("linux",  "aarch64") => "okto-linux-aarch64",
        _ => {
            error!("[cli] update: unsupported platform {OS}/{ARCH}");
            anyhow::bail!("unsupported platform: {OS}/{ARCH}");
        }
    };

    debug!("[cli] fetching latest release metadata from github API");
    let api_output = Command::new("curl")
        .args(["-fsSL", "https://api.github.com/repos/georgebradford0/okto/releases/latest"])
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
    let current_exe_str = current_exe.to_str().unwrap_or("/usr/local/bin/okto");
    if latest_version == current_version {
        info!("[cli] update: already on latest (v{current_version})");
        println!("Already up to date (v{current_version}).");
        // Still reconcile completions in case they were left stale by an
        // older `okto update` that predated the refresh logic.
        refresh_completions(std::path::Path::new(current_exe_str)).await;
        return Ok(());
    }

    let url = format!("https://github.com/georgebradford0/okto/releases/latest/download/{artifact}");

    println!("Downloading {artifact}...");
    debug!("[cli] downloading {url}");
    let status = Command::new("curl")
        .args(["-fsSL", &url, "-o", "/tmp/okto-update"])
        .status()
        .await?;
    debug!("[cli] download curl exited with {status}");
    if !status.success() {
        error!("[cli] update: download failed (curl status {status})");
    }
    anyhow::ensure!(status.success(), "download failed");

    let dest = current_exe_str;

    debug!("[cli] chmod +x /tmp/okto-update");
    Command::new("chmod").args(["+x", "/tmp/okto-update"]).status().await?;

    debug!("[cli] installing updated binary to {dest}");
    let mv = Command::new("mv")
        .args(["/tmp/okto-update", dest])
        .status()
        .await?;
    if !mv.success() {
        warn!("[cli] update: `mv` to {dest} failed; retrying with sudo");
        let status = Command::new("sudo")
            .args(["mv", "/tmp/okto-update", dest])
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

    // Capture the version of the currently-running lair before we pull, so
    // we can show a before/after comparison once the restart lands.
    let old_version = if service::is_running() {
        service::lair_binary_version().ok()
    } else {
        None
    };

    // Snapshot which *local* agents are currently Running so we can respawn
    // them on the new image after the container restart. Local agents are
    // processes inside the lair container — they die when the container is
    // recreated and are otherwise left as Stopped in the registry until
    // someone manually starts them. Remote agents are skipped: they run in
    // their own containers on other hosts and are unaffected by a local
    // restart (lair just re-opens its outbound Noise tunnel to them on
    // demand). Read the registry from disk directly rather than going via
    // the mgmt API, mirroring `agents::list`.
    // Capture each agent's route-safe `slug` (lair's registry key), not its
    // display `name`. We already hold the loaded record here, so pass the slug
    // straight through to the start endpoint rather than re-deriving it from the
    // name after the restart — that round-trip is what broke when an older CLI
    // (pre-slug) sent a display name to a slug-keyed lair. `(name, slug)` keeps
    // the friendly name for the progress/error messages.
    let local_agents_to_restart: Vec<(String, String)> = if service::is_running() {
        let registry_path = service::lair_data_dir().join("agents.json");
        match okto_core::Registry::load(registry_path) {
            Ok(reg) => reg.list().iter()
                .filter(|a| !a.is_remote() && a.status == okto_core::AgentStatus::Running)
                .map(|a| (a.name.clone(), a.slug.clone()))
                .collect(),
            Err(e) => {
                warn!("[cli] could not read agent registry for restart snapshot: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    debug!(
        "[cli] lair update will respawn {} local agent(s) after restart: {:?}",
        local_agents_to_restart.len(),
        local_agents_to_restart,
    );

    println!("Pulling {image}...");
    service::docker_pull(&image)?;

    // Persist the (possibly new) image reference so subsequent reloads keep
    // using it without --image being repassed.
    if let Some(mut rec) = launch {
        rec.image = Some(image.clone());
        service::write_launch(&rec)?;
    }

    if service::is_running() {
        init::restart_lair("lair update", std::time::Duration::from_secs(service::DEFAULT_READY_TIMEOUT_SECS)).await?;
        let new_version = service::lair_binary_version().ok();
        match (old_version.as_deref(), new_version.as_deref()) {
            (Some(old), Some(new)) if old != new => {
                println!("Updated: {old} → {new}");
            }
            (Some(_), Some(new)) => {
                println!("Already up to date: {new}");
            }
            (_, Some(new)) => {
                println!("Running: {new}");
            }
            _ => {}
        }
        info!("[cli] lair update complete (container restarted on {image})");

        // Respawn local agents that were Running before the restart. Per-
        // agent failures are reported but don't fail the whole update — the
        // user can retry individual agents with `okto agents start <name>`.
        // `init::restart_lair` already polls /health before returning, so
        // the mgmt API is ready to accept these POSTs.
        if !local_agents_to_restart.is_empty() {
            println!(
                "Restarting {} local agent(s) on new image...",
                local_agents_to_restart.len(),
            );
            for (name, slug) in &local_agents_to_restart {
                // Address the agent by slug — the key lair's API is keyed on —
                // so the restart is immune to display-name → slug translation.
                if let Err(e) = agents::start(slug).await {
                    error!("[cli] failed to restart agent '{name}' ({slug}) after update: {e}");
                    println!("  Failed to restart '{name}': {e} (run `okto agents start {slug}` to retry)");
                }
            }
        }
    } else if service::read_launch().is_some() {
        info!("[cli] lair update: image pulled; lair not running, will apply on next reload");
        println!("lair is not running; new image will be used on next `okto reload`.");
    } else {
        info!("[cli] lair update: image pulled; lair not initialized");
        println!("lair has not been initialized; run `okto init` to start it.");
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
    // The per-agent dir is keyed by slug; resolve a display name to it.
    let slug = agents::resolve_slug(name);
    let path = service::agents_dir().join(&slug).join("agent.log");
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
    // (`okto-core`) and CLI `tracing::*` calls have somewhere to land. Quiet
    // by default (`warn`) so normal `okto` runs are unchanged; opt into
    // diagnostics via `OKTO_LOG=debug` (or `RUST_LOG=debug`). Always stderr,
    // never stdout, so user-facing `println!` output stays clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_env("OKTO_LOG")
                .or_else(|_| EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init { env, noise_port, http_port, image, mcp_config, system_prompt_append, disable_push, ready_timeout } => {
            info!("[cli] init starting (noise_port={noise_port}, http_port={http_port}, disable_push={disable_push}, ready_timeout={ready_timeout}s)");
            let mut extra_env = init::parse_extra_env(&env)?;
            if disable_push {
                // Reject the contradictory combo up front so the operator
                // doesn't have to guess which one won.
                if let Some((k, _)) = extra_env.iter().find(|(k, _)| k == "OKTO_RELAY_URL") {
                    error!("[init] --disable-push conflicts with --env {k}=...");
                    eprintln!("error: --disable-push conflicts with --env OKTO_RELAY_URL=...; pass one or the other.");
                    std::process::exit(1);
                }
                // Empty value is the on-wire signal to lair (see
                // `OKTO_RELAY_URL` parsing in lair/src/lair.rs).
                extra_env.push(("OKTO_RELAY_URL".to_string(), String::new()));
                info!("[init] --disable-push set; will persist OKTO_RELAY_URL= in lair-env");
                println!("Push notifications disabled (OKTO_RELAY_URL= written to lair-env).");
            }

            // Resolve `--system-prompt-append` up front so a bad `@path`
            // fails before we mutate anything on disk.
            let prompt_append = match system_prompt_append.as_deref() {
                Some(raw) => Some(resolve_text_or_at_path(raw)?),
                None      => None,
            };

            let config_path = okto_core::config_path();
            let config_exists = config_path.exists();

            // Pre-flight: validate any --mcp-config file BEFORE we prompt or
            // write anything. A broken mcp file used to fail after config.json
            // was written, leaving `okto init` refusing to re-run and lair
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
                    "{} exists — reusing it. (Edit via `okto config set …` or `okto destroy` to start over.)",
                    config_path.display(),
                );
                let mut cfg = okto_core::read_config();
                if let Err(e) = validate_resolved_config(&cfg) {
                    error!("[init] existing config {} is invalid: {e}", config_path.display());
                    eprintln!("error: existing {} is invalid: {e}", config_path.display());
                    eprintln!("Edit it directly or run `okto config set ...` and re-run `okto init`.");
                    std::process::exit(1);
                }
                if let Some(new_append) = prompt_append.clone() {
                    debug!("[init] updating system_prompt_append on existing config");
                    cfg.system_prompt_append = new_append;
                    okto_core::write_config(&cfg);
                    println!("Updated system_prompt_append in {}.", config_path.display());
                }
            } else {
                println!("{} not found — let's configure okto.\n", config_path.display());

                let anthropic = init::prompt("Anthropic API key (Enter to skip):       ")?;
                let openai    = init::prompt("OpenAI API key (Enter to skip):          ")?;
                let api_url   = init::prompt("API URL (Enter for Anthropic default):   ")?;
                let model     = init::prompt("Model (Enter for claude-sonnet-4-6):     ")?;

                let to_opt = |s: String| {
                    let s = s.trim().to_string();
                    if s.is_empty() { None } else { Some(s) }
                };
                let cfg = Config {
                    anthropic_api_key: to_opt(anthropic),
                    openai_api_key:    to_opt(openai),
                    api_url:           to_opt(api_url),
                    model:             Some(to_opt(model).unwrap_or_else(|| "claude-sonnet-4-6".to_string())),
                    system_prompt_append: prompt_append.clone().unwrap_or(None),
                    ..Default::default()
                };

                if let Err(e) = validate_resolved_config(&cfg) {
                    error!("[init] invalid config supplied interactively: {e}");
                    eprintln!("\nerror: invalid config: {e}");
                    std::process::exit(1);
                }

                okto_core::write_config(&cfg);
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
                ready_timeout: std::time::Duration::from_secs(ready_timeout),
            }).await?;
            info!("[cli] init complete");
        }

        Command::Destroy { yes } => {
            info!("[cli] destroy starting");
            if !yes {
                use std::io::Write;
                print!("This will stop lair, terminate every agent, and wipe ~/.okto/lair and ~/.okto/agents. Type 'yes' to confirm: ");
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
                if let Ok(reg) = okto_core::Registry::load(path) {
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

        Command::Reload { agents: agent_targets, env, ready_timeout } => {
            info!("[cli] reload starting (agent_targets={}, env_pairs={}, ready_timeout={ready_timeout}s)", agent_targets.len(), env.len());
            if !env.is_empty() {
                let new_pairs = init::parse_extra_env(&env)?;
                let path = service::env_file_path();
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))
                    .unwrap_or_default();
                let mut entries = init::parse_env_file(&text);
                for (k, v) in new_pairs {
                    debug!("[cli] reload: upserting env key '{k}'");
                    if let Some(slot) = entries.iter_mut().find(|(ek, _)| ek == &k) {
                        slot.1 = v;
                    } else {
                        entries.push((k, v));
                    }
                }
                init::write_secret_file(&path, &init::serialize_env_file(&entries))?;
            }
            init::restart_lair("reload", std::time::Duration::from_secs(ready_timeout)).await?;

            let names: Vec<String> = if agent_targets.is_empty() {
                let path = service::lair_data_dir().join("agents.json");
                match okto_core::Registry::load(path) {
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
            generate(shell, &mut Cli::command(), "okto", &mut std::io::stdout());
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
                    let cfg = okto_core::read_config();
                    println!("anthropic_api_key:    {}", cfg.anthropic_api_key.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                    println!("openai_api_key:       {}", cfg.openai_api_key.as_deref().map(mask).unwrap_or_else(|| "(not set)".into()));
                    println!("model:                {}", cfg.model.as_deref().unwrap_or("(default)"));
                    println!("api_url:              {}", cfg.api_url.as_deref().unwrap_or("(Anthropic)"));
                    println!("cost_input1M:         {}", cfg.cost_input_1m.map(|v| format!("${v}/1M")).unwrap_or_else(|| "(not set)".into()));
                    println!("cost_output1M:        {}", cfg.cost_output_1m.map(|v| format!("${v}/1M")).unwrap_or_else(|| "(not set)".into()));
                    match cfg.system_prompt_append.as_deref() {
                        Some(text) => {
                            let preview: String = text.chars().take(80).collect();
                            let suffix = if text.chars().count() > 80 { "…" } else { "" };
                            println!("system_prompt_append: {preview}{suffix}");
                        }
                        None => println!("system_prompt_append: (not set)"),
                    }
                }
                ConfigAction::Set { model, api_url, anthropic_api_key, openai_api_key, system_prompt_append, cost_input1m, cost_output1m } => {
                    let mut cfg = okto_core::read_config();
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
                    if let Some(raw) = system_prompt_append {
                        debug!("[cli] config set: updating system_prompt_append");
                        cfg.system_prompt_append = resolve_text_or_at_path(&raw)?;
                    }
                    // A negative value clears the field (no natural "unset" for a
                    // numeric flag, and a negative price is never valid anyway).
                    if let Some(v) = cost_input1m {
                        debug!("[cli] config set: updating cost_input1M");
                        cfg.cost_input_1m = (v >= 0.0).then_some(v);
                    }
                    if let Some(v) = cost_output1m {
                        debug!("[cli] config set: updating cost_output1M");
                        cfg.cost_output_1m = (v >= 0.0).then_some(v);
                    }
                    okto_core::write_config(&cfg);
                    info!("[cli] config updated at {}", okto_core::config_path().display());
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
                        println!("(no operator env vars set — use `okto env set KEY=VALUE`)");
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
                    init::restart_lair("env set", std::time::Duration::from_secs(service::DEFAULT_READY_TIMEOUT_SECS)).await?;
                }
                EnvAction::Unset { keys } => {
                    if keys.is_empty() {
                        anyhow::bail!("no keys supplied");
                    }
                    for k in &keys {
                        if init::MANAGED_ENV_KEYS.contains(&k.as_str()) {
                            anyhow::bail!("'{k}' is managed by okto and can't be unset");
                        }
                    }
                    let before = entries.len();
                    entries.retain(|(k, _)| !keys.contains(k));
                    if entries.len() == before {
                        println!("No matching keys to remove.");
                    } else {
                        debug!("[cli] env unset: removed {} key(s); writing {}", before - entries.len(), path.display());
                        init::write_secret_file(&path, &init::serialize_env_file(&entries))?;
                        init::restart_lair("env unset", std::time::Duration::from_secs(service::DEFAULT_READY_TIMEOUT_SECS)).await?;
                    }
                }
            }
        }
        Command::Ssh { action } => match action {
            SshAction::Pubkey => ssh::pubkey().await?,
        },
        Command::Tasks { action } => match action {
            TasksAction::List { agent }    => tasks::list(agent.as_deref()).await?,
            TasksAction::Stop { id, agent } => tasks::stop(&id, agent.as_deref()).await?,
        },
    }
    Ok(())
}
