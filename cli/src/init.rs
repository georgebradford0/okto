//! `octo init` — bootstrap a lair Docker container on the local host.

use std::{
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use data_encoding::BASE32_NOPAD;
use octo_core::{ensure_ssh_keypair, Config};

use crate::dockerd;

pub struct InitOptions<'a> {
    pub noise_port: u16,
    pub http_port:  u16,
    pub image:      &'a str,
    pub mcp_config: Option<&'a Path>,
    /// Operator-supplied `KEY=VALUE` pairs forwarded into the lair container's
    /// process env via `--env-file`. Distinct from `config.json` credentials,
    /// which lair reads from the bind-mounted file at request time.
    pub extra_env:  &'a [(String, String)],
}

/// Expand a `"${VAR}"` reference against the operator's process env. Returns
/// the expanded value on success, or the unresolved variable name on miss.
/// Strings that don't fit the exact `${...}` form are returned unchanged.
pub fn expand_host_env(v: &str) -> std::result::Result<String, String> {
    if !(v.starts_with("${") && v.ends_with('}')) {
        return Ok(v.to_string());
    }
    let var = &v[2..v.len() - 1];
    std::env::var(var).map_err(|_| var.to_string())
}

pub async fn run(opts: InitOptions<'_>) -> Result<()> {
    let docker = dockerd::build_client()?;
    dockerd::ensure_docker_reachable(&docker).await?;

    let lair_dir = dockerd::lair_data_dir();
    fs::create_dir_all(&lair_dir)
        .with_context(|| format!("create {}", lair_dir.display()))?;

    // SSH keypair for ops backchannels (e.g. remote-VM agents — see CLAUDE.md).
    match ensure_ssh_keypair(&lair_dir) {
        Ok((priv_path, pub_path)) => {
            println!("SSH keypair ready:");
            println!("  private: {}", priv_path.display());
            println!("  public:  {}", pub_path.display());
        }
        Err(e) => eprintln!("warning: could not ensure SSH keypair: {e:#}"),
    }

    // main.rs::Command::Init merged flags onto cfg and called write_config
    // before invoking us — config.json is already on disk and will be
    // bind-mounted into /data/config.json when the lair container starts.
    println!("Operator config: {}.", octo_core::config_path().display());

    // Seed mcp.json if the operator provided one. `${VAR}` references in env
    // / headers values are expanded against the operator's process env at
    // write time so secrets get baked in before the file lands inside the
    // lair container (which can't see the host env). If any referenced var
    // is unset, surface every missing one and abort.
    if let Some(path) = opts.mcp_config {
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
                "mcp config {} references env var(s) not set in this shell: {}. \
                 Export them and re-run, or inline the values in the file.",
                path.display(),
                missing.join(", "),
            );
        }

        let dest = lair_dir.join("mcp.json");
        // mode 0600 because env values are now resolved literals — secret
        // material for MCP servers ends up plaintext in this file.
        write_secret_file(&dest, &serde_json::to_string_pretty(&servers)?)?;
        println!("Seeded MCP config: {}", dest.display());
    }

    // Ensure /data/noise_key.bin (priv || pub, 64 bytes) exists and pull the
    // pubkey that will go in the QR. Same format `load_or_generate_keypair`
    // expects, so lair doesn't silently regenerate at boot and break the QR.
    let key_file = lair_dir.join("noise_key.bin");
    let pubkey_b32 = ensure_noise_keypair(&key_file)?;

    // Compose the env file fed to `docker run --env-file`.
    let env_path = dockerd::env_file_path();
    fs::create_dir_all(env_path.parent().unwrap()).ok();
    let env_text = build_env_file(&EnvFileInput {
        public_port: opts.noise_port,
        extra_env:   opts.extra_env,
    });
    write_secret_file(&env_path, &env_text)?;
    println!("Wrote env file: {}", env_path.display());

    // Pull and (re)start lair.
    println!("Pulling image {}...", opts.image);
    dockerd::pull_image(&docker, opts.image).await?;

    let launch = dockerd::LairLaunch {
        image:           opts.image,
        host_noise_port: opts.noise_port,
        host_http_port:  opts.http_port,
        data_dir:        &lair_dir,
        env_file:        &env_path,
        docker_socket:   resolve_docker_socket(),
        operator_config: &octo_core::config_path(),
    };
    println!("Starting lair container...");
    dockerd::start_lair(&docker, &launch).await?;

    // Persist launch parameters so `octo reload` and `octo env set/unset` can
    // recreate lair with the same image + ports without re-prompting.
    dockerd::write_launch(&dockerd::LaunchRecord {
        image:           opts.image.to_string(),
        host_noise_port: opts.noise_port,
        host_http_port:  opts.http_port,
    })?;

    println!("Waiting for lair to be ready...");
    dockerd::wait_for_health(opts.http_port, std::time::Duration::from_secs(60)).await?;

    let ip = match dockerd::detect_public_ip().await {
        Ok(ip) => ip,
        Err(e) => {
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

    Ok(())
}

/// Ensure a 64-byte (priv || pub) Noise keypair exists at `path`, returning
/// the base32-encoded pubkey for the QR. If `path` already holds a valid
/// 64-byte keypair, reuse it so a re-run of `octo init` preserves whatever
/// pinned-key state mobile clients have cached. If it's a different length
/// (older CLIs only wrote the 32-byte private half, which lair silently
/// regenerated on boot and broke the QR), regenerate and overwrite.
fn ensure_noise_keypair(path: &Path) -> Result<String> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() == 64 {
            println!("Reusing existing Noise keypair at {}.", path.display());
            return Ok(BASE32_NOPAD.encode(&bytes[32..]));
        }
        eprintln!(
            "warning: {} is {} bytes (expected 64) — regenerating Noise keypair.",
            path.display(),
            bytes.len(),
        );
    }
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
    println!("Wrote Noise keypair to {}.", path.display());
    Ok(BASE32_NOPAD.encode(&kp.public))
}

pub struct EnvFileInput<'a> {
    pub public_port: u16,
    /// Operator-supplied extras appended after the managed runtime knobs.
    pub extra_env:   &'a [(String, String)],
}

/// Runtime-only env file for the lair container. Lair credentials (API keys,
/// `MODEL`, etc.) and the Noise private key are NOT written here — lair
/// picks those up from the bind-mounted `/data/config.json` and
/// `/data/noise_key.bin` respectively. Operator-supplied `--env KEY=VALUE`
/// extras are appended so `docker inspect lair` does surface them, by the
/// operator's choice.
pub fn build_env_file(i: &EnvFileInput) -> String {
    let mut out = String::new();
    out.push_str(&format!("PUBLIC_PORT={}\n", i.public_port));
    out.push_str("NOISE_PORT=9000\n");
    out.push_str("OCTO_DATA_DIR=/data\n");
    out.push_str("NOISE_KEY_FILE=/data/noise_key.bin\n");
    out.push_str("OCTO_SKIP_SHELL_ENV=1\n");
    for (k, v) in i.extra_env {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// Env keys octo manages itself (set by `build_env_file`). The `octo env`
/// subcommand refuses to add or remove these.
pub const MANAGED_ENV_KEYS: &[&str] = &[
    "NOISE_PORT", "PUBLIC_PORT", "OCTO_DATA_DIR",
    "NOISE_KEY_FILE", "OCTO_SKIP_SHELL_ENV",
];

/// Parse `--env KEY=VALUE` pairs. Used by both `octo init` and
/// `octo env set`. Rejects malformed pairs and reserved keys.
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

/// Parse an env-file body into an ordered list of (KEY, VALUE) pairs.
/// Blank lines and `#`-comments are skipped. Useful for round-tripping
/// `~/.octo/lair-env` from the env subcommand.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// Serialize (KEY, VALUE) pairs to env-file form. Order is preserved so
/// re-writing after an edit doesn't churn the file unnecessarily.
pub fn serialize_env_file(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in entries {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// Recreate the lair container (remove + create + start) using the launch
/// record persisted at `~/.octo/lair-launch.json`. Used by `octo env set/unset`
/// and `octo reload`, both of which change something `docker restart` doesn't
/// pick up (`--env-file` and the image, respectively).
pub async fn recreate_lair(reason: &str) -> Result<()> {
    let docker = dockerd::build_client()?;
    dockerd::ensure_docker_reachable(&docker).await?;

    let rec = dockerd::read_launch().ok_or_else(|| {
        anyhow::anyhow!(
            "~/.octo/lair-launch.json is missing. Re-run `octo init` once to record \
             launch parameters; subsequent `{reason}` calls won't need flags."
        )
    })?;

    let lair_dir = dockerd::lair_data_dir();
    let env_path = dockerd::env_file_path();
    let launch = dockerd::LairLaunch {
        image:           &rec.image,
        host_noise_port: rec.host_noise_port,
        host_http_port:  rec.host_http_port,
        data_dir:        &lair_dir,
        env_file:        &env_path,
        docker_socket:   resolve_docker_socket(),
        operator_config: &octo_core::config_path(),
    };
    println!("Recreating lair ({reason})...");
    dockerd::start_lair(&docker, &launch).await?;
    println!("Waiting for lair to be ready...");
    dockerd::wait_for_health(rec.host_http_port, std::time::Duration::from_secs(60)).await?;
    println!("lair ready.");
    Ok(())
}

pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
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

pub fn resolve_docker_socket() -> &'static str {
    "/var/run/docker.sock"
}

/// Hydrate `Config` from `~/.octo/config.json` or a `--config` override path.
pub fn load_config(explicit: Option<&Path>) -> Result<Config> {
    match explicit {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("config file not found: {}", p.display());
            }
            let text = fs::read_to_string(p)
                .with_context(|| format!("read {}", p.display()))?;
            serde_json::from_str::<Config>(&text)
                .with_context(|| format!("invalid JSON in {}", p.display()))
        }
        None => Ok(octo_core::read_config()),
    }
}
