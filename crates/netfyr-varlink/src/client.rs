//! Varlink client for communicating with the `netfyr-daemon`.
//!
//! The client implements the Varlink wire protocol manually over a
//! `tokio::net::UnixStream`. Each message is a JSON object terminated by a
//! NUL byte (`\0`), following the Varlink specification.

use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::types::{
    VarlinkApplyReport, VarlinkDaemonStatus, VarlinkPolicy, VarlinkSelector, VarlinkState,
    VarlinkStateDiff,
};

/// Maximum allowed response size (16 MiB). Prevents unbounded memory growth
/// when receiving large query responses.
const MAX_RESPONSE_SIZE: usize = 16 * 1024 * 1024;

/// Timeout for the initial connection attempt to the daemon socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

// ── VarlinkError ──────────────────────────────────────────────────────────────

/// Errors produced by the Varlink client.
#[derive(Debug, Error)]
pub enum VarlinkError {
    /// The socket does not exist or connection was refused. The CLI treats this
    /// as "daemon not running" and falls back to local mode.
    #[error("connection failed: {0}")]
    ConnectionFailed(std::io::Error),

    /// I/O error during a read or write operation after connection.
    #[error("I/O error: {0}")]
    Io(std::io::Error),

    /// The response is not valid JSON or is missing expected fields.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server returned `io.netfyr.InvalidPolicy` — submitted policies failed
    /// validation. The CLI should print the reason and exit with code 2.
    #[error("invalid policy: {0}")]
    InvalidPolicy(String),

    /// The server returned `io.netfyr.BackendError` — a backend operation failed.
    #[error("backend error: {0}")]
    Backend(String),

    /// The server returned `io.netfyr.InternalError` — an unexpected daemon error.
    #[error("internal error: {0}")]
    Internal(String),
}

// ── VarlinkClient ─────────────────────────────────────────────────────────────

/// Async client for the `io.netfyr` Varlink API.
///
/// Wraps a `UnixStream` and serializes requests as NUL-terminated JSON objects.
/// Methods take `&mut self` because they perform sequential I/O on the stream;
/// the CLI makes calls sequentially so shared access is not needed.
pub struct VarlinkClient {
    stream: UnixStream,
}

impl VarlinkClient {
    /// Connect to the daemon's Varlink Unix socket.
    ///
    /// Returns `Err(VarlinkError::ConnectionFailed)` if the socket does not
    /// exist, connection is refused, or the connection times out after 2 seconds.
    pub async fn connect(socket_path: &str) -> Result<Self, VarlinkError> {
        let connect_future = UnixStream::connect(socket_path);
        let stream = timeout(CONNECT_TIMEOUT, connect_future)
            .await
            .map_err(|_| {
                VarlinkError::ConnectionFailed(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connection timed out",
                ))
            })?
            .map_err(VarlinkError::ConnectionFailed)?;
        Ok(Self { stream })
    }

    /// Submit policies with replace-all semantics.
    ///
    /// The daemon discards its current policy set and adopts the submitted set,
    /// then reconciles and applies. Returns an `ApplyReport` with the results.
    pub async fn submit_policies(
        &mut self,
        policies: Vec<VarlinkPolicy>,
    ) -> Result<VarlinkApplyReport, VarlinkError> {
        let params = serde_json::json!({ "policies": policies });
        let response = self.call("io.netfyr.SubmitPolicies", params).await?;
        serde_json::from_value(response["report"].clone()).map_err(|e| {
            VarlinkError::Protocol(format!("failed to decode ApplyReport: {e}"))
        })
    }

    /// Query current system state via the daemon.
    ///
    /// If `selector` is `Some`, filters results by entity type and/or selector
    /// fields. If `None`, returns all entities from all backends.
    pub async fn query(
        &mut self,
        selector: Option<&VarlinkSelector>,
    ) -> Result<Vec<VarlinkState>, VarlinkError> {
        let params = match selector {
            Some(sel) => serde_json::json!({ "selector": sel }),
            None => serde_json::json!({ "selector": null }),
        };
        let response = self.call("io.netfyr.Query", params).await?;
        serde_json::from_value(response["entities"].clone())
            .map_err(|e| VarlinkError::Protocol(format!("failed to decode states: {e}")))
    }

    /// Compute what would change if these policies were submitted, without applying.
    ///
    /// The daemon reconciles the submitted policies against current system state
    /// and returns the operations that would be performed.
    pub async fn dry_run(
        &mut self,
        policies: Vec<VarlinkPolicy>,
    ) -> Result<VarlinkStateDiff, VarlinkError> {
        let params = serde_json::json!({ "policies": policies });
        let response = self.call("io.netfyr.DryRun", params).await?;
        serde_json::from_value(response["diff"].clone()).map_err(|e| {
            VarlinkError::Protocol(format!("failed to decode StateDiff: {e}"))
        })
    }

    /// Get journal history entries, optionally filtered by count, time range, trigger, or entity.
    ///
    /// Returns raw `serde_json::Value` objects (one per entry) so that `netfyr-varlink`
    /// does not need to depend on `netfyr-journal`. The CLI deserializes them into
    /// `JournalEntry` on its end.
    pub async fn get_history(
        &mut self,
        count: Option<usize>,
        since: Option<String>,
        trigger: Option<String>,
        selector_name: Option<String>,
    ) -> Result<Vec<serde_json::Value>, VarlinkError> {
        let mut params = serde_json::Map::new();
        if let Some(c) = count {
            params.insert("count".to_string(), serde_json::json!(c));
        }
        if let Some(s) = since {
            params.insert("since".to_string(), serde_json::json!(s));
        }
        if let Some(t) = trigger {
            params.insert("trigger".to_string(), serde_json::json!(t));
        }
        if let Some(n) = selector_name {
            params.insert("selector_name".to_string(), serde_json::json!(n));
        }
        let response = self
            .call("io.netfyr.GetHistory", serde_json::Value::Object(params))
            .await?;
        match response["entries"].as_array() {
            Some(arr) => Ok(arr.clone()),
            None => Err(VarlinkError::Protocol(
                "response missing 'entries' array".into(),
            )),
        }
    }

    /// Get a single journal entry by sequence ID.
    ///
    /// Returns `None` if the entry does not exist. Returns raw `serde_json::Value`
    /// to avoid a dependency on `netfyr-journal` in this crate.
    pub async fn get_journal_entry(
        &mut self,
        seq: u64,
    ) -> Result<Option<serde_json::Value>, VarlinkError> {
        let params = serde_json::json!({ "seq": seq });
        let response = self.call("io.netfyr.GetJournalEntry", params).await?;
        let entry = &response["entry"];
        if entry.is_null() {
            Ok(None)
        } else {
            Ok(Some(entry.clone()))
        }
    }

    /// Get daemon status including uptime, active policy count, and running factories.
    pub async fn get_status(&mut self) -> Result<VarlinkDaemonStatus, VarlinkError> {
        let response = self.call("io.netfyr.GetStatus", serde_json::json!({})).await?;
        serde_json::from_value(response["status"].clone()).map_err(|e| {
            VarlinkError::Protocol(format!("failed to decode DaemonStatus: {e}"))
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Send a Varlink request and return the `parameters` object from the response.
    ///
    /// Request format: `{"method": "io.netfyr.MethodName", "parameters": {...}}\0`
    /// Success response: `{"parameters": {...}}\0`
    /// Error response: `{"error": "io.netfyr.ErrorName", "parameters": {"reason": "..."}}\0`
    async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, VarlinkError> {
        let request = serde_json::json!({
            "method": method,
            "parameters": params,
        });
        let mut msg = serde_json::to_vec(&request).map_err(|e| {
            VarlinkError::Protocol(format!("failed to serialize request: {e}"))
        })?;
        // Varlink wire protocol: messages are NUL-terminated.
        msg.push(0);

        self.send(&msg).await?;
        let raw = self.recv().await?;

        let response: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| {
            VarlinkError::Protocol(format!("invalid JSON response: {e}"))
        })?;

        // Check for a Varlink error response.
        if let Some(error) = response.get("error").and_then(|e| e.as_str()) {
            let reason = response
                .get("parameters")
                .and_then(|p| p.get("reason"))
                .and_then(|r| r.as_str())
                .unwrap_or("unknown reason")
                .to_string();

            return Err(match error {
                "io.netfyr.InvalidPolicy" => VarlinkError::InvalidPolicy(reason),
                "io.netfyr.BackendError" => VarlinkError::Backend(reason),
                "io.netfyr.InternalError" => VarlinkError::Internal(reason),
                other => VarlinkError::Protocol(format!("unknown error '{other}': {reason}")),
            });
        }

        response
            .get("parameters")
            .cloned()
            .ok_or_else(|| VarlinkError::Protocol("response missing 'parameters' field".into()))
    }

    /// Write all bytes to the stream.
    async fn send(&mut self, msg: &[u8]) -> Result<(), VarlinkError> {
        self.stream.write_all(msg).await.map_err(VarlinkError::Io)
    }

    /// Read bytes from the stream until a NUL terminator (`\0`) is encountered.
    ///
    /// Reads in 4 KiB chunks for efficiency. Returns the accumulated bytes
    /// before the NUL, or an error if the response exceeds `MAX_RESPONSE_SIZE`
    /// or the connection closes before the terminator is found.
    async fn recv(&mut self) -> Result<Vec<u8>, VarlinkError> {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];

        loop {
            if buf.len() >= MAX_RESPONSE_SIZE {
                return Err(VarlinkError::Protocol(
                    "response exceeds 16 MiB size limit".into(),
                ));
            }

            let n = self.stream.read(&mut chunk).await.map_err(VarlinkError::Io)?;
            if n == 0 {
                return Err(VarlinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before NUL terminator",
                )));
            }

            // Scan the chunk for a NUL terminator.
            if let Some(pos) = chunk[..n].iter().position(|&b| b == 0) {
                buf.extend_from_slice(&chunk[..pos]);
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        Ok(buf)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build a socket path inside a temp dir.
    fn temp_socket(dir: &tempfile::TempDir) -> String {
        dir.path().join("test.sock").to_string_lossy().into_owned()
    }

    /// Read one NUL-terminated message from a stream and return the parsed JSON.
    async fn read_request(stream: &mut tokio::net::UnixStream) -> serde_json::Value {
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.expect("read byte");
            if byte[0] == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        serde_json::from_slice(&buf).expect("valid JSON request")
    }

    /// Write a NUL-terminated JSON response to a stream.
    async fn write_response(stream: &mut tokio::net::UnixStream, body: serde_json::Value) {
        let mut msg = serde_json::to_vec(&body).expect("serialize response");
        msg.push(0);
        stream.write_all(&msg).await.expect("write response");
    }

    /// Spawn a mock server that accepts one connection, reads one request, sends
    /// a success response (`{"parameters": params}`), and then returns the parsed
    /// request JSON via the JoinHandle.
    ///
    /// The listener is bound synchronously before spawning to eliminate the race
    /// between socket creation and the client's connect call.
    fn spawn_mock_server(
        socket_path: String,
        response_params: serde_json::Value,
    ) -> tokio::task::JoinHandle<serde_json::Value> {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let req = read_request(&mut stream).await;
            let resp = serde_json::json!({ "parameters": response_params });
            write_response(&mut stream, resp).await;
            req
        })
    }

    /// Spawn a mock server that sends a Varlink error response and returns nothing.
    ///
    /// The listener is bound synchronously before spawning to eliminate the race
    /// between socket creation and the client's connect call.
    fn spawn_error_server(
        socket_path: String,
        error_name: &'static str,
        reason: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            // Drain the request so the client does not get a broken pipe.
            let _req = read_request(&mut stream).await;
            let resp = serde_json::json!({
                "error": error_name,
                "parameters": { "reason": reason }
            });
            write_response(&mut stream, resp).await;
        })
    }

    // ── Scenario: connect ──────────────────────────────────────────────────────

    /// Scenario: connect fails when socket does not exist.
    #[tokio::test]
    async fn test_client_connect_fails_when_socket_does_not_exist() {
        let result = VarlinkClient::connect("/tmp/netfyr_nonexistent_test.sock").await;
        assert!(
            matches!(result, Err(VarlinkError::ConnectionFailed(_))),
            "expected ConnectionFailed"
        );
    }

    /// Scenario: connect succeeds when a server is listening.
    #[tokio::test]
    async fn test_client_connect_succeeds_when_server_is_listening() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        // Bind listener before connecting so the socket file exists.
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            // Connection established — drop stream to close.
        });

        let client = VarlinkClient::connect(&path).await;
        assert!(client.is_ok(), "expected successful connect");
        server.await.unwrap();
    }

    // ── Scenario: SubmitPolicies ───────────────────────────────────────────────

    /// Scenario: submit_policies sends the correct method and receives an ApplyReport.
    #[tokio::test]
    async fn test_submit_policies_sends_policies_and_receives_apply_report() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let report_params = serde_json::json!({
            "report": {
                "succeeded": 2,
                "failed": 0,
                "skipped": 1,
                "changes": [],
                "conflicts": []
            }
        });
        let server = spawn_mock_server(path.clone(), report_params);

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.submit_policies(vec![]).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let report = result.unwrap();
        assert_eq!(report.succeeded, 2, "succeeded count must be 2");
        assert_eq!(report.failed, 0, "failed count must be 0");
        assert_eq!(report.skipped, 1, "skipped count must be 1");

        let req = server.await.unwrap();
        assert_eq!(
            req["method"].as_str(),
            Some("io.netfyr.SubmitPolicies"),
            "method must be io.netfyr.SubmitPolicies"
        );
    }

    /// Scenario: submit_policies with an invalid policy returns InvalidPolicy error.
    #[tokio::test]
    async fn test_submit_policies_with_invalid_policy_returns_invalid_policy_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let server = spawn_error_server(
            path.clone(),
            "io.netfyr.InvalidPolicy",
            "policy 'bad' has unknown factory",
        );

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.submit_policies(vec![]).await;

        assert!(
            matches!(result, Err(VarlinkError::InvalidPolicy(_))),
            "expected InvalidPolicy error, got {result:?}"
        );
        if let Err(VarlinkError::InvalidPolicy(msg)) = result {
            assert!(
                msg.contains("bad"),
                "reason must mention 'bad', got: {msg}"
            );
        }
        server.await.unwrap();
    }

    // ── Scenario: Query ────────────────────────────────────────────────────────

    /// Scenario: query without selector returns all entities from the server.
    #[tokio::test]
    async fn test_query_returns_varlink_states() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let entities_params = serde_json::json!({
            "entities": [
                {
                    "entity_type": "interface",
                    "selector": { "name": "eth0" },
                    "fields": { "mtu": 1500 }
                },
                {
                    "entity_type": "interface",
                    "selector": { "name": "eth1" },
                    "fields": { "mtu": 9000 }
                }
            ]
        });
        let server = spawn_mock_server(path.clone(), entities_params);

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.query(None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let states = result.unwrap();
        assert_eq!(states.len(), 2, "must receive 2 states");
        assert_eq!(states[0].entity_type, "interface");
        assert_eq!(
            states[0].selector.name.as_deref(),
            Some("eth0"),
            "first entity must be eth0"
        );

        let req = server.await.unwrap();
        assert_eq!(
            req["method"].as_str(),
            Some("io.netfyr.Query"),
            "method must be io.netfyr.Query"
        );
    }

    /// Scenario: query with a selector forwards the selector in the request.
    #[tokio::test]
    async fn test_query_with_selector_sends_selector_to_server() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let entities_params = serde_json::json!({ "entities": [] });
        let server = spawn_mock_server(path.clone(), entities_params);

        let selector = VarlinkSelector {
            entity_type: Some("interface".into()),
            name: Some("eth0".into()),
            driver: None,
            mac: None,
            pci_path: None,
        };

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.query(Some(&selector)).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let req = server.await.unwrap();
        let sel = &req["parameters"]["selector"];
        assert_eq!(
            sel["type"].as_str(),
            Some("interface"),
            "selector.type must be 'interface'"
        );
        assert_eq!(
            sel["name"].as_str(),
            Some("eth0"),
            "selector.name must be 'eth0'"
        );
    }

    // ── Scenario: DryRun ──────────────────────────────────────────────────────

    /// Scenario: dry_run returns the StateDiff from the server.
    #[tokio::test]
    async fn test_dry_run_returns_varlink_state_diff() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let diff_params = serde_json::json!({
            "diff": {
                "operations": [
                    {
                        "kind": "modify",
                        "entity_type": "interface",
                        "entity_name": "eth0",
                        "field_changes": [
                            {
                                "field_name": "mtu",
                                "change_kind": "set",
                                "current": null,
                                "desired": { "value": 9000 }
                            }
                        ]
                    }
                ]
            }
        });
        let server = spawn_mock_server(path.clone(), diff_params);

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.dry_run(vec![]).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let diff = result.unwrap();
        assert_eq!(diff.operations.len(), 1, "must have 1 operation");
        assert_eq!(diff.operations[0].kind, "modify");
        assert_eq!(diff.operations[0].entity_name, "eth0");

        let req = server.await.unwrap();
        assert_eq!(
            req["method"].as_str(),
            Some("io.netfyr.DryRun"),
            "method must be io.netfyr.DryRun"
        );
    }

    // ── Scenario: GetStatus ───────────────────────────────────────────────────

    /// Scenario: get_status returns DaemonStatus from the server.
    #[tokio::test]
    async fn test_get_status_returns_daemon_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let status_params = serde_json::json!({
            "status": {
                "uptime_seconds": 3600,
                "active_policies": 3,
                "running_factories": [
                    {
                        "policy_id": "dhcp-eth0",
                        "factory_type": "dhcpv4",
                        "interface_name": "eth0",
                        "state": "running",
                        "lease_ip": "192.168.1.100"
                    }
                ]
            }
        });
        let server = spawn_mock_server(path.clone(), status_params);

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.get_status().await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let status = result.unwrap();
        assert!(
            status.uptime_seconds >= 60,
            "uptime must be >= 60, got {}",
            status.uptime_seconds
        );
        assert_eq!(status.active_policies, 3, "active_policies must be 3");
        assert_eq!(
            status.running_factories.len(),
            1,
            "must have 1 running factory"
        );
        assert_eq!(status.running_factories[0].interface_name, "eth0");
        assert_eq!(
            status.running_factories[0].lease_ip.as_deref(),
            Some("192.168.1.100")
        );

        let req = server.await.unwrap();
        assert_eq!(
            req["method"].as_str(),
            Some("io.netfyr.GetStatus"),
            "method must be io.netfyr.GetStatus"
        );
    }

    // ── Scenario: error responses ─────────────────────────────────────────────

    /// Scenario: BackendError response returns VarlinkError::Backend.
    #[tokio::test]
    async fn test_backend_error_response_returns_backend_variant() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let server = spawn_error_server(
            path.clone(),
            "io.netfyr.BackendError",
            "netlink returned ENODEV",
        );

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.get_status().await;

        assert!(
            matches!(result, Err(VarlinkError::Backend(_))),
            "expected Backend error, got {result:?}"
        );
        if let Err(VarlinkError::Backend(msg)) = result {
            assert!(
                msg.contains("ENODEV"),
                "reason must mention ENODEV, got: {msg}"
            );
        }
        server.await.unwrap();
    }

    /// Scenario: InternalError response returns VarlinkError::Internal.
    #[tokio::test]
    async fn test_internal_error_response_returns_internal_variant() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let server = spawn_error_server(
            path.clone(),
            "io.netfyr.InternalError",
            "panic in factory thread",
        );

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.query(None).await;

        assert!(
            matches!(result, Err(VarlinkError::Internal(_))),
            "expected Internal error, got {result:?}"
        );
        if let Err(VarlinkError::Internal(msg)) = result {
            assert!(
                msg.contains("panic"),
                "reason must mention 'panic', got: {msg}"
            );
        }
        server.await.unwrap();
    }

    /// Scenario: unknown error name returns Protocol error.
    #[tokio::test]
    async fn test_unknown_error_name_returns_protocol_variant() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = temp_socket(&dir);

        let server = spawn_error_server(
            path.clone(),
            "io.netfyr.SomeUnknownError",
            "something unexpected",
        );

        let mut client = VarlinkClient::connect(&path).await.unwrap();
        let result = client.get_status().await;

        assert!(
            matches!(result, Err(VarlinkError::Protocol(_))),
            "expected Protocol error for unknown error name, got {result:?}"
        );
        server.await.unwrap();
    }
}
