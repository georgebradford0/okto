//! `okto reload --check-config` — validate the effective config and ping the
//! configured API. The static-validation paths need no network; the ping paths
//! point `api_url` at a `MockMgmt` server (the OpenAI-compatible backend) so the
//! HTTP round-trip is exercised end-to-end without a real provider or docker.

mod common;

use common::{MockMgmt, OktoCli};
use serde_json::json;

#[tokio::test]
async fn fails_when_no_api_key_is_configured() {
    let cli = OktoCli::new();
    let out = cli.run(&["reload", "--check-config"]).await;
    out.assert_err();
    assert!(out.stdout.contains("Config values ... invalid"), "{}", out.stdout);
    assert!(
        out.stderr.contains("anthropic_api_key or openai_api_key is required"),
        "stderr: {}",
        out.stderr,
    );
}

#[tokio::test]
async fn fails_when_model_is_missing() {
    let cli = OktoCli::new();
    cli.write(".okto/config.json", r#"{"anthropic_api_key":"sk-ant-abcdefgh12345678"}"#);
    let out = cli.run(&["reload", "--check-config"]).await;
    out.assert_err();
    assert!(out.stderr.contains("model is required"), "stderr: {}", out.stderr);
}

#[tokio::test]
async fn pings_openai_backend_and_reports_success() {
    let mock = MockMgmt::start().await; // replies 200 {} to everything
    let cli = OktoCli::new();
    cli.write(
        ".okto/config.json",
        &json!({
            "openai_api_key": "sk-test-key-1234",
            "model": "gpt-test",
            "api_url": format!("http://127.0.0.1:{}/v1/chat/completions", mock.port),
        })
        .to_string(),
    );

    let out = cli.run(&["reload", "--check-config"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("Config values ... ok"), "{}", out.stdout);
    assert!(out.stdout.contains("Configuration looks good."), "{}", out.stdout);

    // The ping actually hit the backend with a POST.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "expected exactly one ping request: {reqs:?}");
    assert_eq!(reqs[0].method, "POST", "{reqs:?}");
    assert!(reqs[0].path.contains("/v1/chat/completions"), "{reqs:?}");
}

#[tokio::test]
async fn reports_api_error_from_the_ping() {
    let mock = MockMgmt::start_with(401, json!({"error": "bad key"})).await;
    let cli = OktoCli::new();
    cli.write(
        ".okto/config.json",
        &json!({
            "openai_api_key": "sk-test-key-1234",
            "model": "gpt-test",
            "api_url": format!("http://127.0.0.1:{}/v1/chat/completions", mock.port),
        })
        .to_string(),
    );

    let out = cli.run(&["reload", "--check-config"]).await;
    out.assert_err();
    // Config values pass; the failure surfaces from the ping with the status.
    assert!(out.stdout.contains("Config values ... ok"), "{}", out.stdout);
    assert!(out.stderr.contains("API returned 401"), "stderr: {}", out.stderr);
}

#[tokio::test]
async fn lair_env_overrides_config_json_for_the_ping() {
    // config.json has no api_url (would default to Anthropic), but the lair-env
    // overlay sets OPENAI_API_URL + OPENAI_API_KEY — exactly how an operator who
    // configured the backend via `okto env` would have it. The check must use
    // the env override and ping the OpenAI-compatible mock.
    let mock = MockMgmt::start().await;
    let cli = OktoCli::new();
    cli.write(".okto/config.json", r#"{"model":"gpt-test"}"#);
    cli.write(
        ".okto/lair-env",
        &format!(
            "OPENAI_API_KEY=sk-from-env-9999\nOPENAI_API_URL=http://127.0.0.1:{}/v1/chat/completions\n",
            mock.port,
        ),
    );

    let out = cli.run(&["reload", "--check-config"]).await;
    out.assert_ok();
    assert!(out.stdout.contains("OpenAI-compatible"), "{}", out.stdout);
    assert_eq!(mock.requests().len(), 1, "{:?}", mock.requests());
}
