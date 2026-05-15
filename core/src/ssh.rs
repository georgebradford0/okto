//! SSH keypair generation for operational backchannels (e.g. SSH-ing into a
//! remote-provisioned VM to tail logs). The key lives in the lair host's data
//! directory and is created once on first boot — both `octo init` and the lair
//! binary call `ensure_ssh_keypair` so existing installs pick one up without a
//! re-init.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rand::rngs::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use tracing::{debug, info};

/// File name of the Ed25519 private key inside the data directory.
pub const SSH_PRIVATE_KEY_FILE: &str = "ssh_id_ed25519";
/// File name of the matching OpenSSH-format public key.
pub const SSH_PUBLIC_KEY_FILE:  &str = "ssh_id_ed25519.pub";

/// Generate an Ed25519 SSH keypair inside `dir` if one doesn't already exist.
/// Returns `(private_path, public_path)`. Idempotent: existing keys are left
/// untouched. The private key is written `0o600` on Unix.
pub fn ensure_ssh_keypair(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let priv_path = dir.join(SSH_PRIVATE_KEY_FILE);
    let pub_path  = dir.join(SSH_PUBLIC_KEY_FILE);

    if priv_path.exists() && pub_path.exists() {
        debug!("[ssh] reusing existing keypair at {}", priv_path.display());
        return Ok((priv_path, pub_path));
    }

    info!("[ssh] generating new Ed25519 keypair in {}", dir.display());
    fs::create_dir_all(dir)
        .with_context(|| format!("create ssh key dir {}", dir.display()))?;

    let mut rng = OsRng;
    let private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .context("generate Ed25519 private key")?;
    let private_pem = private_key.to_openssh(LineEnding::LF)
        .context("encode private key as OpenSSH")?;
    let public_str  = private_key.public_key().to_openssh()
        .context("encode public key as OpenSSH")?;

    fs::write(&priv_path, private_pem.as_bytes())
        .with_context(|| format!("write {}", priv_path.display()))?;
    fs::write(&pub_path, format!("{public_str}\n").as_bytes())
        .with_context(|| format!("write {}", pub_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&priv_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&priv_path, perms)
            .with_context(|| format!("chmod 0600 {}", priv_path.display()))?;
    }

    info!("[ssh] wrote keypair: {} (0600) + {}", priv_path.display(), pub_path.display());
    Ok((priv_path, pub_path))
}
