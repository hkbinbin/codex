//! End-to-end tests for the `wss://` transport with self-signed certificate
//! pinning. These verify that the encrypted channel works without a reverse
//! proxy: the server generates a self-signed cert and prints its fingerprint,
//! and the client connects only when it pins the matching fingerprint.

mod common;

use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use codex_exec_server::ExecServerClient;
use codex_exec_server::RemoteExecServerConnectArgs;
use codex_exec_server::parse_fingerprint_hex;
use common::exec_server::spawn_exec_server_wss;

const TEST_TOKEN: &str = "wss-smoke-token";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

fn connect_args(
    websocket_url: String,
    auth_token: Option<String>,
    tls_pinned_sha256: Option<[u8; 32]>,
) -> RemoteExecServerConnectArgs {
    RemoteExecServerConnectArgs {
        websocket_url,
        client_name: "wss-pinning-test".to_string(),
        connect_timeout: CONNECT_TIMEOUT,
        initialize_timeout: INITIALIZE_TIMEOUT,
        resume_session_id: None,
        auth_token,
        tls_pinned_sha256,
    }
}

/// A client that pins the server's actual certificate fingerprint and presents
/// the correct bearer token connects successfully over `wss://` and can issue
/// an RPC (proving the encrypted JSON-RPC channel works end to end).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_client_with_matching_fingerprint_connects() -> Result<()> {
    let mut server = spawn_exec_server_wss([("CODEX_EXEC_SERVER_AUTH_TOKEN", TEST_TOKEN)]).await?;
    let fingerprint = parse_fingerprint_hex(&server.fingerprint_hex)?;

    let client = ExecServerClient::connect_websocket(connect_args(
        server.websocket_url.clone(),
        Some(TEST_TOKEN.to_string()),
        Some(fingerprint),
    ))
    .await?;

    // A successful RPC confirms the TLS + token + JSON-RPC path is healthy.
    let _info = client.environment_info().await?;

    server.child.start_kill()?;
    Ok(())
}

/// A client that pins the wrong fingerprint is rejected during the TLS
/// handshake, even with a valid token, proving certificate pinning prevents
/// connecting to an unexpected server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_client_with_wrong_fingerprint_is_rejected() -> Result<()> {
    let mut server = spawn_exec_server_wss([("CODEX_EXEC_SERVER_AUTH_TOKEN", TEST_TOKEN)]).await?;

    // Flip one byte of the real fingerprint to simulate a mismatched pin.
    let mut wrong = parse_fingerprint_hex(&server.fingerprint_hex)?;
    wrong[0] ^= 0xff;

    let result = ExecServerClient::connect_websocket(connect_args(
        server.websocket_url.clone(),
        Some(TEST_TOKEN.to_string()),
        Some(wrong),
    ))
    .await;

    match result {
        Ok(_) => {
            return Err(anyhow!(
                "connection with mismatched fingerprint should fail"
            ));
        }
        Err(_) => {}
    }

    server.child.start_kill()?;
    Ok(())
}

/// A client that pins the correct fingerprint but omits the bearer token is
/// rejected at the application layer (401), confirming TLS and token auth are
/// independent and both enforced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_client_without_token_is_rejected() -> Result<()> {
    let mut server = spawn_exec_server_wss([("CODEX_EXEC_SERVER_AUTH_TOKEN", TEST_TOKEN)]).await?;
    let fingerprint = parse_fingerprint_hex(&server.fingerprint_hex)?;

    let result = ExecServerClient::connect_websocket(connect_args(
        server.websocket_url.clone(),
        None,
        Some(fingerprint),
    ))
    .await;

    match result {
        Ok(_) => return Err(anyhow!("connection without token should be rejected")),
        Err(_) => {}
    }

    server.child.start_kill()?;
    Ok(())
}
