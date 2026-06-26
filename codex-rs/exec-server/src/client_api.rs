use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::ExecServerError;
use crate::HttpRequestParams;
use crate::HttpRequestResponse;
use crate::HttpResponseBodyStream;
use crate::NoiseChannelIdentity;
use crate::NoiseChannelPublicKey;

pub(crate) const DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection options for any exec-server client transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecServerClientConnectOptions {
    pub client_name: String,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
}

/// WebSocket connection arguments for a remote exec-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecServerConnectArgs {
    pub websocket_url: String,
    pub client_name: String,
    pub connect_timeout: Duration,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
    /// Optional bearer token sent as `Authorization: Bearer <token>` when
    /// opening the WebSocket connection. Used to authenticate against a
    /// token-protected exec-server (see `CODEX_EXEC_SERVER_AUTH_TOKEN`).
    pub auth_token: Option<String>,
    /// Optional SHA-256 fingerprint (32 bytes) of the server's TLS certificate.
    /// When set and the URL uses `wss://`, the client pins this certificate
    /// instead of validating against system roots, enabling self-signed,
    /// reverse-proxy-free end-to-end encryption.
    pub tls_pinned_sha256: Option<[u8; 32]>,
}

/// Registry-authorized material for one Noise rendezvous connection attempt.
///
/// Treat this as an atomic, single-use bundle. The URL authorization, executor
/// registration, pinned executor key, and harness-key authorization describe one
/// physical connection attempt and must not be mixed with values from another
/// registry response.
pub struct NoiseRendezvousConnectBundle {
    pub websocket_url: String,
    pub environment_id: String,
    pub executor_registration_id: String,
    pub executor_public_key: NoiseChannelPublicKey,
    pub harness_key_authorization: String,
}

/// Connection arguments for an authenticated Noise rendezvous exec-server.
///
/// `harness_identity` identifies the logical harness endpoint and may be reused
/// across reconnects. In contrast, callers must supply a fresh
/// [`NoiseRendezvousConnectBundle`] for each physical connection attempt.
pub struct NoiseRendezvousConnectArgs {
    pub bundle: NoiseRendezvousConnectBundle,
    pub harness_identity: NoiseChannelIdentity,
    pub client_name: String,
    pub connect_timeout: Duration,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
}

/// Supplies fresh registry-authorized material for Noise rendezvous connections.
pub trait NoiseRendezvousConnectProvider: Send + Sync {
    /// Fetch a bundle authorizing this harness key for one physical connection.
    fn connect_bundle(
        &self,
        harness_public_key: NoiseChannelPublicKey,
    ) -> BoxFuture<'_, Result<NoiseRendezvousConnectBundle, ExecServerError>>;
}

/// Stdio connection arguments for a command-backed exec-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StdioExecServerConnectArgs {
    pub command: StdioExecServerCommand,
    pub client_name: String,
    pub initialize_timeout: Duration,
    pub resume_session_id: Option<String>,
}

/// Structured process command used to start an exec-server over stdio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StdioExecServerCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// Parameters used to connect to a remote exec-server environment.
#[derive(Clone)]
pub(crate) enum ExecServerTransportParams {
    WebSocketUrl {
        websocket_url: String,
        connect_timeout: Duration,
        initialize_timeout: Duration,
        auth_token: Option<String>,
        tls_pinned_sha256: Option<[u8; 32]>,
    },
    NoiseRendezvous {
        provider: Arc<dyn NoiseRendezvousConnectProvider>,
        identity: NoiseChannelIdentity,
    },
    #[allow(dead_code)]
    StdioCommand {
        command: StdioExecServerCommand,
        initialize_timeout: Duration,
    },
}

impl std::fmt::Debug for ExecServerTransportParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WebSocketUrl {
                websocket_url,
                connect_timeout,
                initialize_timeout,
                auth_token,
                tls_pinned_sha256,
            } => f
                .debug_struct("WebSocketUrl")
                .field("websocket_url", websocket_url)
                .field("connect_timeout", connect_timeout)
                .field("initialize_timeout", initialize_timeout)
                .field("auth_token", &auth_token.as_ref().map(|_| "<redacted>"))
                .field("tls_pinned", &tls_pinned_sha256.is_some())
                .finish(),
            Self::NoiseRendezvous { .. } => {
                f.debug_struct("NoiseRendezvous").finish_non_exhaustive()
            }
            Self::StdioCommand {
                command,
                initialize_timeout,
            } => f
                .debug_struct("StdioCommand")
                .field("command", command)
                .field("initialize_timeout", initialize_timeout)
                .finish(),
        }
    }
}

impl ExecServerTransportParams {
    pub(crate) fn websocket_url_with_auth(
        websocket_url: String,
        connect_timeout: Duration,
        auth_token: Option<String>,
        tls_pinned_sha256: Option<[u8; 32]>,
    ) -> Self {
        Self::WebSocketUrl {
            websocket_url,
            connect_timeout,
            initialize_timeout: DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT,
            auth_token,
            tls_pinned_sha256,
        }
    }
}

/// Sends HTTP requests through a runtime-selected transport.
///
/// This is the HTTP capability counterpart to [`crate::ExecBackend`]. Callers
/// use it when they need environment-owned network requests but should not
/// depend on the concrete connection type or how that connection is established.
pub trait HttpClient: Send + Sync {
    /// Perform an HTTP request and buffer the response body.
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>>;

    /// Perform an HTTP request and return a streamed body handle.
    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>>;
}
