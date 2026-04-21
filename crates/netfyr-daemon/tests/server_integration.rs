//! Integration tests for the netfyr-daemon Varlink server.
//!
//! These tests start the daemon binary as a subprocess, connect to its Varlink
//! socket, and verify the wire-protocol behavior end-to-end.
//!
//! The daemon binary path is resolved via `env!("CARGO_BIN_EXE_netfyr-daemon")`.
//! Temp directories are used for the socket and policy store so tests do not
//! affect the host system.
//!
//! # Network access
//! The daemon performs an initial `reconcile_and_apply` on startup. With an
//! empty policy store the desired state is empty; any Remove operations
//! generated for existing host interfaces are silently skipped or fail (no root
//! required and no host interfaces are modified).
//!
//! # Netns integration tests
//! Tests marked `netns_` require unprivileged user namespace support
//! (`/proc/sys/kernel/unprivileged_userns_clone == 1`) and dnsmasq for the DHCP
//! scenario. They skip gracefully when the prerequisite is unavailable.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::sleep;

// ── Wire-protocol helpers ─────────────────────────────────────────────────────

/// Send a NUL-terminated JSON Varlink request.
async fn send_request(stream: &mut UnixStream, msg: serde_json::Value) {
    let mut bytes = serde_json::to_vec(&msg).unwrap();
    bytes.push(0u8); // NUL terminator
    stream.write_all(&bytes).await.unwrap();
}

/// Read one NUL-terminated JSON Varlink response.
async fn read_response(stream: &mut UnixStream) -> serde_json::Value {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.expect("stream closed");
        assert!(n > 0, "stream closed before NUL terminator");
        if let Some(pos) = chunk[..n].iter().position(|&b| b == 0) {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&buf).unwrap_or_else(|e| {
        panic!(
            "failed to parse JSON response: {e}\nraw: {}",
            String::from_utf8_lossy(&buf)
        )
    })
}

// ── Daemon process helper ─────────────────────────────────────────────────────

/// RAII wrapper around a running netfyr-daemon subprocess.
struct DaemonProcess {
    child: Child,
    socket_path: std::path::PathBuf,
    _socket_dir: tempfile::TempDir,
    _policy_dir: tempfile::TempDir,
}

impl DaemonProcess {
    /// Start the daemon and wait up to `timeout` for the socket to appear.
    async fn start_with_timeout(timeout: Duration) -> Self {
        let socket_dir = tempfile::tempdir().unwrap();
        let policy_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("netfyr-test.sock");

        let child = Command::new(env!("CARGO_BIN_EXE_netfyr-daemon"))
            .env("NETFYR_SOCKET_PATH", socket_path.as_os_str())
            .env("NETFYR_POLICY_DIR", policy_dir.path())
            // Suppress tracing output to keep test output clean.
            .env("RUST_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn netfyr-daemon binary");

        // Poll for the socket file to appear.
        let deadline = Instant::now() + timeout;
        while !socket_path.exists() {
            assert!(
                Instant::now() < deadline,
                "netfyr-daemon socket did not appear within {:?}",
                timeout
            );
            sleep(Duration::from_millis(50)).await;
        }

        // Small grace period so the daemon finishes binding.
        sleep(Duration::from_millis(100)).await;

        DaemonProcess {
            child,
            socket_path,
            _socket_dir: socket_dir,
            _policy_dir: policy_dir,
        }
    }

    /// Start the daemon with a 15-second timeout.
    async fn start() -> Self {
        Self::start_with_timeout(Duration::from_secs(15)).await
    }

    /// Connect a Varlink client to the daemon socket.
    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket_path)
            .await
            .unwrap_or_else(|e| panic!("failed to connect to daemon socket: {e}"))
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Helper: build a minimal static VarlinkPolicy JSON object ──────────────────

/// A static policy with no inline state (safe: StaticFactory skips it with
/// MissingState; the policy is still persisted in the store).
fn varlink_static_policy(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "factory": "static",
        "priority": 100
    })
}

// ── Feature: Daemon core lifecycle ────────────────────────────────────────────

/// Scenario: Daemon starts and listens on Varlink socket.
#[tokio::test]
async fn test_daemon_starts_and_creates_varlink_socket() {
    let daemon = DaemonProcess::start().await;
    assert!(
        daemon.socket_path.exists(),
        "daemon must create the Varlink socket file on startup"
    );
}

/// Scenario: Daemon accepts connections after startup.
#[tokio::test]
async fn test_daemon_accepts_connections_after_startup() {
    let daemon = DaemonProcess::start().await;
    let _stream = daemon.connect().await;
    // If connect() succeeds, the daemon is listening.
}

// ── Feature: GetStatus ────────────────────────────────────────────────────────

/// Scenario: GetStatus returns daemon information — response has no error.
#[tokio::test]
async fn test_get_status_returns_no_error() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "GetStatus must not return an error: {:?}",
        response
    );
}

/// Scenario: GetStatus response contains a "status" object.
#[tokio::test]
async fn test_get_status_response_has_status_object() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response["parameters"]["status"].is_object(),
        "GetStatus response must include a 'status' object: {:?}",
        response
    );
}

/// Scenario: Fresh daemon has 0 active policies.
#[tokio::test]
async fn test_get_status_initially_has_zero_active_policies() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;

    let response = read_response(&mut stream).await;
    let active_policies = response["parameters"]["status"]["active_policies"]
        .as_i64()
        .expect("active_policies must be an integer");
    assert_eq!(
        active_policies, 0,
        "fresh daemon with no persisted policies must have 0 active policies"
    );
}

/// Scenario: Fresh daemon has 0 running factories.
#[tokio::test]
async fn test_get_status_initially_has_zero_running_factories() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;

    let response = read_response(&mut stream).await;
    let factories = response["parameters"]["status"]["running_factories"]
        .as_array()
        .expect("running_factories must be an array");
    assert!(
        factories.is_empty(),
        "fresh daemon must report 0 running factories"
    );
}

// ── Feature: Unknown method error handling ────────────────────────────────────

/// Scenario: Unknown method returns an error response.
#[tokio::test]
async fn test_unknown_method_returns_error_response() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.ThisMethodDoesNotExist",
            "parameters": {}
        }),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_some(),
        "unknown method must produce an error response: {:?}",
        response
    );
}

/// Scenario: Request with no "method" field returns an error response.
#[tokio::test]
async fn test_missing_method_field_returns_error() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(&mut stream, serde_json::json!({"parameters": {}})).await;

    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_some(),
        "request with no 'method' must produce an error: {:?}",
        response
    );
}

// ── Feature: Policy submission — replace-all semantics ───────────────────────

/// Scenario: SubmitPolicies with two policies → GetStatus shows 2 active policies.
#[tokio::test]
async fn test_submit_policies_increases_active_policy_count() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": {
                "policies": [
                    varlink_static_policy("policy-a"),
                    varlink_static_policy("policy-b"),
                ]
            }
        }),
    )
    .await;

    let submit_response = read_response(&mut stream).await;
    assert!(
        submit_response.get("error").is_none(),
        "SubmitPolicies must not return an error: {:?}",
        submit_response
    );

    // Verify via GetStatus
    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;
    let status = read_response(&mut stream).await;
    let active_policies = status["parameters"]["status"]["active_policies"]
        .as_i64()
        .unwrap();
    assert_eq!(
        active_policies, 2,
        "after submitting 2 policies, active_policies must be 2"
    );
}

/// Scenario: Submit policies replaces entire set — old policies are removed.
#[tokio::test]
async fn test_submit_policies_replaces_entire_policy_set() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // Submit 2 policies first.
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": {
                "policies": [
                    varlink_static_policy("policy-a"),
                    varlink_static_policy("policy-b"),
                ]
            }
        }),
    )
    .await;
    read_response(&mut stream).await;

    // Replace with just 1 policy.
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": {
                "policies": [
                    varlink_static_policy("policy-c"),
                ]
            }
        }),
    )
    .await;
    let submit_response = read_response(&mut stream).await;
    assert!(
        submit_response.get("error").is_none(),
        "second SubmitPolicies must not return an error: {:?}",
        submit_response
    );

    // Policy count must now be 1 (A and B were removed, C is the only policy).
    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;
    let status = read_response(&mut stream).await;
    let active_policies = status["parameters"]["status"]["active_policies"]
        .as_i64()
        .unwrap();
    assert_eq!(
        active_policies, 1,
        "after replacing with 1 policy, active_policies must be 1 (replace-all semantics)"
    );
}

/// Scenario: Submitting an empty policy set removes all policies.
#[tokio::test]
async fn test_submit_empty_policy_set_clears_all_policies() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // First submit some policies.
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": {
                "policies": [
                    varlink_static_policy("policy-a"),
                    varlink_static_policy("policy-b"),
                ]
            }
        }),
    )
    .await;
    read_response(&mut stream).await;

    // Then replace with empty set.
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": { "policies": [] }
        }),
    )
    .await;
    read_response(&mut stream).await;

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;
    let status = read_response(&mut stream).await;
    let active_policies = status["parameters"]["status"]["active_policies"]
        .as_i64()
        .unwrap();
    assert_eq!(
        active_policies, 0,
        "submitting empty policy set must clear all policies"
    );
}

// ── Feature: Dry-run computes diff without applying ───────────────────────────

/// Scenario: DryRun returns a diff object without applying changes.
#[tokio::test]
async fn test_dry_run_returns_diff_object() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.DryRun",
            "parameters": { "policies": [] }
        }),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "DryRun must not return an error: {:?}",
        response
    );
    assert!(
        response["parameters"]["diff"].is_object(),
        "DryRun must return a 'diff' object: {:?}",
        response
    );
}

/// Scenario: DryRun does not change the daemon's active policy count.
#[tokio::test]
async fn test_dry_run_does_not_change_active_policy_count() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // Dry-run with 1 policy.
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.DryRun",
            "parameters": {
                "policies": [varlink_static_policy("dry-run-only")]
            }
        }),
    )
    .await;
    let dry_run_response = read_response(&mut stream).await;
    assert!(
        dry_run_response.get("error").is_none(),
        "DryRun must not return an error: {:?}",
        dry_run_response
    );

    // Policy count must still be 0 (dry-run must not persist policies).
    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;
    let status = read_response(&mut stream).await;
    let active_policies = status["parameters"]["status"]["active_policies"]
        .as_i64()
        .unwrap();
    assert_eq!(
        active_policies, 0,
        "dry-run must not change the active policy count"
    );
}

/// Scenario: DryRun diff contains an "operations" array.
#[tokio::test]
async fn test_dry_run_diff_contains_operations_array() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.DryRun",
            "parameters": { "policies": [] }
        }),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response["parameters"]["diff"]["operations"].is_array(),
        "DryRun diff must have an 'operations' array: {:?}",
        response
    );
}

// ── Feature: Query returns current system state ───────────────────────────────

/// Scenario: Query with no selector returns a list of entities.
#[tokio::test]
async fn test_query_returns_entities_list() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.Query",
            "parameters": { "selector": null }
        }),
    )
    .await;

    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "Query must not return an error: {:?}",
        response
    );
    assert!(
        response["parameters"]["entities"].is_array(),
        "Query must return an 'entities' array: {:?}",
        response
    );
}

/// Scenario: Query returns current system state — multiple calls return consistent results.
#[tokio::test]
async fn test_query_is_repeatable() {
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    async fn do_query(stream: &mut UnixStream) -> serde_json::Value {
        send_request(
            stream,
            serde_json::json!({
                "method": "io.netfyr.Query",
                "parameters": { "selector": null }
            }),
        )
        .await;
        read_response(stream).await
    }

    let response1 = do_query(&mut stream).await;
    let response2 = do_query(&mut stream).await;

    let count1 = response1["parameters"]["entities"].as_array().unwrap().len();
    let count2 = response2["parameters"]["entities"].as_array().unwrap().len();
    assert_eq!(
        count1, count2,
        "Query must return consistent results across repeated calls"
    );
}

// ── Feature: Daemon loads persisted policies on startup ───────────────────────

/// Scenario: Daemon loads persisted policies — pre-populated policy dir is loaded.
#[tokio::test]
async fn test_daemon_loads_persisted_policies_on_startup() {
    let socket_dir = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let socket_path = socket_dir.path().join("netfyr-test.sock");

    // Pre-populate the policy directory with one policy file.
    let policy_content = "kind: policy\nname: pre-existing\nfactory: static\npriority: 100\n\
                          state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n";
    std::fs::write(policy_dir.path().join("pre-existing.yaml"), policy_content).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_netfyr-daemon"))
        .env("NETFYR_SOCKET_PATH", socket_path.as_os_str())
        .env("NETFYR_POLICY_DIR", policy_dir.path())
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn netfyr-daemon");

    // Wait for socket to appear.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !socket_path.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon socket did not appear within 15 seconds"
        );
        sleep(Duration::from_millis(50)).await;
    }
    sleep(Duration::from_millis(100)).await;

    let mut stream = UnixStream::connect(&socket_path).await.unwrap();

    send_request(
        &mut stream,
        serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
    )
    .await;
    let status = read_response(&mut stream).await;
    let active_policies = status["parameters"]["status"]["active_policies"]
        .as_i64()
        .unwrap();

    // Cleanup
    drop(stream);
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        active_policies, 1,
        "daemon must load the pre-existing policy on startup"
    );
}

// ── Feature: Integration test — netns static policy apply ────────────────────

/// Scenario: Daemon applies static policy in namespace — mtu change applied.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_daemon_applies_static_mtu_policy() {
    use netfyr_test_utils::{netns, NetnsGuard};

    // Try to enter a new user + network namespace.
    let _ns_guard = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "SKIP netns_daemon_applies_static_mtu_policy: \
                 failed to create network namespace ({e}). \
                 Kernel may have unprivileged_userns_clone disabled."
            );
            return;
        }
    };

    // Create a veth pair inside the new namespace.
    if let Err(e) = netns::create_veth_pair("veth-test0", "veth-test1").await {
        eprintln!("SKIP: failed to create veth pair: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up("veth-test0").await {
        eprintln!("SKIP: failed to bring veth-test0 up: {e}");
        return;
    }

    // Start daemon (inherits the new network namespace).
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // Submit a static policy setting veth-test0 mtu=1400.
    let policy = serde_json::json!({
        "name": "veth-test0-mtu",
        "factory": "static",
        "priority": 100,
        "state": {
            "entity_type": "ethernet",
            "selector": { "name": "veth-test0" },
            "fields": { "mtu": 1400 }
        }
    });

    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": { "policies": [policy] }
        }),
    )
    .await;
    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "SubmitPolicies must not return an error in netns: {:?}",
        response
    );

    // Verify the MTU was applied via netlink.
    let (conn, handle, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    use futures::TryStreamExt;
    let mut stream_nl = handle.link().get().execute();
    let mut mtu_applied: Option<u32> = None;
    while let Some(msg) = stream_nl.try_next().await.unwrap() {
        let mut is_veth_test0 = false;
        let mut link_mtu: Option<u32> = None;
        for attr in &msg.attributes {
            match attr {
                netlink_packet_route::link::LinkAttribute::IfName(n) if n == "veth-test0" => {
                    is_veth_test0 = true;
                }
                netlink_packet_route::link::LinkAttribute::Mtu(m) => {
                    link_mtu = Some(*m);
                }
                _ => {}
            }
        }
        if is_veth_test0 {
            mtu_applied = link_mtu;
            break;
        }
    }

    assert_eq!(
        mtu_applied,
        Some(1400),
        "veth-test0 MTU must be 1400 after applying policy"
    );
}

/// Scenario: Replace-all removes old policies in namespace — MTU changes from 1400 to 1300.
///
/// Requires unprivileged user namespace support.
#[tokio::test]
async fn netns_replace_all_updates_mtu() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns_guard = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "SKIP netns_replace_all_updates_mtu: \
                 failed to create network namespace ({e})"
            );
            return;
        }
    };

    if let Err(e) = netns::create_veth_pair("veth-rep0", "veth-rep1").await {
        eprintln!("SKIP: failed to create veth pair: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up("veth-rep0").await {
        eprintln!("SKIP: failed to bring veth-rep0 up: {e}");
        return;
    }

    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // First policy: mtu=1400
    let policy_1400 = serde_json::json!({
        "name": "veth-rep0-mtu",
        "factory": "static",
        "priority": 100,
        "state": {
            "entity_type": "ethernet",
            "selector": { "name": "veth-rep0" },
            "fields": { "mtu": 1400 }
        }
    });
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": { "policies": [policy_1400] }
        }),
    )
    .await;
    read_response(&mut stream).await;

    // Replace with mtu=1300
    let policy_1300 = serde_json::json!({
        "name": "veth-rep0-mtu",
        "factory": "static",
        "priority": 100,
        "state": {
            "entity_type": "ethernet",
            "selector": { "name": "veth-rep0" },
            "fields": { "mtu": 1300 }
        }
    });
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": { "policies": [policy_1300] }
        }),
    )
    .await;
    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "second SubmitPolicies must not return an error: {:?}",
        response
    );

    // Verify MTU is now 1300.
    let (conn, handle, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    use futures::TryStreamExt;
    let mut stream_nl = handle.link().get().execute();
    let mut mtu_applied: Option<u32> = None;
    while let Some(msg) = stream_nl.try_next().await.unwrap() {
        let mut is_target = false;
        let mut link_mtu: Option<u32> = None;
        for attr in &msg.attributes {
            match attr {
                netlink_packet_route::link::LinkAttribute::IfName(n) if n == "veth-rep0" => {
                    is_target = true;
                }
                netlink_packet_route::link::LinkAttribute::Mtu(m) => {
                    link_mtu = Some(*m);
                }
                _ => {}
            }
        }
        if is_target {
            mtu_applied = link_mtu;
            break;
        }
    }

    assert_eq!(
        mtu_applied,
        Some(1300),
        "veth-rep0 MTU must be 1300 after replacing policy"
    );
}

// ── Journal-enabled daemon helper ────────────────────────────────────────────

/// A daemon process that captures a journal directory for post-test inspection.
///
/// Used to verify that external changes (and only those) produce ExternalChange
/// journal entries, that self-changes are excluded, and that burst changes are
/// coalesced into a single entry.
struct DaemonProcessWithJournal {
    child: Child,
    socket_path: std::path::PathBuf,
    journal_dir: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
    _policy_dir: tempfile::TempDir,
}

impl DaemonProcessWithJournal {
    /// Start the daemon with an isolated journal directory.
    async fn start() -> Self {
        let socket_dir = tempfile::tempdir().unwrap();
        let policy_dir = tempfile::tempdir().unwrap();
        let journal_dir = tempfile::tempdir().unwrap();
        let socket_path = socket_dir.path().join("netfyr-ec-test.sock");

        let child = Command::new(env!("CARGO_BIN_EXE_netfyr-daemon"))
            .env("NETFYR_SOCKET_PATH", socket_path.as_os_str())
            .env("NETFYR_POLICY_DIR", policy_dir.path())
            .env("NETFYR_JOURNAL_DIR", journal_dir.path())
            .env("RUST_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn netfyr-daemon");

        let deadline = Instant::now() + Duration::from_secs(15);
        while !socket_path.exists() {
            assert!(
                Instant::now() < deadline,
                "netfyr-daemon socket did not appear within 15 seconds"
            );
            sleep(Duration::from_millis(50)).await;
        }
        sleep(Duration::from_millis(100)).await;

        DaemonProcessWithJournal {
            child,
            socket_path,
            journal_dir,
            _socket_dir: socket_dir,
            _policy_dir: policy_dir,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket_path)
            .await
            .unwrap_or_else(|e| panic!("failed to connect to daemon socket: {e}"))
    }

    /// Parse and return all journal entries from current.ndjson.
    fn read_journal_entries(&self) -> Vec<netfyr_journal::JournalEntry> {
        let path = self.journal_dir.path().join("current.ndjson");
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<netfyr_journal::JournalEntry>(l).ok())
            .collect()
    }
}

impl Drop for DaemonProcessWithJournal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Return the trigger type string for a journal entry.
fn trigger_type(entry: &netfyr_journal::JournalEntry) -> &'static str {
    match &entry.trigger {
        netfyr_journal::Trigger::ExternalChange { .. } => "external_change",
        netfyr_journal::Trigger::PolicyApply { .. } => "policy_apply",
        netfyr_journal::Trigger::DaemonStartup => "daemon_startup",
        netfyr_journal::Trigger::DhcpEvent { .. } => "dhcp_event",
        netfyr_journal::Trigger::Revert { .. } => "revert",
    }
}

/// Count journal entries with the given trigger type string.
fn count_entries_with_trigger(
    entries: &[netfyr_journal::JournalEntry],
    trigger: &str,
) -> usize {
    entries.iter().filter(|e| trigger_type(e) == trigger).count()
}

/// Submit a static MTU policy for a named interface via an open Varlink stream.
async fn submit_mtu_policy(stream: &mut UnixStream, iface: &str, mtu: u64) {
    send_request(
        stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": {
                "policies": [{
                    "name": format!("{iface}-mtu"),
                    "factory": "static",
                    "priority": 100,
                    "state": {
                        "entity_type": "ethernet",
                        "selector": { "name": iface },
                        "fields": { "mtu": mtu }
                    }
                }]
            }
        }),
    )
    .await;
    let response = read_response(stream).await;
    assert!(
        response.get("error").is_none(),
        "SubmitPolicies must not return an error: {:?}",
        response
    );
}

/// Run `ip link set <iface> mtu <mtu>` in the current network namespace.
fn ip_set_mtu(iface: &str, mtu: u32) -> bool {
    Command::new("ip")
        .args(["link", "set", iface, "mtu", &mtu.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `ip addr add <addr> dev <iface>` in the current network namespace.
fn ip_addr_add(iface: &str, addr: &str) -> bool {
    Command::new("ip")
        .args(["addr", "add", addr, "dev", iface])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `ip addr del <addr> dev <iface>` in the current network namespace.
fn ip_addr_del(iface: &str, addr: &str) -> bool {
    Command::new("ip")
        .args(["addr", "del", addr, "dev", iface])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Feature: Monitor detects MTU change ──────────────────────────────────────

/// AC: Monitor detects MTU change → journal entry with trigger "external_change",
/// outcome "observed", and changed_entities including the interface name.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_external_mtu_change_creates_external_change_journal_entry() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_external_mtu_change: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-mtu0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-mtu1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply initial policy: mtu=9000.
    submit_mtu_policy(&mut stream, iface, 9000).await;

    // Give the daemon time to write the journal entry before the external change.
    sleep(Duration::from_millis(200)).await;

    let entries_before = daemon.read_journal_entries();
    let ext_before = count_entries_with_trigger(&entries_before, "external_change");

    // External change: set mtu=1500 without going through the daemon.
    if !ip_set_mtu(iface, 1500) {
        eprintln!("SKIP: ip link set mtu failed");
        return;
    }

    // Wait for debounce (500ms) + processing buffer (700ms) = 1200ms.
    sleep(Duration::from_millis(1200)).await;

    let entries_after = daemon.read_journal_entries();
    let ext_after = count_entries_with_trigger(&entries_after, "external_change");

    assert!(
        ext_after > ext_before,
        "external MTU change must create at least one ExternalChange journal entry \
         (before={ext_before}, after={ext_after})"
    );

    // Find the ExternalChange entry and verify its fields.
    let ext_entry = entries_after
        .iter()
        .filter(|e| trigger_type(e) == "external_change")
        .last()
        .expect("must have at least one ExternalChange entry");

    // Outcome must be Observed.
    assert!(
        matches!(ext_entry.outcome, netfyr_journal::ApplyOutcome::Observed),
        "ExternalChange entry outcome must be Observed, got {:?}",
        ext_entry.outcome
    );

    // changed_entities must include the interface name.
    if let netfyr_journal::Trigger::ExternalChange { ref changed_entities } = ext_entry.trigger {
        assert!(
            changed_entities.contains(&iface.to_string()),
            "changed_entities must include {iface}: {:?}",
            changed_entities
        );
    } else {
        panic!("trigger must be ExternalChange");
    }

    // The diff must include a change for the mtu field.
    let has_mtu_change = ext_entry.diff.operations.iter().any(|op| {
        op.entity_name == iface
            && op.field_changes.iter().any(|fc| fc.field_name == "mtu")
    });
    assert!(
        has_mtu_change,
        "ExternalChange diff must include an mtu field change for {iface}"
    );
}

// ── Feature: Self-changes are excluded ───────────────────────────────────────

/// AC: When the daemon itself applies a policy, no ExternalChange entry is written.
///
/// NOTE: The current implementation may fail this test. After `reconcile_and_apply`
/// completes, the self-generated netlink events arrive ~500ms later (after debounce).
/// By then `is_applying()` is false, so the events are processed. The journal
/// comparison then finds fields (mac, operstate, carrier, etc.) that are in the
/// actual backend state but absent from the journal snapshot (which only stores
/// desired state). This causes spurious ExternalChange entries for those fields.
/// The fix is to store the actual post-apply state in the journal, not just the
/// desired state. See the verify phase.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_self_changes_do_not_create_external_change_entry() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_self_changes_excluded: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-self0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-self1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply a policy. The daemon will set mtu=9000 and generate netlink events.
    submit_mtu_policy(&mut stream, iface, 9000).await;

    // Wait for the debounce period to expire plus a buffer, so any self-generated
    // events have been processed.
    sleep(Duration::from_millis(1500)).await;

    let entries = daemon.read_journal_entries();
    let ext_count = count_entries_with_trigger(&entries, "external_change");
    let policy_count = count_entries_with_trigger(&entries, "policy_apply");

    // There must be exactly one policy_apply entry.
    assert_eq!(
        policy_count, 1,
        "policy submission must create exactly one policy_apply journal entry"
    );

    // There must be no external_change entries from the self-generated events.
    // NOTE: This assertion is expected per the spec. If it fails, there is a bug
    // where the daemon records self-changes as external changes because the journal
    // stores only desired state (subset of fields), not the full actual state.
    assert_eq!(
        ext_count, 0,
        "self-changes must not produce ExternalChange journal entries \
         (found {ext_count} ExternalChange entries after policy apply)"
    );
}

// ── Feature: Burst changes are coalesced ─────────────────────────────────────

/// AC: Burst changes coalesced — two external changes in quick succession produce
/// a single ExternalChange journal entry.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_burst_changes_coalesced_into_single_journal_entry() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_burst_changes_coalesced: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-burst0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-burst1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply initial policy.
    submit_mtu_policy(&mut stream, iface, 9000).await;

    // Wait for the policy apply to settle and for self-generated events to be
    // processed (if any), so we start from a known baseline.
    sleep(Duration::from_millis(1500)).await;

    let entries_baseline = daemon.read_journal_entries();

    // Make two rapid changes within the 500ms debounce window.
    if !ip_set_mtu(iface, 1400) {
        eprintln!("SKIP: ip link set mtu 1400 failed");
        return;
    }
    if !ip_addr_add(iface, "10.99.55.1/24") {
        eprintln!("SKIP: ip addr add failed");
        return;
    }
    // The two events above should arrive within the debounce window (< 500ms apart).

    // Wait for debounce + buffer.
    sleep(Duration::from_millis(1200)).await;

    let entries_after = daemon.read_journal_entries();
    let new_ext_entries: Vec<_> = entries_after
        .iter()
        .filter(|e| {
            trigger_type(e) == "external_change"
                && e.seq > entries_baseline.last().map(|e| e.seq).unwrap_or(0)
        })
        .collect();

    // The two burst changes should produce exactly ONE ExternalChange entry, not two.
    assert_eq!(
        new_ext_entries.len(),
        1,
        "two rapid changes must produce exactly one ExternalChange journal entry, \
         got {} new entries",
        new_ext_entries.len()
    );
}

// ── Feature: External changes do not trigger re-reconciliation ────────────────

/// AC: The daemon records the change but does not re-apply the policy.
/// After `ip link set mtu 1500` on an interface managed with mtu=9000, the
/// interface retains mtu=1500 (the daemon does NOT revert it).
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_external_change_does_not_trigger_reapply() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_no_reapply: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-noreap0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-noreap1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply mtu=9000.
    submit_mtu_policy(&mut stream, iface, 9000).await;
    sleep(Duration::from_millis(200)).await;

    // External change: set mtu=1500.
    if !ip_set_mtu(iface, 1500) {
        eprintln!("SKIP: ip link set mtu failed");
        return;
    }

    // Wait for debounce + buffer. The daemon must NOT re-apply mtu=9000.
    sleep(Duration::from_millis(1500)).await;

    // Query actual MTU via rtnetlink directly (not through the daemon).
    let (conn, handle, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    use futures::TryStreamExt;
    let mut link_stream = handle.link().get().execute();
    let mut actual_mtu: Option<u32> = None;
    while let Some(msg) = link_stream.try_next().await.unwrap() {
        let mut is_target = false;
        let mut link_mtu: Option<u32> = None;
        for attr in &msg.attributes {
            match attr {
                netlink_packet_route::link::LinkAttribute::IfName(n) if n == iface => {
                    is_target = true;
                }
                netlink_packet_route::link::LinkAttribute::Mtu(m) => {
                    link_mtu = Some(*m);
                }
                _ => {}
            }
        }
        if is_target {
            actual_mtu = link_mtu;
            break;
        }
    }

    assert_eq!(
        actual_mtu,
        Some(1500),
        "daemon must NOT revert external mtu change: interface must retain mtu=1500"
    );
}

// ── Feature: Monitor ignores unmanaged interfaces ─────────────────────────────

/// AC: A change on an interface not covered by any policy produces no journal entry.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_unmanaged_interface_change_not_journaled() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_unmanaged_ignored: namespace unavailable ({e})");
            return;
        }
    };

    let managed_iface = "veth-ec-mgd0";
    let unmanaged_iface = "veth-ec-umg0";

    // Create two veth pairs: one managed, one unmanaged.
    if let Err(e) = netns::create_veth_pair(managed_iface, "veth-ec-mgd1").await {
        eprintln!("SKIP: create_veth_pair(managed) failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(managed_iface).await {
        eprintln!("SKIP: set_link_up(managed) failed: {e}");
        return;
    }
    if let Err(e) = netns::create_veth_pair(unmanaged_iface, "veth-ec-umg1").await {
        eprintln!("SKIP: create_veth_pair(unmanaged) failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(unmanaged_iface).await {
        eprintln!("SKIP: set_link_up(unmanaged) failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Submit policy only for the managed interface.
    submit_mtu_policy(&mut stream, managed_iface, 9000).await;

    // Wait for apply and any self-generated events to settle.
    sleep(Duration::from_millis(1500)).await;

    let entries_baseline = daemon.read_journal_entries();
    let ext_baseline = count_entries_with_trigger(&entries_baseline, "external_change");

    // Change the UNMANAGED interface externally.
    if !ip_set_mtu(unmanaged_iface, 1300) {
        eprintln!("SKIP: ip link set mtu on unmanaged interface failed");
        return;
    }

    // Wait for debounce + buffer.
    sleep(Duration::from_millis(1200)).await;

    let entries_after = daemon.read_journal_entries();
    let ext_after = count_entries_with_trigger(&entries_after, "external_change");

    // No new ExternalChange entry must be written for the unmanaged interface.
    assert_eq!(
        ext_after, ext_baseline,
        "change on unmanaged interface must not produce ExternalChange journal entries \
         (baseline={ext_baseline}, after={ext_after})"
    );

    // Verify: no ExternalChange entry names the unmanaged interface.
    let unmanaged_entry = entries_after.iter().find(|e| {
        trigger_type(e) == "external_change"
            && match &e.trigger {
                netfyr_journal::Trigger::ExternalChange { changed_entities } => {
                    changed_entities.contains(&unmanaged_iface.to_string())
                }
                _ => false,
            }
    });
    assert!(
        unmanaged_entry.is_none(),
        "no ExternalChange entry must reference the unmanaged interface {unmanaged_iface}"
    );
}

// ── Feature: Monitor detects address changes ──────────────────────────────────

/// AC: Monitor detects address addition → ExternalChange journal entry recorded.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_external_address_addition_creates_journal_entry() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_addr_add: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-addr0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-addr1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply initial policy so the interface is managed and has a journal snapshot.
    submit_mtu_policy(&mut stream, iface, 9000).await;
    sleep(Duration::from_millis(1500)).await;

    let entries_baseline = daemon.read_journal_entries();
    let ext_baseline = count_entries_with_trigger(&entries_baseline, "external_change");

    // External address addition.
    if !ip_addr_add(iface, "10.99.40.1/24") {
        eprintln!("SKIP: ip addr add failed");
        return;
    }

    // Wait for debounce + buffer.
    sleep(Duration::from_millis(1200)).await;

    let entries_after = daemon.read_journal_entries();
    let ext_after = count_entries_with_trigger(&entries_after, "external_change");

    assert!(
        ext_after > ext_baseline,
        "external address addition must create an ExternalChange journal entry \
         (before={ext_baseline}, after={ext_after})"
    );
}

/// AC: Monitor detects address removal → ExternalChange journal entry recorded.
///
/// Requires unprivileged user namespace support. Skips gracefully if unavailable.
#[tokio::test]
async fn netns_external_address_removal_creates_journal_entry() {
    use netfyr_test_utils::{netns, NetnsGuard};

    let _ns = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("SKIP netns_addr_del: namespace unavailable ({e})");
            return;
        }
    };

    let iface = "veth-ec-addrdel0";
    if let Err(e) = netns::create_veth_pair(iface, "veth-ec-addrdel1").await {
        eprintln!("SKIP: create_veth_pair failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up(iface).await {
        eprintln!("SKIP: set_link_up failed: {e}");
        return;
    }

    let daemon = DaemonProcessWithJournal::start().await;
    let mut stream = daemon.connect().await;

    // Apply initial policy and add an address so there's something to remove.
    submit_mtu_policy(&mut stream, iface, 9000).await;
    sleep(Duration::from_millis(300)).await;

    if !ip_addr_add(iface, "10.99.41.1/24") {
        eprintln!("SKIP: ip addr add failed");
        return;
    }

    // Wait for events from the address add to settle.
    sleep(Duration::from_millis(1500)).await;

    let entries_baseline = daemon.read_journal_entries();
    let ext_baseline = count_entries_with_trigger(&entries_baseline, "external_change");

    // External address removal.
    if !ip_addr_del(iface, "10.99.41.1/24") {
        eprintln!("SKIP: ip addr del failed");
        return;
    }

    // Wait for debounce + buffer.
    sleep(Duration::from_millis(1200)).await;

    let entries_after = daemon.read_journal_entries();
    let ext_after = count_entries_with_trigger(&entries_after, "external_change");

    assert!(
        ext_after > ext_baseline,
        "external address removal must create an ExternalChange journal entry \
         (before={ext_baseline}, after={ext_after})"
    );
}

// ── Feature: Daemon handles DHCP policy in namespace ─────────────────────────

/// Scenario: Daemon handles DHCP policy in namespace — lease acquired.
///
/// Requires unprivileged user namespace support and dnsmasq installed.
#[tokio::test]
async fn netns_daemon_handles_dhcp_policy_acquires_lease() {
    use netfyr_test_utils::{netns, DnsmasqGuard, NetnsGuard};

    let _ns_guard = match NetnsGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "SKIP netns_daemon_handles_dhcp_policy_acquires_lease: \
                 failed to create network namespace ({e})"
            );
            return;
        }
    };

    // Create veth pair and configure the server side.
    if let Err(e) = netns::create_veth_pair("veth-dhcp0", "veth-dhcp1").await {
        eprintln!("SKIP: failed to create veth pair: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up("veth-dhcp0").await {
        eprintln!("SKIP: set_link_up(veth-dhcp0) failed: {e}");
        return;
    }
    if let Err(e) = netns::set_link_up("veth-dhcp1").await {
        eprintln!("SKIP: set_link_up(veth-dhcp1) failed: {e}");
        return;
    }
    // Assign server address on veth-dhcp1.
    if let Err(e) = netns::add_address("veth-dhcp1", "10.99.0.1/24").await {
        eprintln!("SKIP: add_address(veth-dhcp1) failed: {e}");
        return;
    }

    // Start dnsmasq on veth-dhcp1.
    let _dnsmasq = match DnsmasqGuard::start(
        "veth-dhcp1",
        "10.99.0.1",
        "10.99.0.100",
        "10.99.0.200",
        "120s",
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP netns_daemon_handles_dhcp_policy_acquires_lease: dnsmasq failed to start ({e})");
            return;
        }
    };

    // Start daemon inside the namespace.
    let daemon = DaemonProcess::start().await;
    let mut stream = daemon.connect().await;

    // Submit a DHCPv4 policy for veth-dhcp0.
    let dhcp_policy = serde_json::json!({
        "name": "dhcp-veth-dhcp0",
        "factory": "dhcpv4",
        "priority": 100,
        "selector": { "name": "veth-dhcp0" }
    });
    send_request(
        &mut stream,
        serde_json::json!({
            "method": "io.netfyr.SubmitPolicies",
            "parameters": { "policies": [dhcp_policy] }
        }),
    )
    .await;
    let response = read_response(&mut stream).await;
    assert!(
        response.get("error").is_none(),
        "SubmitPolicies (DHCPv4) must not return an error: {:?}",
        response
    );

    // Wait up to 10 seconds for a DHCP lease to be acquired.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut lease_ip: Option<String> = None;
    while Instant::now() < deadline {
        send_request(
            &mut stream,
            serde_json::json!({"method": "io.netfyr.GetStatus", "parameters": {}}),
        )
        .await;
        let status = read_response(&mut stream).await;
        let factories = status["parameters"]["status"]["running_factories"]
            .as_array()
            .unwrap();
        if let Some(f) = factories.first() {
            if let Some(ip) = f["lease_ip"].as_str() {
                lease_ip = Some(ip.to_string());
                break;
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    let ip = lease_ip.expect("DHCP lease must be acquired within 10 seconds");
    // Verify the IP is in the dnsmasq range 10.99.0.100-10.99.0.200.
    let parts: Vec<u8> = ip.split('.').filter_map(|p| p.parse().ok()).collect();
    assert_eq!(parts.len(), 4, "lease IP must be a valid IPv4 address");
    assert_eq!(&parts[..3], &[10, 99, 0], "lease IP must be in 10.99.0.x");
    assert!(
        parts[3] >= 100 && parts[3] <= 200,
        "lease IP last octet must be in range 100-200, got {}",
        parts[3]
    );
}
