//! `okto ssh …` subcommands.
//!
//! The lair container holds **one** SSH keypair at `$HOME/.ssh/id_ed25519`
//! that every agent in the container uses. `okto ssh pubkey` prints that
//! public key so the operator can register it once on external services
//! (Prime Intellect, GitHub, GPU pods, etc.).

use anyhow::{Context, Result};
use tracing::debug;

use crate::service;

/// Print the container's SSH public key (one-line OpenSSH format). Reads
/// from `~/.okto/.ssh/id_ed25519.pub` on the host — the same file the
/// lair container sees at `/data/.ssh/id_ed25519.pub` via the bind mount.
pub async fn pubkey() -> Result<()> {
    let pub_path = okto_core::container_ssh_public_key(&service::config_dir());
    debug!("[ssh] reading pubkey from {}", pub_path.display());
    let text = std::fs::read_to_string(&pub_path)
        .with_context(|| format!(
            "read {} (run `okto init` first, or check that lair has started \
             and generated the container key)",
            pub_path.display(),
        ))?;
    print!("{}", text);
    if !text.ends_with('\n') { println!(); }
    Ok(())
}
