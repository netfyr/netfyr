//! Unprivileged network namespace helpers for integration tests.
//!
//! Uses `unshare(CLONE_NEWUSER | CLONE_NEWNET)` to create an isolated
//! network namespace without root privileges. Each `NetnsGuard` enters a new
//! namespace on construction and restores the original on drop.
//!
//! **Kernel requirement**: `/proc/sys/kernel/unprivileged_userns_clone` must
//! be 1 (the default on most distros). If namespace creation fails with
//! `EPERM`, tests should skip rather than fail.

use std::fs;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};

use futures::TryStreamExt;
use nix::sched::{unshare, CloneFlags};
use rtnetlink::LinkUnspec;

// ── NetnsGuard ────────────────────────────────────────────────────────────────

/// RAII guard that creates and enters an unprivileged user + network namespace.
///
/// Drops back to the original namespace when the guard is dropped.
pub struct NetnsGuard {
    /// File descriptor pointing to the original network namespace.
    original_ns: OwnedFd,
}

impl NetnsGuard {
    /// Enter a fresh user + network namespace.
    ///
    /// Returns `Err` if `unshare(2)` fails (e.g., kernel has user namespaces
    /// disabled). Callers should skip the test in that case.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Save the current netns fd before entering the new one.
        let original_ns_fd =
            fs::OpenOptions::new().read(true).open("/proc/self/ns/net")?;
        let fd = original_ns_fd.into_raw_fd();
        // SAFETY: We just obtained this fd from OpenOptions::open and take ownership.
        let original_ns = unsafe { OwnedFd::from_raw_fd(fd) };

        // Create a new user namespace + network namespace.
        unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET)?;

        // Write UID/GID mappings so the new user namespace is usable.
        let uid = nix::unistd::Uid::current().as_raw();
        let gid = nix::unistd::Gid::current().as_raw();

        // Deny setgroups before writing gid_map (required by kernel).
        let _ = fs::write("/proc/self/setgroups", "deny");
        let _ = fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"));
        let _ = fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"));

        Ok(Self { original_ns })
    }
}

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        // Restore original network namespace via setns(2).
        // nix::sched::setns requires a CloneFlags argument.
        let _ = nix::sched::setns(&self.original_ns, CloneFlags::CLONE_NEWNET);
    }
}

// ── Interface helpers ─────────────────────────────────────────────────────────

/// Get the interface index for a named interface in the current namespace.
pub async fn get_link_index(
    name: &str,
) -> Result<u32, Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    let mut stream = handle.link().get().execute();
    while let Some(msg) = stream.try_next().await? {
        for attr in &msg.attributes {
            if let netlink_packet_route::link::LinkAttribute::IfName(n) = attr {
                if n == name {
                    return Ok(msg.header.index);
                }
            }
        }
    }
    Err(format!("interface not found: {name}").into())
}

/// Create a veth pair inside the current network namespace.
pub async fn create_veth_pair(
    name0: &str,
    name1: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    handle
        .link()
        .add(rtnetlink::LinkVeth::new(name0, name1).build())
        .execute()
        .await?;

    Ok(())
}

/// Bring an interface to admin-up state.
pub async fn set_link_up(
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    let index = get_link_index(name).await?;
    handle
        .link()
        .change(LinkUnspec::new_with_index(index).up().build())
        .execute()
        .await?;
    Ok(())
}

/// Bring an interface to admin-down state.
pub async fn set_link_down(
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    let index = get_link_index(name).await?;
    handle
        .link()
        .change(LinkUnspec::new_with_index(index).down().build())
        .execute()
        .await?;
    Ok(())
}

/// Set the MTU of an interface.
pub async fn set_mtu(
    name: &str,
    mtu: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    let index = get_link_index(name).await?;
    handle
        .link()
        .change(LinkUnspec::new_with_index(index).mtu(mtu).build())
        .execute()
        .await?;
    Ok(())
}

/// Add an IP address (CIDR string, e.g. `"10.99.0.1/24"`) to an interface.
pub async fn add_address(
    name: &str,
    cidr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    let index = get_link_index(name).await?;

    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| format!("invalid CIDR: {cidr}"))?;
    let ip: std::net::IpAddr = ip_str.parse()?;
    let prefix: u8 = prefix_str.parse()?;

    handle.address().add(index, ip, prefix).execute().await?;

    Ok(())
}
