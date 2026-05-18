//! `octo qr` — reconstruct and print the connection QR code mobile clients
//! scan to reach this host's lair.
//!
//! Lair prints the same QR at container boot (see `lair/src/bootstrap.rs`),
//! but it scrolls out of `docker logs` over time. This rebuilds it from the
//! persisted Noise keypair + launch record so the operator can reprint it on
//! demand. Wire format is identical: `2:<host>:<port>:<pubkey_base32>`.

use anyhow::{Context, Result};
use tracing::debug;

use crate::{init, service};

/// Build `2:<host>:<port>:<pubkey>` and render it to stdout as a QR code.
pub async fn print(host_override: Option<String>) -> Result<()> {
    // Noise pubkey: last 32 bytes of ~/.octo/lair/noise_key.bin. Read the file
    // directly rather than via `load_or_generate_keypair` so we never mint a
    // key the running lair doesn't have.
    let key_file = service::lair_data_dir().join("noise_key.bin");
    let bytes = std::fs::read(&key_file).with_context(|| {
        format!(
            "read {} — has lair been initialized? Run `octo init` first.",
            key_file.display(),
        )
    })?;
    anyhow::ensure!(
        bytes.len() == 64,
        "{} is {} bytes, expected 64 — the Noise keypair looks corrupt",
        key_file.display(),
        bytes.len(),
    );
    let pubkey_b32 = octo_core::to_base32(&bytes[32..]);

    // Port: the host-side Noise port from the last `octo init` / `octo reload`.
    let noise_port = service::read_launch()
        .map(|r| r.noise_port)
        .unwrap_or(service::LAIR_DEFAULT_NOISE_PORT);

    // Host: explicit override → PUBLIC_HOST in lair-env → OCTO_DEV loopback →
    // auto-detected public IP. Mirrors `bootstrap::resolve_public_host`.
    let host = match host_override.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(h) => h,
        None => resolve_host().await?,
    };

    let qr_data = format!("2:{host}:{noise_port}:{pubkey_b32}");
    debug!("[cli] qr data: {qr_data}");
    render(&qr_data)?;
    println!("Connect string: {qr_data}");
    Ok(())
}

/// Resolve the host the QR advertises, mirroring lair's own precedence.
async fn resolve_host() -> Result<String> {
    let env_path = service::env_file_path();
    if let Ok(text) = std::fs::read_to_string(&env_path) {
        let entries = init::parse_env_file(&text);
        if let Some((_, v)) = entries.iter().find(|(k, _)| k == "PUBLIC_HOST") {
            if !v.trim().is_empty() {
                return Ok(v.trim().to_string());
            }
        }
        if entries.iter().any(|(k, v)| k == "OCTO_DEV" && v.trim() == "1") {
            return Ok("127.0.0.1".to_string());
        }
    }
    let ip = service::detect_public_ip().await.context(
        "auto-detect public IP (set PUBLIC_HOST via `octo env set` to override)",
    )?;
    anyhow::ensure!(
        !ip.is_empty(),
        "public IP auto-detection returned an empty result; set PUBLIC_HOST via `octo env set`",
    );
    Ok(ip)
}

fn render(data: &str) -> Result<()> {
    let code = qrcode::QrCode::new(data).context("render QR code")?;
    let image = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .build();
    println!();
    println!("Scan this QR code with the octo app to connect:");
    println!();
    println!("{image}");
    println!();
    Ok(())
}
