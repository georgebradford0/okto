//! `octo init` — bootstrap a lair container on the local Linux host.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use data_encoding::BASE32_NOPAD;
use octo_core::ensure_ssh_keypair;
use tracing::{debug, error, info, warn};

use crate::service;

/// Read a single line from stdin, displaying `label` as the prompt. Trims
/// trailing newline + surrounding whitespace.
pub fn prompt(label: &str) -> Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).context("read from stdin")?;
    Ok(s.trim().to_string())
}

pub struct InitOptions<'a> {
    pub noise_port: u16,
    pub http_port:  u16,
    /// Pre-parsed + env-expanded contents to seed into `~/.octo/lair/mcp.json`,
    /// paired with the source path for log messages. `None` means no seeding.
    /// Validated up-front by `load_seed_mcp_config` so failures can't leave
    /// init in a half-finished state.
    pub mcp_seed:   Option<McpSeed>,
    pub extra_env:  &'a [(String, String)],
    /// Image reference to `docker run`. `None` falls back to
    /// `$OCTO_LAIR_IMAGE` or `service::DEFAULT_LAIR_IMAGE`.
    pub image:      Option<&'a str>,
}

pub struct McpSeed {
    pub source: PathBuf,
    pub json:   String,
}

/// Expand `"${VAR}"` against the operator's process env.
pub fn expand_host_env(v: &str) -> std::result::Result<String, String> {
    if !(v.starts_with("${") && v.ends_with('}')) {
        return Ok(v.to_string());
    }
    let var = &v[2..v.len() - 1];
    std::env::var(var).map_err(|_| var.to_string())
}

/// Parse a user-supplied mcp.json file, expand `"${VAR}"` against the
/// operator's process env, and return pretty-printed JSON ready to be
/// written to `~/.octo/lair/mcp.json`. Validation only — does not write
/// anything. Called before `octo init` writes `config.json` so a broken
/// mcp file can't leave the user in a half-init'd state.
pub fn load_seed_mcp_config(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read mcp config {}", path.display()))?;
    let mut servers: Vec<serde_json::Value> = serde_json::from_str(&text)
        .with_context(|| format!("parse mcp config {}: must be a JSON array", path.display()))?;

    let mut missing: Vec<String> = Vec::new();
    for server in &mut servers {
        for key in ["env", "headers"] {
            let Some(obj) = server.get_mut(key).and_then(|e| e.as_object_mut()) else { continue };
            for (_, val) in obj.iter_mut() {
                let Some(s) = val.as_str() else { continue };
                match expand_host_env(s) {
                    Ok(resolved) => *val = serde_json::Value::String(resolved),
                    Err(var)     => missing.push(var),
                }
            }
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        anyhow::bail!(
            "mcp config {} references env var(s) not visible to this process: {}. \
             Verify with `env | grep <NAME>` — variables defined in ~/.bashrc must \
             be `export`ed (not just assigned) to reach child processes. Otherwise \
             inline the values in the file.",
            path.display(),
            missing.join(", "),
        );
    }

    serde_json::to_string_pretty(&servers).context("re-serialize mcp config")
}

pub async fn run(opts: InitOptions<'_>) -> Result<()> {
    info!("[init] run starting");
    service::ensure_docker_present()?;

    let cfg_dir    = service::config_dir();
    let lair_dir   = service::lair_data_dir();
    let agents_dir = service::agents_dir();
    debug!("[init] creating directory {}", lair_dir.display());
    fs::create_dir_all(&lair_dir)
        .with_context(|| format!("create {}", lair_dir.display()))?;
    debug!("[init] creating directory {}", agents_dir.display());
    fs::create_dir_all(&agents_dir)
        .with_context(|| format!("create {}", agents_dir.display()))?;

    // SSH keypair for ops backchannels.
    match ensure_ssh_keypair(&lair_dir) {
        Ok((priv_path, pub_path)) => {
            debug!("[init] SSH keypair ready at {}", priv_path.display());
            println!("SSH keypair ready:");
            println!("  private: {}", priv_path.display());
            println!("  public:  {}", pub_path.display());
        }
        Err(e) => {
            warn!("[init] could not ensure SSH keypair: {e:#}");
            eprintln!("warning: could not ensure SSH keypair: {e:#}");
        }
    }

    println!("Operator config: {}.", octo_core::config_path().display());

    // Snapshot the prior mcp.json (if any) so we can restore it if MCP
    // startup ends up failing after we seed the new one. `seed_server_names`
    // is the list of names whose connect markers we'll scan for once lair is
    // healthy.
    let mcp_dest = lair_dir.join("mcp.json");
    let prior_mcp = fs::read_to_string(&mcp_dest).ok();
    let mut seed_server_names: Vec<String> = Vec::new();
    let mut seed_source_display: Option<String> = None;
    if let Some(seed) = opts.mcp_seed {
        seed_server_names = crate::mcp::server_names_from_json(&seed.json)?;
        seed_source_display = Some(seed.source.display().to_string());
        debug!(
            "[init] seeding {} into {} ({} server(s))",
            seed.source.display(), mcp_dest.display(), seed_server_names.len(),
        );
        write_secret_file(&mcp_dest, &seed.json)?;
        println!("Seeded MCP config from {} -> {}", seed.source.display(), mcp_dest.display());
    }

    // Ensure `<lair_dir>/noise_key.bin` (priv || pub, 64 bytes) exists.
    let key_file = lair_dir.join("noise_key.bin");
    let pubkey_b32 = ensure_noise_keypair(&key_file)?;

    // Compose the env file passed to the lair container via --env-file.
    // Additive: pre-existing entries are kept and `--env KEY=VAL` upserts on
    // top, so re-running `octo init -e GH_TOKEN=…` doesn't wipe other vars.
    // Use `octo env unset` or `octo destroy` to remove entries.
    let env_path = service::env_file_path();
    fs::create_dir_all(env_path.parent().unwrap()).ok();
    let existing_text = fs::read_to_string(&env_path).unwrap_or_default();
    let mut entries = parse_env_file(&existing_text);
    for (k, v) in opts.extra_env {
        if let Some(slot) = entries.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.clone();
        } else {
            entries.push((k.clone(), v.clone()));
        }
    }
    write_secret_file(&env_path, &serialize_env_file(&entries))?;
    debug!("[init] wrote env file {} ({} entries)", env_path.display(), entries.len());
    println!("Wrote env file: {} ({} entries)", env_path.display(), entries.len());

    let image = service::resolve_image(opts.image);
    info!("[init] pulling lair image {image}");
    println!("Pulling lair image: {image}");
    service::docker_pull(&image)?;

    let launch = service::LairLaunch {
        noise_port: opts.noise_port,
        http_port:  opts.http_port,
        config_dir: &cfg_dir,
        env_file:   &env_path,
        image:      &image,
    };
    println!("Starting lair...");
    let id = service::start_lair(&launch)?;
    println!("lair container: {}", id.chars().take(12).collect::<String>());

    service::write_launch(&service::LaunchRecord {
        noise_port: opts.noise_port,
        http_port:  opts.http_port,
        image:      Some(image.clone()),
    })?;

    println!("Waiting for lair to be ready...");
    service::wait_for_health(opts.http_port, std::time::Duration::from_secs(60)).await?;
    info!("[init] lair is healthy on http port {}", opts.http_port);

    // If we seeded an mcp.json, verify every server actually connected
    // inside lair before declaring init successful. `init_mcp_pool` is
    // awaited synchronously before the HTTP server binds, so by the time
    // /health passes the markers are already in `docker logs`. We still use
    // a small polling window in case the docker log relay is briefly behind.
    if !seed_server_names.is_empty() {
        println!("Verifying MCP server(s) started in lair...");
        let results = crate::mcp::wait_for_mcp_markers(crate::mcp::WaitOpts {
            agent:    crate::mcp::LAIR_AGENT_NAME,
            names:    &seed_server_names,
            timeout:  std::time::Duration::from_secs(10),
            since:    String::new(),
        }).await;
        let failures: Vec<String> = seed_server_names.iter()
            .filter(|n| !results.get(*n).map(|m| m.is_success()).unwrap_or(false))
            .cloned()
            .collect();
        if !failures.is_empty() {
            error!("[init] {} MCP server(s) failed to start: {}", failures.len(), failures.join(", "));
            eprintln!("\nMCP startup failures detected during init:");
            eprint!("{}", crate::mcp::format_marker_report(&results, &seed_server_names));

            // Roll back the entire init: stop the just-started container,
            // forget the launch record, and restore mcp.json to whatever
            // the operator had before (or delete it if there was nothing
            // before). Image is left in the local cache — pulls are cheap
            // on re-run and the operator may just need to fix mcp.json.
            warn!("[init] rolling back init due to MCP startup failures");
            eprintln!("\nRolling back init:");
            eprintln!("  - stopping lair container '{}'", service::LAIR_CONTAINER_NAME);
            service::stop_lair_if_running();

            let launch_path = service::launch_config_path();
            if launch_path.exists() {
                debug!("[init] rollback: removing launch record {}", launch_path.display());
                eprintln!("  - removing {}", launch_path.display());
                let _ = fs::remove_file(&launch_path);
            }

            match prior_mcp {
                Some(text) => {
                    debug!("[init] rollback: restoring previous {}", mcp_dest.display());
                    eprintln!("  - restoring previous {}", mcp_dest.display());
                    write_secret_file(&mcp_dest, &text)?;
                }
                None => {
                    debug!("[init] rollback: removing seeded {}", mcp_dest.display());
                    eprintln!("  - removing seeded {}", mcp_dest.display());
                    let _ = fs::remove_file(&mcp_dest);
                }
            }

            let source = seed_source_display.as_deref().unwrap_or("<mcp config>");
            anyhow::bail!(
                "init aborted — {} MCP server(s) from {} failed to start: {}. \
                 Fix the entries (ensure their commands exist inside the lair image, \
                 or switch to HTTP transport) and re-run `octo init`.",
                failures.len(),
                source,
                failures.join(", "),
            );
        }
        println!("All MCP server(s) connected:");
        print!("{}", crate::mcp::format_marker_report(&results, &seed_server_names));
    }

    let ip = match service::detect_public_ip().await {
        Ok(ip) => {
            debug!("[init] detected public IP {ip}");
            ip
        }
        Err(e) => {
            warn!("[init] could not detect public IP ({e:#}); falling back to 127.0.0.1");
            eprintln!("warning: could not detect public IP ({e:#}). Falling back to 127.0.0.1.");
            "127.0.0.1".to_string()
        }
    };
    let qr_data = format!("2:{ip}:{}:{pubkey_b32}", opts.noise_port);
    println!("\nlair is live at {ip}:{}\n", opts.noise_port);
    println!("QR data: {qr_data}\n");

    let code = qrcode::QrCode::new(&qr_data).context("generate QR code")?;
    let image = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .build();
    println!("{image}");

    info!("[init] run complete; lair live at {ip}:{}", opts.noise_port);
    Ok(())
}

fn ensure_noise_keypair(path: &Path) -> Result<String> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() == 64 {
            warn!("[init] reusing existing Noise keypair at {}", path.display());
            println!("Reusing existing Noise keypair at {}.", path.display());
            return Ok(BASE32_NOPAD.encode(&bytes[32..]));
        }
        warn!(
            "[init] {} is {} bytes (expected 64); regenerating Noise keypair",
            path.display(), bytes.len(),
        );
        eprintln!(
            "warning: {} is {} bytes (expected 64) — regenerating Noise keypair.",
            path.display(),
            bytes.len(),
        );
    }
    debug!("[init] generating new Noise_XX_25519 keypair");
    println!("Generating Noise_XX_25519 keypair...");
    let builder = snow::Builder::new(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().context("parse noise params")?,
    );
    let kp = builder.generate_keypair().context("generate keypair")?;
    let mut combined = kp.private.clone();
    combined.extend_from_slice(&kp.public);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(path, &combined)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms).ok();
    }
    debug!("[init] wrote Noise keypair (64 bytes, chmod 0600) to {}", path.display());
    println!("Wrote Noise keypair to {}.", path.display());
    Ok(BASE32_NOPAD.encode(&kp.public))
}

/// Env keys octo manages itself (baked into the image or set by
/// `service::start_lair`). The `octo env` subcommand refuses to add or
/// remove these.
pub const MANAGED_ENV_KEYS: &[&str] = &[
    "NOISE_PORT", "PUBLIC_PORT",
    "OCTO_HOME", "OCTO_DATA_DIR", "OCTO_AGENTS_DIR",
    "OCTO_SKIP_SHELL_ENV", "OCTO_LAIR_BINARY",
    "HOME",
];

pub fn parse_extra_env(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for pair in raw {
        let (k, v) = pair.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("'{pair}' must be KEY=VALUE")
        })?;
        let k = k.trim();
        if k.is_empty() {
            anyhow::bail!("'{pair}': empty KEY");
        }
        let first = k.chars().next().unwrap();
        if !(first.is_ascii_alphabetic() || first == '_') {
            anyhow::bail!("'{pair}': KEY must start with letter or underscore");
        }
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            anyhow::bail!("'{pair}': KEY may only contain letters, digits, and underscores");
        }
        if MANAGED_ENV_KEYS.contains(&k) {
            anyhow::bail!("'{k}': reserved name managed by octo");
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

pub fn serialize_env_file(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in entries {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// Stop + respawn the lair container with the persisted launch record. Used
/// by `octo reload` and `octo env set/unset`.
pub async fn restart_lair(reason: &str) -> Result<()> {
    info!("[init] restarting lair (reason: {reason})");
    let rec = service::read_launch().ok_or_else(|| {
        error!("[init] cannot restart lair: ~/.octo/lair-launch.json is missing");
        anyhow::anyhow!(
            "~/.octo/lair-launch.json is missing. Re-run `octo init` once to record \
             launch parameters; subsequent `{reason}` calls won't need flags."
        )
    })?;
    let cfg_dir = service::config_dir();
    let env_path = service::env_file_path();
    let image = service::resolve_image(rec.image.as_deref());
    let launch = service::LairLaunch {
        noise_port: rec.noise_port,
        http_port:  rec.http_port,
        config_dir: &cfg_dir,
        env_file:   &env_path,
        image:      &image,
    };
    println!("Restarting lair ({reason})...");
    service::start_lair(&launch)?;
    println!("Waiting for lair to be ready...");
    service::wait_for_health(rec.http_port, std::time::Duration::from_secs(60)).await?;
    info!("[init] lair restart complete (reason: {reason})");
    println!("lair ready.");
    Ok(())
}

pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    debug!("[init] writing secret file {} (chmod 0600, {} bytes)", path.display(), contents.len());
    fs::write(path, contents)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms).ok();
    }
    Ok(())
}
