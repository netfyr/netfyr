//! DHCPv6 client for the ipv6auto factory.
// SPEC-412 will integrate this module into Ipv6AutoFactory; until then,
// all public items are temporarily unused.
#![allow(dead_code)]
//!
//! Supports stateful (IA_NA: acquires addresses and options) and stateless
//! (Information-Request: acquires options only) modes. The mode is determined
//! by the M/O flags in Router Advertisements (SPEC-412).
//!
//! This is not a standalone factory: it does not produce `State` or send
//! `FactoryEvent` directly. The caller (ipv6auto factory, SPEC-412) starts
//! a `Dhcpv6Client`, receives `Dhcpv6Result` messages, merges them into its
//! produced state, and forwards `FactoryEvent` to the daemon.

mod client;
mod duid;
pub mod lease;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use self::client::{create_dhcpv6_socket, find_link_local, run_dhcpv6_client, Dhcpv6Context};
use self::duid::{load_or_create_duid, DuidLlt};
use self::lease::Dhcpv6Lease;
use super::address::get_interface_mac;

// ── Public types ──────────────────────────────────────────────────────────────

/// Results sent by the DHCPv6 client to its caller (the ipv6auto factory).
#[derive(Debug, Clone)]
pub enum Dhcpv6Result {
    /// A new lease was successfully acquired.
    Acquired(Dhcpv6Lease),
    /// An existing lease was renewed (T1/T2 renewal or stateless refresh).
    Renewed(Dhcpv6Lease),
    /// The stateful lease expired without successful renewal or rebind.
    Expired,
    /// A transient error occurred; the client will retry.
    Error(String),
}

// ── Client ────────────────────────────────────────────────────────────────────

/// DHCPv6 client for one interface.
///
/// Manages the lifecycle of a background tokio task that runs the DHCPv6
/// protocol state machine and sends `Dhcpv6Result` messages to the caller.
pub struct Dhcpv6Client {
    interface: String,
    stateful: bool,
    /// Snapshot of the latest lease for synchronous reads.
    lease: Arc<Mutex<Option<Dhcpv6Lease>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task_handle: Option<JoinHandle<()>>,
}

impl Dhcpv6Client {
    /// Start the DHCPv6 client on `interface`.
    ///
    /// `stateful` selects the mode:
    /// - `true`: IA_NA stateful — acquires addresses and options.
    /// - `false`: Information-Request stateless — acquires options only.
    ///
    /// `duid_path` points to the persisted DUID file (default:
    /// `/var/lib/netfyr/duid`). Integration tests pass a tmpdir path.
    ///
    /// The caller must ensure a link-local address has completed DAD before
    /// calling `start()`. (The ipv6auto factory guarantees this because M/O
    /// flags can only be received in an RA, which requires a working link-local.)
    ///
    /// Returns immediately; the DHCPv6 exchange runs asynchronously. Results
    /// are sent via `result_tx`.
    pub async fn start(
        interface: &str,
        stateful: bool,
        duid_path: &Path,
        result_tx: mpsc::Sender<Dhcpv6Result>,
    ) -> Result<Self, String> {
        // Look up the link-local address and ifindex. The caller must have
        // waited for DAD to complete; this is a safety check.
        let (link_local, ifindex) = find_link_local(interface).await?;

        // Get MAC for DUID generation.
        let mac = get_interface_mac(interface).await?;

        // Load or create the DUID; the same DUID is shared across all interfaces.
        let duid: DuidLlt = load_or_create_duid(duid_path, mac)
            .map_err(|e| format!("dhcpv6: DUID error: {e}"))?;

        // Create the UDP socket bound to (link_local, 546).
        let socket = create_dhcpv6_socket(link_local, interface, ifindex)?;

        let lease = Arc::new(Mutex::new(None::<Dhcpv6Lease>));
        let (stop_tx, stop_rx) = oneshot::channel();

        let ctx = Dhcpv6Context {
            interface: interface.to_string(),
            stateful,
            iaid: ifindex,
            duid,
            link_local,
            ifindex,
            socket,
            result_tx,
            lease: Arc::clone(&lease),
        };

        let handle = tokio::spawn(async move {
            run_dhcpv6_client(ctx, stop_rx).await;
        });

        Ok(Self {
            interface: interface.to_string(),
            stateful,
            lease,
            stop_tx: Some(stop_tx),
            task_handle: Some(handle),
        })
    }

    /// Stop the client. In stateful mode the background task sends a Release
    /// message before exiting. Idempotent.
    pub async fn stop(&mut self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        Ok(())
    }

    /// Return a clone of the latest lease, or `None` if no lease has been
    /// acquired yet.
    pub fn current_lease(&self) -> Option<Dhcpv6Lease> {
        self.lease.lock().unwrap().clone()
    }

    /// Name of the interface this client is running on.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// Whether the client is in stateful mode.
    pub fn is_stateful(&self) -> bool {
        self.stateful
    }
}
