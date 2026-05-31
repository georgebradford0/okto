//! `okto qr` and `okto ssh pubkey` — both reconstruct output from files under
//! `~/.okto`. `qr` is kept offline by setting `OKTO_DEV=1` in `lair-env`, which
//! short-circuits public-IP detection to `127.0.0.1`.

mod common;

use common::OktoCli;

#[tokio::test]
async fn qr_renders_a_connect_string_in_dev_mode() {
    let cli = OktoCli::new();
    // 64-byte Noise keypair file (priv || pub); qr only reads the last 32.
    cli.write_bytes(".okto/lair/noise_key.bin", &[7u8; 64]);
    cli.write_launch(8443, 8000);
    // OKTO_DEV=1 makes host resolution return loopback without any network.
    cli.write(".okto/lair-env", "OKTO_DEV=1\n");

    let out = cli.run(&["qr"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("Connect string: 2:127.0.0.1:8443:"),
        "expected a loopback connect string, got: {}",
        out.stdout,
    );
}

#[tokio::test]
async fn qr_honours_an_explicit_host_override() {
    let cli = OktoCli::new();
    cli.write_bytes(".okto/lair/noise_key.bin", &[1u8; 64]);
    cli.write_launch(9001, 8000);

    let out = cli.run(&["qr", "--host", "example.test"]).await;
    out.assert_ok();
    assert!(
        out.stdout.contains("Connect string: 2:example.test:9001:"),
        "expected the overridden host + port, got: {}",
        out.stdout,
    );
}

#[tokio::test]
async fn qr_without_a_noise_key_errors_clearly() {
    let cli = OktoCli::new();
    let out = cli.run(&["qr", "--host", "example.test"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("okto init") || out.stderr.contains("noise_key.bin"),
        "expected a guidance error pointing at init, got: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn qr_rejects_a_corrupt_noise_key() {
    let cli = OktoCli::new();
    // Wrong length → qr should refuse rather than emit a bogus pubkey.
    cli.write_bytes(".okto/lair/noise_key.bin", &[0u8; 10]);
    cli.write_launch(8443, 8000);

    let out = cli.run(&["qr", "--host", "h"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("64") || out.stderr.to_lowercase().contains("corrupt"),
        "expected a corrupt-key error, got: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn ssh_pubkey_prints_the_container_key() {
    let cli = OktoCli::new();
    let pubkey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITESTKEY okto-container\n";
    cli.write(".okto/.ssh/id_ed25519.pub", pubkey);

    let out = cli.run(&["ssh", "pubkey"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("ssh-ed25519"), "{}", out.stdout);
    assert!(out.stdout.contains("TESTKEY"), "{}", out.stdout);
}

#[tokio::test]
async fn ssh_pubkey_without_a_key_errors() {
    let cli = OktoCli::new();
    let out = cli.run(&["ssh", "pubkey"]).await;
    out.assert_err();
    assert!(
        out.stderr.contains("okto init") || out.stderr.contains("id_ed25519.pub"),
        "expected a missing-key error, got: {}",
        out.stderr,
    );
}
