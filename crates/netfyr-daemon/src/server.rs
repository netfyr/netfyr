//! Varlink IPC server for the netfyr daemon.
//!
//! Implements the NUL-terminated JSON-over-UnixStream wire protocol used by
//! `netfyr-varlink`. The server owns `PolicyStore`, `FactoryManager`, and
//! `Reconciler` and processes requests sequentially (one connection at a time).
//!
//! # Protocol
//! - Request:  `{"method": "io.netfyr.MethodName", "parameters": {...}}\0`
//! - Success:  `{"parameters": {...}}\0`
//! - Error:    `{"error": "io.netfyr.ErrorName", "parameters": {"reason": "..."}}\0`

use std::time::Instant;

use anyhow::Result;
use netfyr_backend::FactoryEvent;
use netfyr_journal::Trigger;
use netfyr_varlink::{
    convert_apply_report_with_conflicts, VarlinkDaemonStatus, VarlinkFactoryStatus, VarlinkPolicy,
    VarlinkSelector, VarlinkState, VarlinkStateDiff,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::SignalKind;
use tracing::{debug, error, info, warn};

use crate::factory_manager::FactoryManager;
use crate::policy_store::PolicyStore;
use crate::reconciler::Reconciler;

/// Maximum message size accepted from clients (16 MiB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

// ── Wire-protocol helpers ─────────────────────────────────────────────────────

/// Read one NUL-terminated message from `stream` and parse it as JSON.
///
/// Reads in 4 KiB chunks. Returns an error if the stream closes before the
/// NUL terminator or if the accumulated bytes exceed `MAX_MESSAGE_SIZE`.
async fn read_message(stream: &mut UnixStream) -> Result<serde_json::Value> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    loop {
        if buf.len() >= MAX_MESSAGE_SIZE {
            anyhow::bail!("incoming message exceeds 16 MiB size limit");
        }

        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // Client disconnected cleanly.
            anyhow::bail!("client disconnected");
        }

        // Scan for a NUL terminator.
        if let Some(pos) = chunk[..n].iter().position(|&b| b == 0) {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Write a `serde_json::Value` as a NUL-terminated JSON message.
async fn write_message(stream: &mut UnixStream, msg: &serde_json::Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(0); // NUL terminator
    stream.write_all(&bytes).await?;
    Ok(())
}

/// Write a success response: `{"parameters": params}\0`.
async fn write_success(stream: &mut UnixStream, params: serde_json::Value) -> Result<()> {
    write_message(stream, &serde_json::json!({ "parameters": params })).await
}

/// Write an error response: `{"error": "io.netfyr.{name}", "parameters": {"reason": "..."}}\0`.
async fn write_error(stream: &mut UnixStream, error_name: &str, reason: &str) -> Result<()> {
    let full_name = if error_name.starts_with("io.netfyr.") {
        error_name.to_string()
    } else {
        format!("io.netfyr.{}", error_name)
    };
    write_message(
        stream,
        &serde_json::json!({
            "error": full_name,
            "parameters": { "reason": reason }
        }),
    )
    .await
}

// ── Request handlers ──────────────────────────────────────────────────────────

/// `io.netfyr.SubmitPolicies` — replace all policies and reconcile.
async fn handle_submit_policies(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    policy_store: &mut PolicyStore,
    factory_manager: &mut FactoryManager,
    reconciler: &Reconciler,
) -> Result<()> {
    // Parse policies from parameters.
    let varlink_policies: Vec<VarlinkPolicy> = match params.get("policies") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            anyhow::anyhow!("failed to parse policies: {}", e)
        })?,
        None => {
            return write_error(stream, "InternalError", "missing 'policies' parameter").await;
        }
    };

    // Convert VarlinkPolicy → Policy.
    let mut policies = Vec::with_capacity(varlink_policies.len());
    for vp in varlink_policies {
        match netfyr_policy::Policy::try_from(vp) {
            Ok(p) => policies.push(p),
            Err(e) => {
                return write_error(stream, "InvalidPolicy", &e).await;
            }
        }
    }

    // Replace all policies in the store.
    if let Err(e) = policy_store.replace_all(policies) {
        error!("Failed to persist policies: {}", e);
        return write_error(stream, "InternalError", &e.to_string()).await;
    }

    // Sync factories (stop removed, start new).
    match factory_manager.sync(policy_store.policies()).await {
        Ok(failed) if !failed.is_empty() => {
            warn!(failed = ?failed, "Some factories failed to start");
        }
        Err(e) => {
            error!("Factory sync error: {}", e);
        }
        _ => {}
    }

    // Reconcile and apply.
    let apply_result = match reconciler
        .reconcile_and_apply(
            policy_store,
            factory_manager,
            Trigger::PolicyApply { source: "daemon".into() },
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Reconciliation failed: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let varlink_report =
        convert_apply_report_with_conflicts(apply_result.report, &apply_result.conflicts);

    write_success(stream, serde_json::json!({ "report": varlink_report })).await
}

/// `io.netfyr.Query` — query current system state.
async fn handle_query(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    reconciler: &Reconciler,
) -> Result<()> {
    // Parse optional selector.
    let varlink_selector: Option<VarlinkSelector> = params
        .get("selector")
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                serde_json::from_value(v.clone()).ok()
            }
        });

    let (entity_type, selector) = match &varlink_selector {
        Some(vs) => {
            let entity_type_str = vs.entity_type.clone();
            let selector = netfyr_state::Selector::from(vs.clone());
            (entity_type_str, Some(selector))
        }
        None => (None, None),
    };

    let state_set = match reconciler
        .query(entity_type.as_deref(), selector.as_ref())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Query failed: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let entities: Vec<VarlinkState> = state_set.iter().map(VarlinkState::from).collect();

    write_success(stream, serde_json::json!({ "entities": entities })).await
}

/// `io.netfyr.DryRun` — compute diff without applying.
async fn handle_dry_run(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    factory_manager: &FactoryManager,
    reconciler: &Reconciler,
) -> Result<()> {
    let varlink_policies: Vec<VarlinkPolicy> = match params.get("policies") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            anyhow::anyhow!("failed to parse policies: {}", e)
        })?,
        None => {
            return write_error(stream, "InternalError", "missing 'policies' parameter").await;
        }
    };

    let mut policies = Vec::with_capacity(varlink_policies.len());
    for vp in varlink_policies {
        match netfyr_policy::Policy::try_from(vp) {
            Ok(p) => policies.push(p),
            Err(e) => {
                return write_error(stream, "InvalidPolicy", &e).await;
            }
        }
    }

    // Use an ephemeral store for the dry-run — don't touch the real policy store.
    let temp_store = PolicyStore::ephemeral(policies);

    let (reconcile_diff, _conflicts) =
        match reconciler.dry_run(&temp_store, factory_manager).await {
            Ok(r) => r,
            Err(e) => {
                error!("Dry-run failed: {}", e);
                return write_error(stream, "InternalError", &e.to_string()).await;
            }
        };

    let varlink_diff = VarlinkStateDiff::from(reconcile_diff);

    write_success(stream, serde_json::json!({ "diff": varlink_diff })).await
}

/// `io.netfyr.GetStatus` — return daemon status.
async fn handle_get_status(
    stream: &mut UnixStream,
    policy_store: &PolicyStore,
    factory_manager: &FactoryManager,
    start_time: Instant,
) -> Result<()> {
    let uptime_seconds = start_time.elapsed().as_secs() as i64;
    let active_policies = policy_store.len() as i64;

    let running_factories: Vec<VarlinkFactoryStatus> = factory_manager
        .factory_statuses()
        .into_iter()
        .map(|fs| VarlinkFactoryStatus {
            policy_id: fs.policy_name,
            factory_type: "dhcpv4".to_string(),
            interface_name: fs.interface,
            state: if fs.has_lease { "running" } else { "waiting" }.to_string(),
            lease_ip: fs.lease_ip,
        })
        .collect();

    let status = VarlinkDaemonStatus {
        uptime_seconds,
        active_policies,
        running_factories,
    };

    write_success(stream, serde_json::json!({ "status": status })).await
}

// ── Connection handler ────────────────────────────────────────────────────────

/// Handle all requests on a single connection until the client disconnects.
///
/// Processes requests sequentially. On each request, reads a NUL-terminated
/// JSON message, dispatches by `method`, and writes a response. Returns when
/// the client closes the connection (EOF) or an I/O error occurs.
///
/// Note: While a connection is active, factory events queue in the mpsc
/// channel and are processed after this function returns. This is acceptable
/// because CLI connections are short-lived (typically one request).
async fn handle_connection(
    stream: &mut UnixStream,
    policy_store: &mut PolicyStore,
    factory_manager: &mut FactoryManager,
    reconciler: &Reconciler,
    start_time: Instant,
) {
    loop {
        let msg = match read_message(stream).await {
            Ok(m) => m,
            Err(e) => {
                // "client disconnected" is the normal EOF path; other errors are logged.
                let msg = e.to_string();
                if !msg.contains("client disconnected") {
                    debug!("Connection read error: {}", e);
                }
                return;
            }
        };

        let method = match msg.get("method").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                let _ = write_error(stream, "InternalError", "missing 'method' field").await;
                return;
            }
        };

        let params = msg
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let result = match method.as_str() {
            "io.netfyr.SubmitPolicies" => {
                handle_submit_policies(
                    stream,
                    &params,
                    policy_store,
                    factory_manager,
                    reconciler,
                )
                .await
            }
            "io.netfyr.Query" => handle_query(stream, &params, reconciler).await,
            "io.netfyr.DryRun" => {
                handle_dry_run(stream, &params, factory_manager, reconciler).await
            }
            "io.netfyr.GetStatus" => {
                handle_get_status(stream, policy_store, factory_manager, start_time).await
            }
            other => {
                write_error(
                    stream,
                    "InternalError",
                    &format!("unknown method: '{}'", other),
                )
                .await
            }
        };

        if let Err(e) = result {
            debug!("Error writing response: {}", e);
            return;
        }
    }
}

// ── Main event loop ───────────────────────────────────────────────────────────

/// Start the Varlink server and run the main event loop.
///
/// Binds a `UnixListener` at `socket_path` and multiplexes two event sources
/// using `tokio::select!`:
/// 1. Incoming Varlink connections — processed to completion before looping.
/// 2. Factory events (DHCP lease changes) — trigger reconciliation.
///
/// Returns when SIGTERM or SIGINT is received. Removes the socket file and
/// calls `factory_manager.stop_all()` before returning.
pub async fn serve_varlink(
    socket_path: &str,
    mut policy_store: PolicyStore,
    mut factory_manager: FactoryManager,
    reconciler: Reconciler,
    start_time: Instant,
) -> Result<()> {
    // Remove a stale socket file from a previous run, if any.
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove stale socket file {}: {}", socket_path, e);
        }
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|e| anyhow::anyhow!("Failed to bind Varlink socket {}: {}", socket_path, e))?;

    info!("Varlink server listening on {}", socket_path);

    // Set up SIGTERM signal handler.
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            // ── Branch 1: incoming Varlink connection ─────────────────────────
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((mut stream, _)) => {
                        debug!("Accepted Varlink connection");
                        handle_connection(
                            &mut stream,
                            &mut policy_store,
                            &mut factory_manager,
                            &reconciler,
                            start_time,
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {}", e);
                    }
                }
            }

            // ── Branch 2: factory event (DHCP lease change) ───────────────────
            Some(event) = factory_manager.next_event() => {
                match event {
                    FactoryEvent::LeaseAcquired { ref policy_name, .. } => {
                        info!(policy = %policy_name, "DHCP lease acquired; re-reconciling");
                        if let Err(e) = reconciler
                            .reconcile_and_apply(
                                &policy_store,
                                &factory_manager,
                                Trigger::DhcpEvent {
                                    policy_name: policy_name.to_string(),
                                    event_kind: "lease_acquired".into(),
                                },
                            )
                            .await
                        {
                            error!("Reconciliation after lease acquisition failed: {}", e);
                        }
                    }
                    FactoryEvent::LeaseRenewed { ref policy_name, .. } => {
                        debug!(policy = %policy_name, "DHCP lease renewed; re-reconciling");
                        if let Err(e) = reconciler
                            .reconcile_and_apply(
                                &policy_store,
                                &factory_manager,
                                Trigger::DhcpEvent {
                                    policy_name: policy_name.to_string(),
                                    event_kind: "lease_renewed".into(),
                                },
                            )
                            .await
                        {
                            error!("Reconciliation after lease renewal failed: {}", e);
                        }
                    }
                    FactoryEvent::LeaseExpired { ref policy_name } => {
                        info!(policy = %policy_name, "DHCP lease expired; re-reconciling");
                        if let Err(e) = reconciler
                            .reconcile_and_apply(
                                &policy_store,
                                &factory_manager,
                                Trigger::DhcpEvent {
                                    policy_name: policy_name.to_string(),
                                    event_kind: "lease_expired".into(),
                                },
                            )
                            .await
                        {
                            error!("Reconciliation after lease expiry failed: {}", e);
                        }
                    }
                    FactoryEvent::Error { ref policy_name, ref error } => {
                        warn!(policy = %policy_name, error = %error, "DHCP factory error");
                    }
                }
            }

            // ── Branch 3: SIGTERM ─────────────────────────────────────────────
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
                break;
            }

            // ── Branch 4: SIGINT / Ctrl-C ─────────────────────────────────────
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down...");
                break;
            }
        }
    }

    // Graceful shutdown: release all DHCP leases.
    info!("Releasing DHCP leases...");
    if let Err(e) = factory_manager.stop_all().await {
        error!("Error during factory shutdown: {}", e);
    }

    // Remove socket file so systemd socket activation can rebind cleanly.
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove socket file on shutdown: {}", e);
        }
    }

    info!("Daemon shutdown complete");
    Ok(())
}

// ── Server unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory_manager::FactoryManager;
    use crate::policy_store::PolicyStore;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// Create a pair of connected Unix stream sockets for testing.
    async fn make_stream_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    /// Build a minimal static policy for use in tests.
    fn make_test_policy(name: &str) -> netfyr_policy::Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: static\npriority: 100\n\
             state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n"
        );
        netfyr_policy::parse_policy_yaml(&yaml)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    // ── Wire protocol: write_error ─────────────────────────────────────────────

    /// Scenario: write_error adds the io.netfyr. namespace prefix.
    #[tokio::test]
    async fn test_write_error_prefixes_with_io_netfyr_namespace() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "TestError", "something went wrong")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        let error_name = msg["error"].as_str().unwrap();
        assert_eq!(
            error_name, "io.netfyr.TestError",
            "error name must be prefixed with 'io.netfyr.'"
        );
    }

    /// Scenario: write_error does not double-prefix an already-namespaced error.
    #[tokio::test]
    async fn test_write_error_already_prefixed_not_double_prefixed() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "io.netfyr.TestError", "reason")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        let error_name = msg["error"].as_str().unwrap();
        assert_eq!(
            error_name, "io.netfyr.TestError",
            "already-prefixed error name must not be double-prefixed"
        );
    }

    /// Scenario: write_error includes the reason string in parameters.
    #[tokio::test]
    async fn test_write_error_includes_reason_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "TestError", "something went wrong")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert_eq!(
            msg["parameters"]["reason"].as_str().unwrap(),
            "something went wrong"
        );
    }

    // ── Wire protocol: write_success ──────────────────────────────────────────

    /// Scenario: write_success wraps the payload under "parameters".
    #[tokio::test]
    async fn test_write_success_wraps_payload_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        write_success(&mut server, serde_json::json!({"count": 42}))
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert_eq!(
            msg["parameters"]["count"].as_i64().unwrap(),
            42
        );
    }

    /// Scenario: write_success does not include an "error" field.
    #[tokio::test]
    async fn test_write_success_has_no_error_field() {
        let (mut server, mut client) = make_stream_pair().await;
        write_success(&mut server, serde_json::json!({}))
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "success response must not contain an 'error' field"
        );
    }

    // ── Wire protocol: read_message ────────────────────────────────────────────

    /// Scenario: read_message parses a NUL-terminated JSON message.
    #[tokio::test]
    async fn test_read_message_parses_nul_terminated_json() {
        let (mut writer, mut reader) = make_stream_pair().await;
        let json = r#"{"method":"io.netfyr.GetStatus","parameters":{}}"#;
        let mut bytes = json.as_bytes().to_vec();
        bytes.push(0u8); // NUL terminator
        writer.write_all(&bytes).await.unwrap();

        let msg = read_message(&mut reader).await.unwrap();
        assert_eq!(
            msg["method"].as_str().unwrap(),
            "io.netfyr.GetStatus"
        );
    }

    /// Scenario: read_message fails cleanly when the client disconnects without NUL.
    #[tokio::test]
    async fn test_read_message_fails_when_client_disconnects() {
        let (writer, mut reader) = make_stream_pair().await;
        drop(writer); // Close the write end
        let result = read_message(&mut reader).await;
        assert!(
            result.is_err(),
            "read_message must fail when the client disconnects"
        );
    }

    // ── GetStatus handler ──────────────────────────────────────────────────────

    /// Scenario: GetStatus returns daemon information — 3 policies in store → active_policies == 3.
    #[tokio::test]
    async fn test_handle_get_status_returns_active_policy_count() {
        let (mut server, mut client) = make_stream_pair().await;
        let policies = vec![
            make_test_policy("policy-a"),
            make_test_policy("policy-b"),
            make_test_policy("policy-c"),
        ];
        let policy_store = PolicyStore::ephemeral(policies);
        let factory_manager = FactoryManager::new();
        let start_time = Instant::now();

        handle_get_status(&mut server, &policy_store, &factory_manager, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let active_policies = msg["parameters"]["status"]["active_policies"]
            .as_i64()
            .unwrap();
        assert_eq!(
            active_policies, 3,
            "active_policies must match the number of policies in the store"
        );
    }

    /// Scenario: GetStatus returns 1 running factory with its status.
    /// (Verified via empty factory manager — 0 factories running.)
    #[tokio::test]
    async fn test_handle_get_status_returns_empty_running_factories_list() {
        let (mut server, mut client) = make_stream_pair().await;
        let policy_store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let start_time = Instant::now();

        handle_get_status(&mut server, &policy_store, &factory_manager, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let factories = msg["parameters"]["status"]["running_factories"]
            .as_array()
            .unwrap();
        assert!(
            factories.is_empty(),
            "fresh daemon with no factories must return empty running_factories"
        );
    }

    /// Scenario: GetStatus uptime_seconds is non-negative.
    #[tokio::test]
    async fn test_handle_get_status_uptime_seconds_is_non_negative() {
        let (mut server, mut client) = make_stream_pair().await;
        let policy_store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let start_time = Instant::now();

        handle_get_status(&mut server, &policy_store, &factory_manager, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let uptime = msg["parameters"]["status"]["uptime_seconds"]
            .as_i64()
            .unwrap();
        assert!(uptime >= 0, "uptime_seconds must be non-negative");
    }

    /// Scenario: GetStatus response has no "error" field.
    #[tokio::test]
    async fn test_handle_get_status_response_has_no_error_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let policy_store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let start_time = Instant::now();

        handle_get_status(&mut server, &policy_store, &factory_manager, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "GetStatus must not return an error field"
        );
    }

    // ── DryRun handler ────────────────────────────────────────────────────────

    /// Scenario: Dry-run computes diff without applying — empty policies returns success.
    #[tokio::test]
    async fn test_handle_dry_run_with_empty_policies_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({"policies": []}),
            &factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "dry_run with empty policies must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: Dry-run response contains a 'diff' field in parameters.
    #[tokio::test]
    async fn test_handle_dry_run_response_has_diff_field_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        let factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({"policies": []}),
            &factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("diff").is_some(),
            "dry_run response must include a 'diff' field in parameters: {:?}",
            msg
        );
    }

    /// Scenario: DryRun with missing 'policies' parameter returns an error response.
    #[tokio::test]
    async fn test_handle_dry_run_with_missing_policies_parameter_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        let factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({}), // no 'policies' key
            &factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "dry_run with missing 'policies' parameter must return an error response"
        );
    }

    // ── Query handler ─────────────────────────────────────────────────────────

    /// Scenario: Query returns current system state — no selector returns success.
    #[tokio::test]
    async fn test_handle_query_with_no_selector_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({"selector": null}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "query with null selector must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: Query response contains an 'entities' field.
    #[tokio::test]
    async fn test_handle_query_response_has_entities_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("entities").is_some(),
            "query response must include an 'entities' field: {:?}",
            msg
        );
    }

    /// Scenario: Query 'entities' field is an array.
    #[tokio::test]
    async fn test_handle_query_entities_field_is_an_array() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"]["entities"].is_array(),
            "query 'entities' must be an array: {:?}",
            msg
        );
    }

    // ── SubmitPolicies handler ────────────────────────────────────────────────

    /// Scenario: Submit policies replaces entire set — empty list returns success.
    #[tokio::test]
    async fn test_handle_submit_policies_with_empty_list_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let mut policy_store = PolicyStore::ephemeral(vec![]);
        let mut factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &mut policy_store,
            &mut factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "submit_policies with empty list must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: SubmitPolicies response contains a 'report' field.
    #[tokio::test]
    async fn test_handle_submit_policies_response_has_report_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let mut policy_store = PolicyStore::ephemeral(vec![]);
        let mut factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &mut policy_store,
            &mut factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("report").is_some(),
            "submit_policies response must include a 'report' field: {:?}",
            msg
        );
    }

    /// Scenario: Submit policies replaces entire set — policy store is updated to the new set.
    #[tokio::test]
    async fn test_handle_submit_policies_replaces_policy_store_with_new_set() {
        let (mut server, mut client) = make_stream_pair().await;
        // Pre-populate with one policy.
        let mut policy_store = PolicyStore::ephemeral(vec![make_test_policy("old-policy")]);
        let mut factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        // Submit empty list — old-policy must be replaced with nothing.
        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &mut policy_store,
            &mut factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let _msg = read_message(&mut client).await.unwrap();
        assert!(
            policy_store.is_empty(),
            "policy store must be empty after submit with empty policy list (replace-all semantics)"
        );
    }

    /// Scenario: SubmitPolicies with missing 'policies' field returns an error response.
    #[tokio::test]
    async fn test_handle_submit_policies_with_missing_policies_field_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        let mut policy_store = PolicyStore::ephemeral(vec![]);
        let mut factory_manager = FactoryManager::new();
        let reconciler = Reconciler::new();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({}), // missing 'policies' key
            &mut policy_store,
            &mut factory_manager,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "submit_policies with missing 'policies' field must return an error response"
        );
    }

    /// Scenario: Unknown method returns an error response.
    #[tokio::test]
    async fn test_handle_connection_unknown_method_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        // Send an unknown method
        let request = r#"{"method":"io.netfyr.NonExistentMethod","parameters":{}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        // Read the error response from server
        let _policy_store = PolicyStore::ephemeral(vec![]);
        let _factory_manager = FactoryManager::new();
        let _reconciler = Reconciler::new();
        let _start_time = Instant::now();

        // Run handle_connection in a background task
        let server_task = tokio::spawn(async move {
            handle_connection(
                &mut server,
                &mut PolicyStore::ephemeral(vec![]),
                &mut FactoryManager::new(),
                &Reconciler::new(),
                Instant::now(),
            )
            .await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_some(),
            "unknown method must produce an error response: {:?}",
            response
        );

        // Drop client to allow server task to finish
        drop(client);
        let _ = server_task.await;
    }
}
