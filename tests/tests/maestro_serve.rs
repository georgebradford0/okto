//! A long-running, fully-offline lair for **Maestro e2e tests** on a simulator
//! or emulator.
//!
//! This is *not* an assertion test — it's a `#[ignore]`d entry point that boots
//! the real `lair` binary on a fixed port with the **dev keypair**, backed by
//! the same in-process `MockLlm` the rest of the e2e suite uses, then parks
//! until you kill it (Ctrl-C). The mobile app on a simulator/emulator connects
//! to it with a deterministic, known connection string — no API spend, no
//! network, no Docker.
//!
//! Run it:
//!
//! ```sh
//! cargo test -p okto-tests --test maestro_serve serve -- --ignored --nocapture
//! ```
//!
//! It prints the connection strings to paste/scan:
//!   - iOS Simulator:   `2:127.0.0.1:9000:<dev-pubkey>`
//!   - Android emulator:`2:10.0.2.2:9000:<dev-pubkey>`
//!
//! The pubkey is the fixed `DEV_PUBKEY_BASE32`, which is also hardcoded in
//! `mobile/App.tsx`, so the string never changes. lair binds its Noise proxy on
//! `0.0.0.0:9000`, reachable from the iOS Simulator via loopback and from the
//! Android emulator via its `10.0.2.2` host alias.
//!
//! Reuses `common::mock_llm::MockLlm` (canonical SSE shaping, exercised by the
//! rest of the suite) and `common::lair_proc::lair_binary` (builds the bin), so
//! there is no duplicated wire logic to drift.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::lair_proc::{free_port, lair_binary};
use common::mock_llm::{MockLlm, Turn};
use common::tunnel;

/// Fixed Noise port the connection strings below advertise. Matches lair's
/// default and the dev string baked into `mobile/App.tsx`.
const NOISE_PORT: u16 = 9000;

/// A handful of friendly scripted replies so the first few chat turns in a demo
/// read naturally; the mock falls back to `"ok"` once these are exhausted, so
/// every subsequent message still gets a reply.
fn demo_turns() -> Vec<Turn> {
    vec![
        Turn::text(
            "Hi! I'm a mock Okto agent running locally for end-to-end testing. \
             Ask me anything — every reply here is canned and fully offline.",
        ),
        Turn::text("Sure — that's a scripted response from the e2e mock LLM."),
        Turn::text("Still here. This lair has no real model behind it."),
    ]
}

#[tokio::test]
#[ignore = "long-running server for Maestro; run explicitly with --ignored"]
async fn serve() {
    let bin = lair_binary().await;
    let mock = MockLlm::start(demo_turns())
        .await
        .expect("start mock LLM");

    let tempdir = tempfile::tempdir().expect("tempdir");
    let home = tempdir.path().to_path_buf();
    let data_dir = home.join("lair");
    let agents_dir = home.join("agents");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&agents_dir).unwrap();

    let http_port = free_port();

    let mut child = tokio::process::Command::new(&bin)
        .arg("--role")
        .arg("lair")
        // Dev keypair → deterministic, app-hardcoded pubkey; offline host resolution.
        .env("OKTO_DEV", "1")
        .env("PUBLIC_HOST", "127.0.0.1")
        .env("HOME", &home)
        .env("OKTO_HOME", &home)
        .env("OKTO_DATA_DIR", &data_dir)
        .env("OKTO_AGENTS_DIR", &agents_dir)
        .env("NOISE_PORT", NOISE_PORT.to_string())
        .env("PUBLIC_PORT", NOISE_PORT.to_string())
        .env("OKTO_HTTP_PORT", http_port.to_string())
        .env("ANTHROPIC_API_URL", mock.url())
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("MODEL", "claude-test")
        // Advertise no relay (like `okto init --disable-push`): the mobile
        // client then skips push registration, so no iOS notification-permission
        // dialog pops up to block the Maestro flow.
        .env("OKTO_RELAY_URL", "")
        .env("RUST_LOG", "info")
        .env_remove("OPENAI_API_URL")
        .env_remove("OPENAI_API_KEY")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn lair");

    // Wait for /health over the Noise tunnel, exactly like the mobile client.
    let deadline = Duration::from_secs(30);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("lair exited early with {status}");
        }
        if let Ok((200, _)) = tunnel::http_get(NOISE_PORT, "/health").await {
            break;
        }
        if start.elapsed() > deadline {
            panic!("lair did not become ready within {deadline:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let pk = okto_core::DEV_PUBKEY_BASE32;
    eprintln!("\n========================================================================");
    eprintln!(" Mock lair is READY and serving on 0.0.0.0:{NOISE_PORT} (offline, no API spend).");
    eprintln!(" Paste/scan one of these connection strings in the app:");
    eprintln!();
    eprintln!("   iOS Simulator    →  2:127.0.0.1:{NOISE_PORT}:{pk}");
    eprintln!("   Android emulator →  2:10.0.2.2:{NOISE_PORT}:{pk}");
    eprintln!();
    eprintln!(" Press Ctrl-C to stop (lair is killed on exit).");
    eprintln!("========================================================================\n");

    // Park until killed; bail out if lair dies under us.
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("lair exited unexpectedly with {status}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
