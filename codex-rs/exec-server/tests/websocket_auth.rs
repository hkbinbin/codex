mod common;

use std::time::Duration;

use anyhow::anyhow;
use codex_exec_server::CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR;
use codex_exec_server_protocol::INITIALIZE_METHOD;
use codex_exec_server_protocol::INITIALIZED_METHOD;
use codex_exec_server_protocol::InitializeParams;
use codex_exec_server_protocol::InitializeResponse;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use common::exec_server::spawn_exec_server_url_only;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TEST_TOKEN: &str = "s3cr3t-smoke-token";
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

fn websocket_request_with_token(
    websocket_url: &str,
    token: Option<&str>,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = websocket_url
        .into_client_request()
        .expect("valid websocket request");
    if let Some(token) = token {
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .expect("valid header value"),
        );
    }
    request
}

/// A token-protected exec-server accepts a client that presents the matching
/// bearer token and completes the JSON-RPC initialize handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_accepts_connection_with_valid_bearer_token() -> anyhow::Result<()> {
    let (mut child, websocket_url, _codex_home, _helper_paths) =
        spawn_exec_server_url_only([(CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR, TEST_TOKEN)]).await?;

    let request = websocket_request_with_token(&websocket_url, Some(TEST_TOKEN));
    let (mut websocket, _response) = connect_async(request)
        .await
        .map_err(|err| anyhow!("authorized connection should succeed: {err}"))?;

    let initialize = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(serde_json::to_value(InitializeParams {
            client_name: "exec-server-auth-test".to_string(),
            resume_session_id: None,
        })?),
        trace: None,
    });
    websocket
        .send(Message::Text(serde_json::to_string(&initialize)?.into()))
        .await?;

    let frame = tokio::time::timeout(RECV_TIMEOUT, websocket.next())
        .await
        .map_err(|_| anyhow!("timed out waiting for initialize response"))?
        .ok_or_else(|| anyhow!("websocket closed before initialize response"))??;
    let text = match frame {
        Message::Text(text) => text.to_string(),
        other => return Err(anyhow!("expected text initialize response, got {other:?}")),
    };
    let response: JSONRPCMessage = serde_json::from_str(&text)?;
    let JSONRPCMessage::Response(JSONRPCResponse { id, result }) = response else {
        return Err(anyhow!("expected initialize response, got {response:?}"));
    };
    assert_eq!(id, RequestId::Integer(1));
    let initialize_response: InitializeResponse = serde_json::from_value(result)?;
    assert!(
        !initialize_response.session_id.is_empty(),
        "initialize should return a session id"
    );

    let initialized = JSONRPCMessage::Notification(JSONRPCNotification {
        method: INITIALIZED_METHOD.to_string(),
        params: Some(serde_json::to_value(())?),
    });
    websocket
        .send(Message::Text(serde_json::to_string(&initialized)?.into()))
        .await?;

    child.start_kill()?;
    Ok(())
}

/// A token-protected exec-server rejects a client that omits the bearer token
/// with HTTP 401 during the WebSocket handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_rejects_connection_without_token() -> anyhow::Result<()> {
    let (mut child, websocket_url, _codex_home, _helper_paths) =
        spawn_exec_server_url_only([(CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR, TEST_TOKEN)]).await?;

    let request = websocket_request_with_token(&websocket_url, None);
    let result = connect_async(request).await;
    assert_unauthorized(result);

    child.start_kill()?;
    Ok(())
}

/// A token-protected exec-server rejects a client that presents a non-matching
/// bearer token with HTTP 401 during the WebSocket handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_rejects_connection_with_invalid_token() -> anyhow::Result<()> {
    let (mut child, websocket_url, _codex_home, _helper_paths) =
        spawn_exec_server_url_only([(CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR, TEST_TOKEN)]).await?;

    let request = websocket_request_with_token(&websocket_url, Some("wrong-token"));
    let result = connect_async(request).await;
    assert_unauthorized(result);

    child.start_kill()?;
    Ok(())
}

fn assert_unauthorized(
    result: Result<
        (
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::handshake::client::Response,
        ),
        tokio_tungstenite::tungstenite::Error,
    >,
) {
    match result {
        Ok(_) => panic!("unauthenticated connection should be rejected"),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(
                response.status().as_u16(),
                401,
                "expected HTTP 401 Unauthorized"
            );
        }
        Err(other) => panic!("expected HTTP 401 error, got {other:?}"),
    }
}
