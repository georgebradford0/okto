//! Desktop (Tauri) e2e — the renderer's transport path.
//!
//! The desktop client never speaks Noise from JS. Instead `desktop_lib::
//! noise_connect` (desktop/src-tauri/src/lib.rs) composes three `okto_core`
//! primitives and hands the renderer a plaintext loopback port:
//!
//! ```ignore
//! let pk   = okto_core::from_base32(server_pubkey_b32)?;          // QR pubkey
//! let priv = okto_core::load_or_generate_keypair(key_path);       // device id
//! let port = okto_core::open_noise_proxy(host, port, pk, priv)?;  // the tunnel
//! // renderer: new WebSocket(`ws://127.0.0.1:${port}/stream`)
//! ```
//!
//! These tests exercise that exact composition end-to-end against a real lair
//! + mock LLM: open the proxy the way the Tauri command does, then drive a chat
//! turn over a *plaintext* loopback WebSocket exactly like the renderer's
//! `new WebSocket(...)` does. The Noise leg is real; only the AppHandle wrapper
//! (which just resolves the key path) is omitted.

mod common;

use std::time::Duration;

use common::{LairProcess, Turn};
use futures_util::{SinkExt, StreamExt};
use okto_core::noise::DEV_PUBKEY_BASE32;
use okto_core::{from_base32, load_or_generate_keypair, open_noise_proxy};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// Mirror `noise_connect`: decode the QR pubkey, load/generate this device's
/// static key, and open the loopback Noise proxy to lair. Returns the bound
/// plaintext port the renderer would point its WebSocket at.
async fn open_desktop_proxy(lair: &LairProcess, key_dir: &std::path::Path) -> u16 {
    let expected_pubkey = from_base32(DEV_PUBKEY_BASE32).expect("dev pubkey is valid base32");
    assert_eq!(expected_pubkey.len(), 32, "decoded pubkey must be 32 bytes");

    let key_path = key_dir.join("noise_key.bin");
    let (static_private, _static_public) =
        load_or_generate_keypair(&key_path.to_string_lossy());

    open_noise_proxy(
        "127.0.0.1".to_string(),
        lair.noise_port,
        expected_pubkey,
        static_private,
    )
    .await
    .expect("open_noise_proxy should bind a loopback port")
}

/// Read the next JSON text frame from the renderer-style WebSocket, answering
/// pings, bounded so a stalled stream errors instead of hanging.
async fn next_event<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Option<Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let fut = async {
        loop {
            match ws.next().await {
                None => return None,
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).expect("event is JSON");
                    if v.get("type").and_then(|x| x.as_str()) == Some("ping") {
                        let id = v.get("id").cloned().unwrap_or(json!(0));
                        let _ = ws
                            .send(Message::Text(json!({"type":"pong","id":id}).to_string()))
                            .await;
                        continue;
                    }
                    return Some(v);
                }
                Some(Ok(Message::Close(_))) => return None,
                Some(Ok(_)) => continue,
                Some(Err(e)) => panic!("websocket error: {e}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .expect("timed out waiting for a chat event")
}

#[tokio::test]
async fn desktop_proxy_streams_a_chat_turn() {
    // lair will answer one user turn with plain assistant text.
    let lair = LairProcess::start(vec![Turn::text("Hello from the desktop tunnel")])
        .await
        .expect("lair to start");

    let key_dir = tempfile::tempdir().expect("temp key dir");
    let port = open_desktop_proxy(&lair, key_dir.path()).await;

    // Exactly what the renderer does: a plaintext WebSocket to the loopback
    // port the Tauri command returned.
    let url = format!("ws://127.0.0.1:{port}/stream");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .unwrap_or_else(|e| panic!("renderer WS connect to {url}: {e}\n--- lair.log ---\n{}", lair.log()));

    // The server greets every /stream open with a `ready` frame.
    let ready = next_event(&mut ws).await.expect("a ready frame");
    assert_eq!(
        ready.get("type").and_then(|x| x.as_str()),
        Some("ready"),
        "first frame should be ready, got {ready}"
    );

    // Drive a turn and collect until the terminator.
    ws.send(Message::Text(
        json!({"type":"user_message","text":"hi"}).to_string(),
    ))
    .await
    .expect("send user_message");

    let mut saw_text = String::new();
    let mut saw_done = false;
    while let Some(ev) = next_event(&mut ws).await {
        match ev.get("type").and_then(|x| x.as_str()) {
            Some("text") => {
                saw_text.push_str(ev.get("text").and_then(|x| x.as_str()).unwrap_or(""))
            }
            Some("done") => {
                saw_done = true;
                break;
            }
            Some("error") => panic!("turn errored: {ev}"),
            _ => {}
        }
    }

    assert!(saw_done, "turn should terminate with a done frame");
    assert!(
        saw_text.contains("Hello from the desktop tunnel"),
        "streamed text should carry the model's reply, got {saw_text:?}"
    );
}

#[tokio::test]
async fn desktop_proxy_persists_one_device_identity_across_calls() {
    // `load_or_generate_keypair` is what gives the desktop a stable device
    // identity across launches: the second open against the same key path must
    // reuse the key on disk, not mint a new one — and still tunnel cleanly.
    let lair = LairProcess::start(vec![Turn::text("ok")])
        .await
        .expect("lair to start");

    let key_dir = tempfile::tempdir().expect("temp key dir");
    let key_path = key_dir.path().join("noise_key.bin");

    let (priv1, pub1) = load_or_generate_keypair(&key_path.to_string_lossy());
    let (priv2, pub2) = load_or_generate_keypair(&key_path.to_string_lossy());
    assert_eq!(priv1, priv2, "device private key must persist across launches");
    assert_eq!(pub1, pub2, "device public key must persist across launches");

    // And the persisted identity still opens a working tunnel + WS.
    let port = open_desktop_proxy(&lair, key_dir.path()).await;
    let url = format!("ws://127.0.0.1:{port}/stream");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("renderer WS connect");
    let ready = next_event(&mut ws).await.expect("a ready frame");
    assert_eq!(ready.get("type").and_then(|x| x.as_str()), Some("ready"));
}
