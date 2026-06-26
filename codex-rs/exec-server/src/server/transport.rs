use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::header::ORIGIN;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::routing::get;
use std::io::Write as _;
use std::net::SocketAddr;
use tokio::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpListener;
use tracing::info;
use tracing::warn;

use crate::ExecServerRuntimePaths;
use crate::connection::JsonRpcConnection;
use crate::server::processor::ConnectionProcessor;

pub const DEFAULT_LISTEN_URL: &str = "ws://127.0.0.1:0";

/// Environment variable used to require a bearer token on incoming WebSocket
/// connections. When set to a non-empty value, every WebSocket upgrade request
/// must carry a matching `Authorization: Bearer <token>` header, otherwise the
/// server responds with `401 Unauthorized`. When unset/empty, no auth is
/// enforced (preserves the legacy local-only behavior).
pub const CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR: &str = "CODEX_EXEC_SERVER_AUTH_TOKEN";

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ExecServerListenTransport {
    WebSocket(SocketAddr),
    WebSocketTls(SocketAddr),
    Stdio,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ExecServerListenUrlParseError {
    UnsupportedListenUrl(String),
    InvalidWebSocketListenUrl(String),
}

impl std::fmt::Display for ExecServerListenUrlParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecServerListenUrlParseError::UnsupportedListenUrl(listen_url) => write!(
                f,
                "unsupported --listen URL `{listen_url}`; expected `ws://IP:PORT`, `wss://IP:PORT`, or `stdio`"
            ),
            ExecServerListenUrlParseError::InvalidWebSocketListenUrl(listen_url) => write!(
                f,
                "invalid websocket --listen URL `{listen_url}`; expected `ws://IP:PORT` or `wss://IP:PORT`"
            ),
        }
    }
}

impl std::error::Error for ExecServerListenUrlParseError {}

pub(crate) fn parse_listen_url(
    listen_url: &str,
) -> Result<ExecServerListenTransport, ExecServerListenUrlParseError> {
    if matches!(listen_url, "stdio" | "stdio://") {
        return Ok(ExecServerListenTransport::Stdio);
    }

    if let Some(socket_addr) = listen_url.strip_prefix("wss://") {
        return socket_addr
            .parse::<SocketAddr>()
            .map(ExecServerListenTransport::WebSocketTls)
            .map_err(|_| {
                ExecServerListenUrlParseError::InvalidWebSocketListenUrl(listen_url.to_string())
            });
    }

    if let Some(socket_addr) = listen_url.strip_prefix("ws://") {
        return socket_addr
            .parse::<SocketAddr>()
            .map(ExecServerListenTransport::WebSocket)
            .map_err(|_| {
                ExecServerListenUrlParseError::InvalidWebSocketListenUrl(listen_url.to_string())
            });
    }

    Err(ExecServerListenUrlParseError::UnsupportedListenUrl(
        listen_url.to_string(),
    ))
}

pub(crate) async fn run_transport(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match parse_listen_url(listen_url)? {
        ExecServerListenTransport::WebSocket(bind_address) => {
            run_websocket_listener(bind_address, runtime_paths).await
        }
        ExecServerListenTransport::WebSocketTls(bind_address) => {
            run_websocket_tls_listener(bind_address, runtime_paths).await
        }
        ExecServerListenTransport::Stdio => run_stdio_connection(runtime_paths).await,
    }
}

async fn run_stdio_connection(
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_stdio_connection_with_io(io::stdin(), io::stdout(), runtime_paths).await
}

async fn run_stdio_connection_with_io<R, W>(
    reader: R,
    writer: W,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let processor = ConnectionProcessor::new(runtime_paths);
    tracing::info!("codex-exec-server listening on stdio");
    processor
        .run_connection(JsonRpcConnection::from_stdio(
            reader,
            writer,
            "exec-server stdio".to_string(),
        ))
        .await;
    Ok(())
}

async fn run_websocket_listener(
    bind_address: SocketAddr,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(bind_address).await?;
    let local_addr = listener.local_addr()?;
    let processor = ConnectionProcessor::new(runtime_paths);
    info!("codex-exec-server listening on ws://{local_addr}");
    println!("ws://{local_addr}");
    std::io::stdout().flush()?;

    log_auth_mode();

    let router = build_router(processor);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Runs the WebSocket listener over TLS (`wss://`). Generates a self-signed
/// certificate, prints the listen URL and the SHA-256 fingerprint that clients
/// must pin (via `CODEX_EXEC_SERVER_TLS_PINNED_SHA256`), then serves each
/// accepted connection through a rustls `TlsAcceptor` + hyper-util connection
/// builder (required because `axum::serve` cannot wrap a TLS stream and because
/// WebSocket upgrades need `serve_connection_with_upgrades`).
async fn run_websocket_tls_listener(
    bind_address: SocketAddr,
    runtime_paths: ExecServerRuntimePaths,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioExecutor;
    use hyper_util::rt::TokioIo;
    use hyper_util::server::conn::auto::Builder as ConnBuilder;
    use tokio_rustls::TlsAcceptor;
    use tower::ServiceExt;

    let tls = crate::tls::generate_self_signed_tls()?;
    let fingerprint = tls.fingerprint_hex();
    let server_config = crate::tls::build_server_config(tls.cert_der, tls.key_der)?;

    let listener = TcpListener::bind(bind_address).await?;
    let local_addr = listener.local_addr()?;
    let processor = ConnectionProcessor::new(runtime_paths);
    info!("codex-exec-server listening on wss://{local_addr}");
    info!("codex-exec-server TLS certificate fingerprint (sha256): {fingerprint}");
    // The listen URL is consumed programmatically (e.g. by tests); the
    // fingerprint line lets operators copy the value clients must pin.
    println!("wss://{local_addr}");
    println!("pinned-sha256: {fingerprint}");
    std::io::stdout().flush()?;

    log_auth_mode();

    let tls_acceptor = TlsAcceptor::from(server_config);
    let router = build_router(processor);

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                warn!("codex-exec-server failed to accept TCP connection: {error}");
                continue;
            }
        };
        let tls_acceptor = tls_acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    warn!(%peer_addr, "codex-exec-server TLS handshake failed: {error}");
                    return;
                }
            };

            let hyper_service = service_fn(move |mut req: hyper::Request<Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer_addr));
                let router = router.clone();
                async move { router.oneshot(req).await }
            });

            let io = TokioIo::new(tls_stream);
            if let Err(error) = ConnBuilder::new(TokioExecutor::new())
                .http1_only()
                .serve_connection_with_upgrades(io, hyper_service)
                .await
            {
                warn!(%peer_addr, "codex-exec-server TLS connection error: {error}");
            }
        });
    }
}

/// Builds the axum router shared by the plain and TLS WebSocket listeners.
fn build_router(processor: ConnectionProcessor) -> Router {
    let required_auth_token = required_auth_token_from_env();
    Router::new()
        .route("/", any(websocket_upgrade_handler))
        .route("/readyz", get(readiness_handler))
        .layer(middleware::from_fn_with_state(
            AuthState {
                required_auth_token,
            },
            require_bearer_token,
        ))
        .layer(middleware::from_fn(reject_requests_with_origin_header))
        .with_state(ExecServerWebSocketState { processor })
}

/// Logs whether bearer-token authentication is enabled for incoming
/// connections.
fn log_auth_mode() {
    if required_auth_token_from_env().is_some() {
        info!("codex-exec-server requiring bearer token authentication on websocket connections");
    } else {
        warn!(
            "codex-exec-server running WITHOUT websocket authentication; set {CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR} to require a bearer token"
        );
    }
}

/// Reads the required bearer token from the environment, treating empty values
/// as "no token required".
fn required_auth_token_from_env() -> Option<String> {
    match std::env::var(CODEX_EXEC_SERVER_AUTH_TOKEN_ENV_VAR) {
        Ok(token) if !token.is_empty() => Some(token),
        _ => None,
    }
}

#[derive(Clone)]
struct AuthState {
    required_auth_token: Option<String>,
}

/// Extracts the bearer token from an `Authorization` header value.
fn extract_bearer_token(header_value: &str) -> Option<&str> {
    let token = header_value
        .strip_prefix("Bearer ")
        .or_else(|| header_value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

async fn require_bearer_token(
    State(state): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.required_auth_token.as_deref() else {
        // No token configured: authentication disabled.
        return Ok(next.run(request).await);
    };

    // Always allow unauthenticated readiness probes.
    if request.uri().path() == "/readyz" {
        return Ok(next.run(request).await);
    }

    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(extract_bearer_token)
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    if authorized {
        Ok(next.run(request).await)
    } else {
        warn!(
            method = %request.method(),
            uri = %request.uri(),
            "rejecting exec-server websocket request with missing/invalid bearer token"
        );
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Compares two byte slices in constant time to avoid leaking token length /
/// content via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Clone)]
struct ExecServerWebSocketState {
    processor: ConnectionProcessor,
}

async fn readiness_handler() -> StatusCode {
    StatusCode::OK
}

async fn reject_requests_with_origin_header(
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.headers().contains_key(ORIGIN) {
        warn!(
            method = %request.method(),
            uri = %request.uri(),
            "rejecting exec-server websocket listener request with Origin header"
        );
        Err(StatusCode::FORBIDDEN)
    } else {
        Ok(next.run(request).await)
    }
}

async fn websocket_upgrade_handler(
    websocket: WebSocketUpgrade,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(state): State<ExecServerWebSocketState>,
) -> impl IntoResponse {
    info!(%peer_addr, "exec-server websocket client connected");
    websocket.on_upgrade(move |stream| async move {
        state
            .processor
            .run_connection(JsonRpcConnection::from_axum_websocket(
                stream,
                format!("exec-server websocket {peer_addr}"),
            ))
            .await;
    })
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
