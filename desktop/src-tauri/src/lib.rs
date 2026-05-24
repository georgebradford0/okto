//! Tauri commands exposed to the renderer.
//!
//! `noise_connect` wraps `octo_core::noise::open_noise_tunnel`: it performs the
//! Noise XX handshake against lair on the operator's host and binds an
//! ephemeral loopback TCP port that proxies one connection through the
//! encrypted tunnel. The renderer then `new WebSocket('ws://127.0.0.1:<port>/stream')`s
//! against that port — the browser speaks plaintext to loopback, the Rust side
//! encrypts on the wire.
//!
//! Mirrors `mobile/src/NativeNoiseConnection.ts`. The desktop client's static
//! Curve25519 identity lives at `<app_data_dir>/noise_key.bin` and is reused
//! across launches.

use std::sync::Mutex;

use tauri::{AppHandle, Manager, State};

struct AppState {
    /// Loopback port of the most recently opened Noise tunnel, if any. The
    /// renderer reads it back via `noise_active_port` after a hot reload to
    /// re-anchor its WebSocket without re-running the handshake.
    active_port: Mutex<Option<u16>>,
}

#[tauri::command]
async fn noise_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
    port: u16,
    server_pubkey_b32: String,
) -> Result<u16, String> {
    let expected_pubkey = octo_core::from_base32(&server_pubkey_b32)
        .ok_or_else(|| "server pubkey is not valid base32".to_string())?;
    if expected_pubkey.len() != 32 {
        return Err(format!(
            "server pubkey decoded to {} bytes, expected 32",
            expected_pubkey.len(),
        ));
    }

    let key_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?;
    std::fs::create_dir_all(&key_dir)
        .map_err(|e| format!("create app data dir {}: {e}", key_dir.display()))?;
    let key_path = key_dir.join("noise_key.bin");
    let (static_private, _static_public) =
        octo_core::load_or_generate_keypair(&key_path.to_string_lossy());

    let local_port = octo_core::open_noise_tunnel(host, port, expected_pubkey, static_private)
        .await
        .map_err(|e| format!("open noise tunnel: {e}"))?;

    *state.active_port.lock().unwrap() = Some(local_port);
    Ok(local_port)
}

#[tauri::command]
fn noise_active_port(state: State<'_, AppState>) -> Option<u16> {
    *state.active_port.lock().unwrap()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            active_port: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![noise_connect, noise_active_port])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
