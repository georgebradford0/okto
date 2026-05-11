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
    pub api_key:        &'a str,
    pub gh_token:       Option<&'a str>,
    pub noise_port:     u16,
    pub http_port:      u16,
    pub image:          &'a str,
    pub mcp_config:     Option<&'a Path>,
    pub model:          Option<&'a str>,
    pub api_url:        Option<&'a str>,
    pub openai_api_key: Option<&'a str>,
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

    let (noise_private_key_hex, pubkey_b32) = generate_keypair()?;

    // Persist the operator's config (model, API key, etc.) so subsequent
    // CLI invocations have a sensible default.
    let mut cfg = octo_core::read_config();
    cfg.anthropic_api_key = Some(opts.api_key.to_string());
    if opts.openai_api_key.is_some() { cfg.openai_api_key = opts.openai_api_key.map(str::to_string); }
    if opts.model.is_some()          { cfg.model          = opts.model.map(str::to_string); }
    if opts.api_url.is_some()        { cfg.api_url        = opts.api_url.map(str::to_string); }
    if opts.gh_token.is_some()       { cfg.gh_token       = opts.gh_token.map(str::to_string); }
    octo_core::write_config(&cfg);
    println!("Wrote operator config to {}.", octo_core::config_path().display());

    // Resolve gh_token: explicit flag wins; otherwise fall back to whatever was
    // persisted previously so repeat `octo init` doesn't quietly drop it.
    let gh_token = opts.gh_token.map(str::to_string).or_else(|| cfg.gh_token.clone());

    // Seed mcp.json if the operator provided one. `${VAR}` references are
    // expanded against the *host* env so the operator can use the same file
    // the agent will end up reading.
    if let Some(path) = opts.mcp_config {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read mcp config {}", path.display()))?;
        let mut servers: Vec<serde_json::Value> = serde_json::from_str(&text)
            .with_context(|| format!("parse mcp config {}: must be a JSON array", path.display()))?;
        for server in &mut servers {
            if let Some(env) = server.get_mut("env").and_then(|e| e.as_object_mut()) {
                for (_, val) in env.iter_mut() {
                    if let Some(s) = val.as_str() {
                        if s.starts_with("${") && s.ends_with('}') {
                            let var = &s[2..s.len() - 1];
                            match std::env::var(var) {
                                Ok(resolved) => *val = serde_json::Value::String(resolved),
                                Err(_) => eprintln!("warning: ${{{var}}} not set in local environment — storing unexpanded"),
                            }
                        }
                    }
                }
            }
        }
        let dest = lair_dir.join("mcp.json");
        fs::write(&dest, serde_json::to_string_pretty(&servers)?)
            .with_context(|| format!("write {}", dest.display()))?;
        println!("Seeded MCP config: {}", dest.display());
    }

    // Drop a copy of the Noise keypair into the bind-mounted dir so lair can
    // load it without an env var. Same 32-byte format `load_or_generate_keypair`
    // expects.
    let key_file = lair_dir.join("noise_key.bin");
    if !key_file.exists() {
        let private_bytes = hex::decode(&noise_private_key_hex[..64])
            .context("decode noise private key")?;
        fs::write(&key_file, &private_bytes)
            .with_context(|| format!("write {}", key_file.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&key_file)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&key_file, perms).ok();
        }
        println!("Wrote Noise keypair to {}.", key_file.display());
    }

    // Compose the env file fed to `docker run --env-file`.
    let env_path = dockerd::env_file_path();
    fs::create_dir_all(env_path.parent().unwrap()).ok();
    let env_text = build_env_file(&EnvFileInput {
        api_key:           opts.api_key,
        gh_token:          gh_token.as_deref(),
        model:             opts.model,
        api_url:           opts.api_url,
        openai_api_key:    opts.openai_api_key,
        noise_private_key: &noise_private_key_hex,
        public_port:       opts.noise_port,
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
    };
    println!("Starting lair container...");
    dockerd::start_lair(&docker, &launch).await?;

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

fn generate_keypair() -> Result<(String, String)> {
    println!("Generating Noise_XX_25519 keypair...");
    let builder = snow::Builder::new(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().context("parse noise params")?,
    );
    let kp = builder.generate_keypair().context("generate keypair")?;
    let mut combined = kp.private.clone();
    combined.extend_from_slice(&kp.public);
    Ok((hex::encode(&combined), BASE32_NOPAD.encode(&kp.public)))
}

pub struct EnvFileInput<'a> {
    pub api_key:           &'a str,
    pub gh_token:          Option<&'a str>,
    pub model:             Option<&'a str>,
    pub api_url:           Option<&'a str>,
    pub openai_api_key:    Option<&'a str>,
    pub noise_private_key: &'a str,
    pub public_port:       u16,
}

pub fn build_env_file(i: &EnvFileInput) -> String {
    let mut out = String::new();
    out.push_str(&format!("ANTHROPIC_API_KEY={}\n", i.api_key));
    out.push_str(&format!("NOISE_PRIVATE_KEY={}\n", i.noise_private_key));
    out.push_str(&format!("PUBLIC_PORT={}\n", i.public_port));
    out.push_str("NOISE_PORT=9000\n");
    out.push_str("OCTO_DATA_DIR=/data\n");
    out.push_str("NOISE_KEY_FILE=/data/noise_key.bin\n");
    out.push_str("OCTO_SKIP_SHELL_ENV=1\n");
    if let Some(v) = i.gh_token       { out.push_str(&format!("GH_TOKEN={v}\n")); }
    if let Some(v) = i.model          { out.push_str(&format!("MODEL={v}\n")); }
    if let Some(v) = i.api_url        { out.push_str(&format!("OPENAI_API_URL={v}\n")); }
    if let Some(v) = i.openai_api_key { out.push_str(&format!("OPENAI_API_KEY={v}\n")); }
    out
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
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
