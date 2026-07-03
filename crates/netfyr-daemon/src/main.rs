//! netfyr-daemon — declarative network configuration daemon.
//!
//! # Startup sequence
//!
//! 1. Initialize structured logging.
//! 2. Ensure the socket directory exists.
//! 3. Load persisted policies from disk ([`PolicyStore`]).
//! 4. Sync DHCP factories for existing policies ([`FactoryManager`]).
//! 5. Run initial reconciliation and apply.
//! 6. Notify systemd that the daemon is ready (`sd_notify READY=1`).
//! 7. Run the Varlink server event loop.
//! 8. On shutdown: release DHCP leases and exit (leave applied network config
//!    in place — the system should keep working).
//!
//! # Design decisions
//!
//! - **Resilient startup.** Each step logs errors but does not abort. If the
//!   policy store is unreadable, the daemon starts with an empty set. If
//!   initial reconciliation fails, the daemon still serves the Varlink API.
//!   A network daemon that refuses to start makes the system harder to
//!   recover, not easier.
//!
//! - **Config survives shutdown.** Applied network configuration is left in
//!   place when the daemon exits. Tearing down routes and addresses on stop
//!   would drop network connectivity — the system should keep working
//!   regardless of daemon lifecycle.
//!
//! - **Passive external change recording.** When the netlink monitor detects
//!   changes made by other tools (e.g. `ip link set`), the daemon journals
//!   them but does not re-apply desired state. Re-applying would create a
//!   state-fighting loop with other network management tools. The user can
//!   inspect drift via `netfyr history` and decide whether to revert.

pub mod policy_store;
mod factory_manager;
mod ipv6auto;
mod netlink_monitor;
mod reconciler;
mod server;

use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use netfyr_journal::Trigger;

use crate::factory_manager::FactoryManager;
use crate::policy_store::PolicyStore;
use crate::reconciler::Reconciler;

#[tokio::main]
async fn main() -> Result<()> {
    // Print the daemon's name to stdout unconditionally. This allows workspace
    // smoke tests and users running the binary bare to identify the program.
    // All other output uses tracing (stderr), so stdout contains only this line.
    println!("netfyr");

    // 1. Initialize structured logging (write to stderr; stdout is reserved for
    //    the "netfyr" identity line printed above).
    //    NETFYR_LOG sets the level for all netfyr crates (e.g. NETFYR_LOG=debug).
    //    RUST_LOG still works for fine-grained control. Falls back to "info".
    let env_filter = if let Ok(level) = std::env::var("NETFYR_LOG") {
        tracing_subscriber::EnvFilter::new(format!(
            "netfyr_daemon={level},netfyr_backend={level},netlink_packet_route=error"
        ))
    } else {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(
                "info,netlink_packet_route=error",
            ))
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter)
        .init();

    // Paths — override via environment for testing / alternative deployments.
    let socket_path = std::env::var("NETFYR_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/netfyr/netfyr.sock".to_string());
    let policy_dir = std::env::var("NETFYR_POLICY_DIR")
        .unwrap_or_else(|_| "/var/lib/netfyr/policies".to_string());

    // 2. Ensure the socket directory exists (RuntimeDirectory=netfyr in the
    //    systemd unit normally creates /run/netfyr/, but we create it here for
    //    non-systemd environments and tests).
    if let Some(dir) = Path::new(&socket_path).parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(%e, path = %dir.display(), "failed to create socket directory");
        }
    }

    // 3. Load persisted policies (StateDirectory=netfyr creates /var/lib/netfyr/).
    //    On failure, log the error but continue with an empty store — the daemon
    //    should still accept new policies via Varlink.
    let policy_store = match PolicyStore::load(Path::new(&policy_dir)) {
        Ok(store) => {
            tracing::info!(
                count = store.len(),
                dir = %policy_dir,
                "Loaded persisted policies"
            );
            store
        }
        Err(e) => {
            tracing::error!(%e, dir = %policy_dir, "failed to load policy store");
            PolicyStore::ephemeral(vec![])
        }
    };

    // 4. Start factories for existing DHCPv4 policies.
    let mut factory_manager = FactoryManager::new();
    match factory_manager.sync(policy_store.policies()).await {
        Ok(failed) if !failed.is_empty() => {
            tracing::warn!(
                failed = ?failed,
                "Some factories failed to start during daemon startup"
            );
        }
        Err(e) => {
            tracing::error!(%e, "factory sync error during startup");
        }
        _ => {}
    }

    // 5. Run initial reconciliation. On failure, log and continue — the daemon
    //    should still be available so the user can submit corrected policies.
    let reconciler = Reconciler::new();
    if let Err(e) = reconciler
        .reconcile_and_apply(&policy_store, &factory_manager, Trigger::DaemonStartup)
        .await
    {
        tracing::error!(%e, "initial reconciliation failed");
    }

    // 6. Record startup time and notify systemd.
    let start_time = Instant::now();
    match sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        Ok(()) => tracing::debug!("sd_notify READY=1 sent"),
        Err(e) => tracing::debug!(%e, "sd_notify"),
    }

    // 7. Run the Varlink server event loop. Returns on SIGTERM or SIGINT.
    //
    // If the server cannot bind (e.g. socket directory is inaccessible when run
    // outside systemd without root), log the error and exit cleanly. The daemon
    // has already printed its name and notified systemd (or silently failed to
    // do so), so there is nothing more to do.
    if let Err(e) = server::serve_varlink(
        &socket_path,
        policy_store,
        factory_manager,
        reconciler,
        start_time,
    )
    .await
    {
        tracing::error!(%e, "varlink server error");
    }

    Ok(())
}
