//! Netlink monitor for external network state change detection.
//!
//! Subscribes to RTNLGRP_LINK (link attribute changes),
//! RTNLGRP_IPV4_IFADDR (address additions/removals), and
//! RTNLGRP_IPV4_ROUTE (route additions/removals) multicast groups and
//! emits debounced change notifications to the daemon's event loop.
//!
//! Message parsing uses raw byte offsets against the fixed-size Linux kernel
//! structs (nlmsghdr, ifinfomsg, ifaddrmsg, rtmsg, nlattr) rather than importing an
//! additional parsing crate. These layouts are stable ABI and documented in
//! linux/netlink.h and linux/rtnetlink.h.

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;

use anyhow::Result;
use netlink_sys::{protocols::NETLINK_ROUTE, Socket, SocketAddr};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Duration, Instant};
use tracing::{debug, error, warn};

// ── Netlink multicast groups ──────────────────────────────────────────────────

const RTNLGRP_LINK: u32 = 1;
const RTNLGRP_IPV4_IFADDR: u32 = 5;
const RTNLGRP_IPV4_ROUTE: u32 = 6;

// ── RTnetlink message types (from linux/rtnetlink.h) ─────────────────────────

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;

// ── Netlink control message types (linux/netlink.h) ───────────────────────────

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

// Receive timeout for the blocking RTM_GETLINK dump at startup (seconds).
const DUMP_RECV_TIMEOUT_SECS: i64 = 5;

// ── Netlink request flags (linux/netlink.h) ───────────────────────────────────

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300; // NLM_F_ROOT | NLM_F_MATCH

// ── Attribute types ───────────────────────────────────────────────────────────

const IFLA_IFNAME: u16 = 3;
/// Route output interface attribute (RTA_OIF) — carries the ifindex of the
/// output interface for a route.
const RTA_OIF: u16 = 7;

// ── Fixed-size struct layouts (bytes) ────────────────────────────────────────

const NLMSG_HDR_LEN: usize = 16; // sizeof(struct nlmsghdr)
const IFINFOMSG_LEN: usize = 16; // sizeof(struct ifinfomsg)
const IFADDRMSG_LEN: usize = 8; // sizeof(struct ifaddrmsg)
/// sizeof(struct rtmsg): rtm_family(1)+rtm_dst_len(1)+rtm_src_len(1)+rtm_tos(1)
///   +rtm_table(1)+rtm_protocol(1)+rtm_scope(1)+rtm_type(1)+rtm_flags(4) = 12 bytes.
/// Stable Linux ABI since kernel 2.2.
const RTMSG_LEN: usize = 12;

// ── Tuning constants ──────────────────────────────────────────────────────────

const DEBOUNCE_MS: u64 = 500;
const RECV_BUF_CAPACITY: usize = 65536;

// ── Public types ──────────────────────────────────────────────────────────────

/// Classifies the kind of netlink notification received.
#[derive(Debug, Clone)]
pub enum ChangeKind {
    /// A link attribute changed (MTU, operstate, flags, etc.).
    LinkChanged,
    /// An IPv4 address was added to an interface.
    AddressAdded,
    /// An IPv4 address was removed from an interface.
    AddressRemoved,
    /// An IPv4 route was added via an interface.
    RouteAdded,
    /// An IPv4 route was removed via an interface.
    RouteRemoved,
}

/// A single parsed netlink change notification.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct NetlinkChange {
    /// The kernel interface index.
    pub ifindex: u32,
    /// The interface name, if determinable from this message or cache.
    pub ifname: Option<String>,
    /// What kind of change was observed.
    pub kind: ChangeKind,
}

// ── NetlinkMonitor ────────────────────────────────────────────────────────────

/// Monitors the kernel's netlink route multicast groups and emits coalesced
/// change notifications after a 500 ms debounce window.
pub struct NetlinkMonitor {
    /// Receives debounced batches of changes from the background task.
    change_rx: mpsc::Receiver<Vec<NetlinkChange>>,
    /// Handle to the background monitoring task.
    task: JoinHandle<()>,
}

impl NetlinkMonitor {
    /// Open a netlink socket, subscribe to link and address groups, and start
    /// the background monitoring task.
    pub async fn start() -> Result<Self> {
        let mut socket = Socket::new(NETLINK_ROUTE)
            .map_err(|e| anyhow::anyhow!("Failed to create netlink monitor socket: {}", e))?;

        socket
            .bind_auto()
            .map_err(|e| anyhow::anyhow!("Failed to bind netlink monitor socket: {}", e))?;

        // Pre-populate the name cache with all existing interfaces by sending an
        // RTM_GETLINK dump while the socket is still blocking and before joining
        // multicast groups. This ensures address-only events after a daemon restart
        // can be resolved to interface names even if no RTM_NEWLINK event has occurred.
        let initial_name_cache = dump_link_names(&socket);

        // Switch to non-blocking BEFORE joining multicast groups so that multicast
        // events arriving after add_membership are handled by the async task, not
        // accidentally interleaved with dump response parsing above.
        socket
            .set_non_blocking(true)
            .map_err(|e| anyhow::anyhow!("Failed to set socket non-blocking: {}", e))?;

        socket
            .add_membership(RTNLGRP_LINK)
            .map_err(|e| anyhow::anyhow!("Failed to join RTNLGRP_LINK: {}", e))?;
        socket
            .add_membership(RTNLGRP_IPV4_IFADDR)
            .map_err(|e| anyhow::anyhow!("Failed to join RTNLGRP_IPV4_IFADDR: {}", e))?;
        socket
            .add_membership(RTNLGRP_IPV4_ROUTE)
            .map_err(|e| anyhow::anyhow!("Failed to join RTNLGRP_IPV4_ROUTE: {}", e))?;

        // Larger receive buffer reduces risk of ENOBUFS under heavy event load.
        let _ = socket.set_rx_buf_sz(1024u32 * 1024);

        let async_fd = AsyncFd::new(socket).map_err(|e| {
            anyhow::anyhow!("Failed to create AsyncFd for netlink monitor socket: {}", e)
        })?;

        let (tx, rx) = mpsc::channel(32);
        let task = tokio::spawn(monitor_task(async_fd, tx, initial_name_cache));

        Ok(Self {
            change_rx: rx,
            task,
        })
    }

    /// Wait for the next coalesced batch of change notifications.
    ///
    /// Returns `None` when the monitor task has exited.
    pub async fn next_change(&mut self) -> Option<Vec<NetlinkChange>> {
        self.change_rx.recv().await
    }

    /// Shut down the monitor and release the netlink socket.
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

// ── Startup link dump ─────────────────────────────────────────────────────────

/// Send an RTM_GETLINK dump request on a blocking socket and collect all
/// ifindex → ifname mappings from the kernel's response.
///
/// Called during `NetlinkMonitor::start()` while the socket is still blocking
/// and before any multicast subscriptions, so dump responses cannot interleave
/// with multicast events.  On any failure the function logs a warning and returns
/// an empty map — the monitor continues with an unpopulated cache and will fill
/// it in as subsequent RTM_NEWLINK multicast messages arrive.
fn dump_link_names(socket: &Socket) -> HashMap<u32, String> {
    // Build an RTM_GETLINK dump request: nlmsghdr + zeroed ifinfomsg.
    let msg_len = NLMSG_HDR_LEN + IFINFOMSG_LEN;
    let mut msg = [0u8; NLMSG_HDR_LEN + IFINFOMSG_LEN];
    msg[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes()); // nlmsg_len
    msg[4..6].copy_from_slice(&RTM_GETLINK.to_ne_bytes()); // nlmsg_type
    msg[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes()); // nlmsg_flags
    msg[8..12].copy_from_slice(&1u32.to_ne_bytes()); // nlmsg_seq
    // nlmsg_pid = 0 (kernel); already zero

    // Set a receive timeout so the blocking recv cannot hang the daemon if the
    // kernel doesn't send NLMSG_DONE (e.g. due to NLMSG_ERROR or permission issues).
    let timeout = libc::timeval { tv_sec: DUMP_RECV_TIMEOUT_SECS, tv_usec: 0 };
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }

    // Connect to the kernel (pid=0, groups=0) so that socket.send() knows the
    // destination without requiring a separate sendto address argument.
    if let Err(e) = socket.connect(&SocketAddr::new(0, 0)) {
        warn!("RTM_GETLINK dump: failed to connect to kernel: {}", e);
        return HashMap::new();
    }

    if let Err(e) = socket.send(&msg, 0) {
        warn!("RTM_GETLINK dump: failed to send request: {}", e);
        return HashMap::new();
    }

    let mut result = HashMap::new();
    loop {
        let mut buf = Vec::with_capacity(RECV_BUF_CAPACITY);
        if let Err(e) = socket.recv_from(&mut buf, 0) {
            // EAGAIN/EWOULDBLOCK = timeout expired; any other error = unrecoverable.
            // In both cases, return what we have so far and let the monitor continue.
            warn!("RTM_GETLINK dump: recv error: {}", e);
            return result;
        }

        // Guard against a zero-length recv (shouldn't happen on a blocking netlink
        // socket, but prevents an infinite outer loop if the kernel closes the socket).
        if buf.is_empty() {
            break;
        }

        let mut done = false;
        let mut offset = 0;
        while offset < buf.len() {
            let remaining = &buf[offset..];
            if remaining.len() < NLMSG_HDR_LEN {
                break;
            }
            let nlmsg_len =
                u32::from_ne_bytes([remaining[0], remaining[1], remaining[2], remaining[3]])
                    as usize;
            if nlmsg_len < NLMSG_HDR_LEN || nlmsg_len > remaining.len() {
                break;
            }
            let nlmsg_type = u16::from_ne_bytes([remaining[4], remaining[5]]);
            let msg_buf = &remaining[..nlmsg_len];

            // NLMSG_DONE signals end of dump; NLMSG_ERROR signals a kernel error.
            // Both terminate the recv loop.
            if nlmsg_type == NLMSG_DONE || nlmsg_type == NLMSG_ERROR {
                done = true;
                break;
            }
            if nlmsg_type == RTM_NEWLINK {
                if let Some((ifindex, Some(ifname))) = parse_link_message(msg_buf) {
                    result.insert(ifindex, ifname);
                }
            }

            let aligned = (nlmsg_len + 3) & !3;
            offset += aligned.max(NLMSG_HDR_LEN);
        }

        if done {
            break;
        }
    }

    let count = result.len();
    let names: Vec<&String> = result.values().collect();
    debug!("RTM_GETLINK dump: cached {} names: {:?}", count, names);
    result
}

// ── Background task ───────────────────────────────────────────────────────────

async fn monitor_task(
    async_fd: AsyncFd<Socket>,
    tx: mpsc::Sender<Vec<NetlinkChange>>,
    initial_name_cache: HashMap<u32, String>,
) {
    // Pending changes keyed by ifindex: (best-known ifname, accumulated kinds).
    let mut pending: HashMap<u32, (Option<String>, Vec<ChangeKind>)> = HashMap::new();
    // Cache ifindex → ifname, pre-populated from the RTM_GETLINK dump at startup
    // and updated as RTM_NEWLINK messages arrive during normal operation.
    let mut name_cache = initial_name_cache;

    // "Far future" sentinel means no pending debounce timer.
    let far_future = Instant::now() + Duration::from_secs(365 * 24 * 3600);
    let mut debounce_deadline = far_future;
    let mut has_pending = false;

    loop {
        tokio::select! {
            // biased: check debounce timer first to prevent starvation under
            // high event rates.
            biased;

            _ = sleep_until(debounce_deadline), if has_pending => {
                has_pending = false;
                debounce_deadline = far_future;

                let changes: Vec<NetlinkChange> = pending
                    .drain()
                    .flat_map(|(ifindex, (ifname, kinds))| {
                        // Deduplicate: emit at most one entry per ChangeKind variant
                        // per interface per debounce window.
                        let mut unique: Vec<ChangeKind> = Vec::new();
                        for k in kinds {
                            if !unique.iter().any(|u| kind_eq(u, &k)) {
                                unique.push(k);
                            }
                        }
                        let ifname = ifname.clone();
                        unique.into_iter().map(move |kind| NetlinkChange {
                            ifindex,
                            ifname: ifname.clone(),
                            kind,
                        })
                    })
                    .collect();

                let count = changes.len();
                debug!(count, "debounce timer fired, emitting changes");
                if !changes.is_empty()
                    && tx.send(changes).await.is_err()
                {
                    debug!("Netlink monitor: receiver dropped, stopping");
                    break;
                }
            }

            guard_result = async_fd.readable() => {
                let mut guard = match guard_result {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Netlink monitor: AsyncFd error: {}", e);
                        break;
                    }
                };

                // Vec<u8> implements bytes::BufMut (via the `bytes` crate which is
                // a transitive dependency through netlink-sys). We pass it here
                // without importing `bytes` directly — the compiler resolves the
                // generic bound from the transitive impl.
                let io_result = guard.try_io(|inner| {
                    let mut buf = Vec::with_capacity(RECV_BUF_CAPACITY);
                    inner.get_ref().recv_from(&mut buf, 0).map(|(_n, _addr)| buf)
                });

                match io_result {
                    Ok(Ok(data)) => {
                        process_buffer(
                            &data,
                            &mut pending,
                            &mut name_cache,
                            &mut has_pending,
                            &mut debounce_deadline,
                        );
                    }
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Ok(Err(e)) => {
                        warn!("Netlink monitor: recv error: {}", e);
                    }
                    Err(_would_block) => {}
                }
            }
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn kind_eq(a: &ChangeKind, b: &ChangeKind) -> bool {
    matches!(
        (a, b),
        (ChangeKind::LinkChanged, ChangeKind::LinkChanged)
            | (ChangeKind::AddressAdded, ChangeKind::AddressAdded)
            | (ChangeKind::AddressRemoved, ChangeKind::AddressRemoved)
            | (ChangeKind::RouteAdded, ChangeKind::RouteAdded)
            | (ChangeKind::RouteRemoved, ChangeKind::RouteRemoved)
    )
}

/// Process a raw receive buffer that may contain one or more netlink messages.
fn process_buffer(
    data: &[u8],
    pending: &mut HashMap<u32, (Option<String>, Vec<ChangeKind>)>,
    name_cache: &mut HashMap<u32, String>,
    has_pending: &mut bool,
    debounce_deadline: &mut Instant,
) {
    let debounce_duration = Duration::from_millis(DEBOUNCE_MS);
    let mut offset = 0;

    while offset < data.len() {
        let buf = &data[offset..];
        if buf.len() < NLMSG_HDR_LEN {
            break;
        }

        // struct nlmsghdr: nlmsg_len(u32) nlmsg_type(u16) nlmsg_flags(u16)
        //                  nlmsg_seq(u32) nlmsg_pid(u32)
        let nlmsg_len = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if nlmsg_len < NLMSG_HDR_LEN || nlmsg_len > buf.len() {
            break;
        }

        let nlmsg_type = u16::from_ne_bytes([buf[4], buf[5]]);
        let msg_buf = &buf[..nlmsg_len];

        if let Some((ifindex, ifname, kind)) = parse_message(nlmsg_type, msg_buf) {
            if let Some(ref name) = ifname {
                name_cache.insert(ifindex, name.clone());
            }
            let resolved_name = ifname.or_else(|| name_cache.get(&ifindex).cloned());

            let entry = pending
                .entry(ifindex)
                .or_insert((resolved_name.clone(), Vec::new()));
            if entry.0.is_none() {
                entry.0 = resolved_name;
            }
            debug!(ifindex, ifname = ?entry.0, ?kind, "netlink event parsed");
            entry.1.push(kind);

            // Reset debounce timer on every event (sliding-window debounce).
            *debounce_deadline = Instant::now() + debounce_duration;
            *has_pending = true;
        }

        // Each message is 4-byte aligned in the buffer.
        let aligned = (nlmsg_len + 3) & !3;
        offset += aligned.max(NLMSG_HDR_LEN);
    }
}

/// Dispatch to the appropriate parser based on message type.
fn parse_message(nlmsg_type: u16, buf: &[u8]) -> Option<(u32, Option<String>, ChangeKind)> {
    match nlmsg_type {
        RTM_NEWLINK | RTM_DELLINK => {
            let (ifindex, ifname) = parse_link_message(buf)?;
            Some((ifindex, ifname, ChangeKind::LinkChanged))
        }
        RTM_NEWADDR => {
            let ifindex = parse_addr_message(buf)?;
            Some((ifindex, None, ChangeKind::AddressAdded))
        }
        RTM_DELADDR => {
            let ifindex = parse_addr_message(buf)?;
            Some((ifindex, None, ChangeKind::AddressRemoved))
        }
        RTM_NEWROUTE => {
            let ifindex = parse_route_message(buf)?;
            Some((ifindex, None, ChangeKind::RouteAdded))
        }
        RTM_DELROUTE => {
            let ifindex = parse_route_message(buf)?;
            Some((ifindex, None, ChangeKind::RouteRemoved))
        }
        _ => {
            debug!(nlmsg_type, "ignoring netlink message type");
            None
        }
    }
}

/// Extract ifindex and IFLA_IFNAME from a RTM_NEWLINK or RTM_DELLINK message.
///
/// struct ifinfomsg layout:
///   ifi_family  u8  (AF_UNSPEC = 0)
///   ifi_pad     u8
///   ifi_type    u16 (ARPHRD_*)
///   ifi_index   i32 (interface index)
///   ifi_flags   u32
///   ifi_change  u32
fn parse_link_message(buf: &[u8]) -> Option<(u32, Option<String>)> {
    if buf.len() < NLMSG_HDR_LEN + IFINFOMSG_LEN {
        return None;
    }
    let index_off = NLMSG_HDR_LEN + 4; // skip ifi_family(1) + ifi_pad(1) + ifi_type(2)
    let ifi_index = i32::from_ne_bytes([
        buf[index_off],
        buf[index_off + 1],
        buf[index_off + 2],
        buf[index_off + 3],
    ]);
    if ifi_index <= 0 {
        debug!(ifindex = ifi_index, "rejecting link message: invalid ifindex");
        return None;
    }
    let ifname = parse_nlattr_string(buf, NLMSG_HDR_LEN + IFINFOMSG_LEN, IFLA_IFNAME);
    Some((ifi_index as u32, ifname))
}

/// Extract ifindex from a RTM_NEWADDR or RTM_DELADDR message.
///
/// struct ifaddrmsg layout:
///   ifa_family     u8
///   ifa_prefixlen  u8
///   ifa_flags      u8
///   ifa_scope      u8
///   ifa_index      u32
fn parse_addr_message(buf: &[u8]) -> Option<u32> {
    if buf.len() < NLMSG_HDR_LEN + IFADDRMSG_LEN {
        let len = buf.len();
        let expected = NLMSG_HDR_LEN + IFADDRMSG_LEN;
        debug!(len, expected, "rejecting addr message: buffer too small");
        return None;
    }
    let index_off = NLMSG_HDR_LEN + 4; // skip ifa_family(1) + ifa_prefixlen(1) + ifa_flags(1) + ifa_scope(1)
    Some(u32::from_ne_bytes([
        buf[index_off],
        buf[index_off + 1],
        buf[index_off + 2],
        buf[index_off + 3],
    ]))
}

/// Extract the output interface index (RTA_OIF) from an RTM_NEWROUTE or RTM_DELROUTE message.
///
/// struct rtmsg layout (12 bytes):
///   rtm_family(u8) rtm_dst_len(u8) rtm_src_len(u8) rtm_tos(u8)
///   rtm_table(u8) rtm_protocol(u8) rtm_scope(u8) rtm_type(u8)
///   rtm_flags(u32)
///
/// Returns `None` if the buffer is too short or if RTA_OIF is absent (e.g.,
/// blackhole/unreachable routes that have no output interface).
fn parse_route_message(buf: &[u8]) -> Option<u32> {
    if buf.len() < NLMSG_HDR_LEN + RTMSG_LEN {
        return None;
    }
    parse_nlattr_u32(buf, NLMSG_HDR_LEN + RTMSG_LEN, RTA_OIF)
}

/// Scan netlink attributes starting at `start`, returning the value of the
/// first attribute with `target_type` as a native-endian u32.
///
/// struct nlattr: nla_len(u16) nla_type(u16) data[nla_len - 4]
/// Attributes are 4-byte aligned.
fn parse_nlattr_u32(buf: &[u8], start: usize, target_type: u16) -> Option<u32> {
    let mut pos = start;
    while pos + 4 <= buf.len() {
        let nla_len = u16::from_ne_bytes([buf[pos], buf[pos + 1]]) as usize;
        let nla_type = u16::from_ne_bytes([buf[pos + 2], buf[pos + 3]]);
        if nla_len < 4 {
            break;
        }
        let data_end = pos + nla_len;
        if data_end > buf.len() {
            break;
        }
        if nla_type == target_type {
            let data = &buf[pos + 4..data_end];
            if data.len() < 4 {
                return None;
            }
            return Some(u32::from_ne_bytes([data[0], data[1], data[2], data[3]]));
        }
        let aligned = (nla_len + 3) & !3;
        pos += aligned.max(4);
    }
    None
}

/// Scan netlink attributes starting at `start`, returning the value of the
/// first attribute with `target_type` as a UTF-8 string (NUL stripped).
///
/// struct nlattr: nla_len(u16) nla_type(u16) data[nla_len - 4]
/// Attributes are 4-byte aligned.
fn parse_nlattr_string(buf: &[u8], start: usize, target_type: u16) -> Option<String> {
    let mut pos = start;
    while pos + 4 <= buf.len() {
        let nla_len = u16::from_ne_bytes([buf[pos], buf[pos + 1]]) as usize;
        let nla_type = u16::from_ne_bytes([buf[pos + 2], buf[pos + 3]]);
        if nla_len < 4 {
            break;
        }
        let data_end = pos + nla_len;
        if data_end > buf.len() {
            break;
        }
        if nla_type == target_type {
            let data = &buf[pos + 4..data_end];
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            return String::from_utf8(data[..end].to_vec()).ok();
        }
        let aligned = (nla_len + 3) & !3;
        pos += aligned.max(4);
    }
    None
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── Test message builders ──────────────────────────────────────────────────

    /// Build a minimal valid RTM_NEWLINK buffer with the given ifindex and ifname.
    ///
    /// Layout: nlmsghdr(16) + ifinfomsg(16) + nlattr IFLA_IFNAME(4 + name + NUL, aligned)
    fn build_link_msg(msg_type: u16, ifindex: i32, ifname: &str) -> Vec<u8> {
        let name_bytes = ifname.as_bytes();
        let attr_data_len = name_bytes.len() + 1; // +1 for NUL
        let attr_len = 4 + attr_data_len; // nla_len(2) + nla_type(2) + data
        let attr_len_aligned = (attr_len + 3) & !3;
        let msg_len = NLMSG_HDR_LEN + IFINFOMSG_LEN + attr_len_aligned;
        let mut buf = vec![0u8; msg_len];

        // nlmsghdr
        buf[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes()); // nlmsg_len
        buf[4..6].copy_from_slice(&msg_type.to_ne_bytes()); // nlmsg_type

        // ifinfomsg: ifi_family(1)+ifi_pad(1)+ifi_type(2)+ifi_index(4)+ifi_flags(4)+ifi_change(4)
        let index_off = NLMSG_HDR_LEN + 4; // skip ifi_family+ifi_pad+ifi_type
        buf[index_off..index_off + 4].copy_from_slice(&ifindex.to_ne_bytes());

        // nlattr IFLA_IFNAME
        let attr_off = NLMSG_HDR_LEN + IFINFOMSG_LEN;
        buf[attr_off..attr_off + 2].copy_from_slice(&(attr_len as u16).to_ne_bytes()); // nla_len
        buf[attr_off + 2..attr_off + 4].copy_from_slice(&IFLA_IFNAME.to_ne_bytes()); // nla_type
        buf[attr_off + 4..attr_off + 4 + name_bytes.len()].copy_from_slice(name_bytes);
        // NUL terminator already zero from vec![0u8; msg_len]

        buf
    }

    /// Build a minimal RTM_NEWADDR or RTM_DELADDR buffer with the given ifindex.
    ///
    /// Layout: nlmsghdr(16) + ifaddrmsg(8)
    fn build_addr_msg(msg_type: u16, ifindex: u32) -> Vec<u8> {
        let msg_len = NLMSG_HDR_LEN + IFADDRMSG_LEN;
        let mut buf = vec![0u8; msg_len];

        // nlmsghdr
        buf[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes());
        buf[4..6].copy_from_slice(&msg_type.to_ne_bytes());

        // ifaddrmsg: ifa_family(1)+ifa_prefixlen(1)+ifa_flags(1)+ifa_scope(1)+ifa_index(4)
        let index_off = NLMSG_HDR_LEN + 4;
        buf[index_off..index_off + 4].copy_from_slice(&ifindex.to_ne_bytes());

        buf
    }

    /// A debounce deadline far in the future — used to assert the deadline was
    /// moved closer (i.e., reset to ~500ms from now) after an event.
    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(365 * 24 * 3600)
    }

    #[allow(clippy::type_complexity)]
    fn empty_state() -> (
        HashMap<u32, (Option<String>, Vec<ChangeKind>)>,
        HashMap<u32, String>,
        bool,
        Instant,
    ) {
        (HashMap::new(), HashMap::new(), false, far_future())
    }

    // ── AC: ChangeKind variants ────────────────────────────────────────────────

    /// AC: ChangeKind::LinkChanged represents a link attribute change.
    #[test]
    fn test_change_kind_link_changed_variant_exists() {
        assert!(matches!(ChangeKind::LinkChanged, ChangeKind::LinkChanged));
    }

    /// AC: ChangeKind::AddressAdded represents IPv4 address addition.
    #[test]
    fn test_change_kind_address_added_variant_exists() {
        assert!(matches!(ChangeKind::AddressAdded, ChangeKind::AddressAdded));
    }

    /// AC: ChangeKind::AddressRemoved represents IPv4 address removal.
    #[test]
    fn test_change_kind_address_removed_variant_exists() {
        assert!(matches!(ChangeKind::AddressRemoved, ChangeKind::AddressRemoved));
    }

    // ── AC: kind_eq deduplication helper ──────────────────────────────────────

    /// AC: kind_eq returns true for matching variants.
    #[test]
    fn test_kind_eq_same_variants_are_equal() {
        assert!(kind_eq(&ChangeKind::LinkChanged, &ChangeKind::LinkChanged));
        assert!(kind_eq(&ChangeKind::AddressAdded, &ChangeKind::AddressAdded));
        assert!(kind_eq(&ChangeKind::AddressRemoved, &ChangeKind::AddressRemoved));
    }

    /// AC: kind_eq returns false for distinct variants.
    #[test]
    fn test_kind_eq_different_variants_are_not_equal() {
        assert!(!kind_eq(&ChangeKind::LinkChanged, &ChangeKind::AddressAdded));
        assert!(!kind_eq(&ChangeKind::AddressAdded, &ChangeKind::AddressRemoved));
        assert!(!kind_eq(&ChangeKind::AddressRemoved, &ChangeKind::LinkChanged));
    }

    // ── AC: process_buffer — edge cases ───────────────────────────────────────

    /// AC: Empty buffer does not set has_pending or add pending entries.
    #[test]
    fn test_process_buffer_with_empty_data_is_no_op() {
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&[], &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(!has_pending, "empty buffer must not set has_pending");
        assert!(pending.is_empty(), "empty buffer must not produce pending entries");
    }

    /// AC: Truncated buffer (shorter than nlmsghdr) is ignored without panic.
    #[test]
    fn test_process_buffer_with_truncated_header_is_no_op() {
        let buf = [0u8; 8]; // shorter than NLMSG_HDR_LEN (16)
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(!has_pending);
        assert!(pending.is_empty());
    }

    /// AC: Unknown message type is silently ignored.
    #[test]
    fn test_process_buffer_ignores_unknown_message_type() {
        let unknown_type: u16 = 9999;
        let msg_len = NLMSG_HDR_LEN + IFINFOMSG_LEN;
        let mut buf = vec![0u8; msg_len];
        buf[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes());
        buf[4..6].copy_from_slice(&unknown_type.to_ne_bytes());

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(!has_pending, "unknown message type must not set has_pending");
        assert!(pending.is_empty());
    }

    // ── AC: Monitor detects link attribute changes (RTM_NEWLINK) ──────────────

    /// AC: RTM_NEWLINK sets has_pending.
    #[test]
    fn test_process_buffer_rtm_newlink_sets_has_pending() {
        let buf = build_link_msg(RTM_NEWLINK, 5, "eth0");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(has_pending, "RTM_NEWLINK must set has_pending = true");
    }

    /// AC: RTM_NEWLINK correctly extracts ifindex and ifname.
    #[test]
    fn test_process_buffer_rtm_newlink_extracts_ifindex_and_ifname() {
        let buf = build_link_msg(RTM_NEWLINK, 5, "eth0");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(pending.contains_key(&5), "pending must contain ifindex 5");
        let (ifname, kinds) = &pending[&5];
        assert_eq!(ifname.as_deref(), Some("eth0"), "ifname must be eth0");
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::LinkChanged)),
            "kind must be LinkChanged"
        );
    }

    /// AC: RTM_DELLINK also produces LinkChanged (link deletion is a link change).
    #[test]
    fn test_process_buffer_rtm_dellink_produces_link_changed() {
        let buf = build_link_msg(RTM_DELLINK, 3, "eth0");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending);
        assert!(pending.contains_key(&3));
        let (_, kinds) = &pending[&3];
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::LinkChanged)));
    }

    // ── AC: Monitor detects address additions (RTM_NEWADDR) ───────────────────

    /// AC: RTM_NEWADDR produces AddressAdded kind.
    #[test]
    fn test_process_buffer_rtm_newaddr_produces_address_added() {
        let buf = build_addr_msg(RTM_NEWADDR, 7);
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending);
        assert!(pending.contains_key(&7));
        let (_, kinds) = &pending[&7];
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::AddressAdded)),
            "RTM_NEWADDR must produce AddressAdded"
        );
    }

    // ── AC: Monitor detects address removals (RTM_DELADDR) ────────────────────

    /// AC: RTM_DELADDR produces AddressRemoved kind.
    #[test]
    fn test_process_buffer_rtm_deladdr_produces_address_removed() {
        let buf = build_addr_msg(RTM_DELADDR, 8);
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending);
        assert!(pending.contains_key(&8));
        let (_, kinds) = &pending[&8];
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::AddressRemoved)),
            "RTM_DELADDR must produce AddressRemoved"
        );
    }

    // ── AC: Name cache resolves ifname for address messages ───────────────────

    /// AC: RTM_NEWLINK populates the name cache.
    #[test]
    fn test_rtm_newlink_populates_name_cache() {
        let buf = build_link_msg(RTM_NEWLINK, 9, "eth1");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert_eq!(
            cache.get(&9).map(String::as_str),
            Some("eth1"),
            "name cache must map ifindex 9 → eth1"
        );
    }

    /// AC: The name cache resolves ifname for RTM_NEWADDR messages that lack IFLA_IFNAME.
    #[test]
    fn test_name_cache_resolves_ifname_for_subsequent_addr_messages() {
        // NEWLINK populates cache, then NEWADDR uses it.
        let mut buf = build_link_msg(RTM_NEWLINK, 9, "eth1");
        buf.extend(build_addr_msg(RTM_NEWADDR, 9));

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        let (ifname, _) = &pending[&9];
        assert_eq!(
            ifname.as_deref(),
            Some("eth1"),
            "addr message ifname must be resolved from name cache"
        );
    }

    // ── AC: Burst changes are coalesced ───────────────────────────────────────

    /// AC: Multiple RTM_NEWLINK messages for the same ifindex accumulate in one pending entry.
    #[test]
    fn test_process_buffer_coalesces_multiple_newlink_for_same_ifindex() {
        let msg1 = build_link_msg(RTM_NEWLINK, 3, "veth0");
        let msg2 = build_link_msg(RTM_NEWLINK, 3, "veth0");
        let mut buf = msg1;
        buf.extend(msg2);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert_eq!(pending.len(), 1, "two events for the same ifindex must produce 1 pending entry");
        assert!(pending.contains_key(&3));
    }

    /// AC: Events for different ifindexes produce separate pending entries.
    #[test]
    fn test_process_buffer_accumulates_different_ifindexes_separately() {
        let msg1 = build_link_msg(RTM_NEWLINK, 1, "veth0");
        let msg2 = build_link_msg(RTM_NEWLINK, 2, "veth1");
        let mut buf = msg1;
        buf.extend(msg2);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert_eq!(pending.len(), 2, "events for distinct ifindexes must produce separate pending entries");
        assert!(pending.contains_key(&1));
        assert!(pending.contains_key(&2));
    }

    /// AC: An addr message and a link message for the same ifindex accumulate in one entry.
    #[test]
    fn test_process_buffer_link_and_addr_same_ifindex_single_pending_entry() {
        let link_msg = build_link_msg(RTM_NEWLINK, 4, "veth0");
        let addr_msg = build_addr_msg(RTM_NEWADDR, 4);
        let mut buf = link_msg;
        buf.extend(addr_msg);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert_eq!(pending.len(), 1, "link and addr for same ifindex must share one pending entry");
        let (_, kinds) = &pending[&4];
        // Both LinkChanged and AddressAdded should be present
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::LinkChanged)));
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::AddressAdded)));
    }

    // ── AC: Debounce deadline is set when events arrive ────────────────────────

    /// AC: process_buffer resets the debounce deadline to ~500ms from now on receipt.
    #[test]
    fn test_process_buffer_sets_debounce_deadline_when_event_arrives() {
        let buf = build_link_msg(RTM_NEWLINK, 5, "eth0");
        let (mut pending, mut cache, mut has_pending, mut _old_deadline) = empty_state();
        let far = far_future();
        let mut deadline = far;

        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(
            deadline < far,
            "debounce deadline must be reset to ~DEBOUNCE_MS from now, not far future"
        );
        // The deadline should be within ~600ms from now (DEBOUNCE_MS + small margin).
        assert!(
            deadline <= Instant::now() + Duration::from_millis(DEBOUNCE_MS + 100),
            "debounce deadline should be within ~600ms from now"
        );
    }

    /// AC: Debounce deadline is NOT changed for an empty or unrecognized buffer.
    #[test]
    fn test_process_buffer_does_not_reset_deadline_for_unrecognized_messages() {
        let far = far_future();
        let mut deadline = far;
        let (mut pending, mut cache, mut has_pending, _) = empty_state();

        process_buffer(&[], &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert_eq!(deadline, far, "unrecognized buffer must not change the debounce deadline");
    }

    // ── AC: Edge cases: invalid ifindex values ─────────────────────────────────

    /// AC: RTM_NEWLINK with ifindex=0 is silently ignored (invalid interface index).
    #[test]
    fn test_process_buffer_link_message_with_zero_ifindex_is_ignored() {
        let buf = build_link_msg(RTM_NEWLINK, 0, "lo");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(!has_pending, "link message with ifindex=0 must be ignored");
        assert!(pending.is_empty(), "no pending entries expected for ifindex=0");
    }

    /// AC: RTM_NEWLINK with a negative ifindex is silently ignored.
    #[test]
    fn test_process_buffer_link_message_with_negative_ifindex_is_ignored() {
        let buf = build_link_msg(RTM_NEWLINK, -1, "eth0");
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);
        assert!(!has_pending, "link message with negative ifindex must be ignored");
        assert!(pending.is_empty(), "no pending entries expected for negative ifindex");
    }

    // ── AC: Address message before link message — name resolved via backfill ────

    /// AC: When an address event arrives before a link event for the same ifindex,
    /// the pending entry's ifname is updated ("backfilled") once the link message
    /// arrives. This ensures the monitor can always associate a name with changes.
    #[test]
    fn test_addr_then_link_for_same_ifindex_backfills_ifname_from_link_message() {
        // Address message arrives first — no name is available yet.
        let addr_msg = build_addr_msg(RTM_NEWADDR, 10);
        // Link message arrives second — it supplies the name.
        let link_msg = build_link_msg(RTM_NEWLINK, 10, "eth2");
        let mut buf = addr_msg;
        buf.extend(link_msg);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending, "events must set has_pending");
        assert!(pending.contains_key(&10), "ifindex 10 must be in pending");
        let (ifname, kinds) = &pending[&10];
        assert_eq!(
            ifname.as_deref(),
            Some("eth2"),
            "ifname must be backfilled from the subsequent link message"
        );
        // Both an address event and a link event must be recorded for the same interface.
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::AddressAdded)),
            "AddressAdded kind must be present"
        );
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::LinkChanged)),
            "LinkChanged kind must be present"
        );
    }

    // ── AC: Burst changes — same-kind accumulation before drain deduplication ───

    /// AC: Burst changes are coalesced — two RTM_NEWLINK events for the same interface
    /// accumulate in one pending entry (verified here). The deduplication of identical
    /// ChangeKind variants happens at drain time in the async task.
    #[test]
    fn test_process_buffer_same_kind_for_same_ifindex_accumulates_before_drain() {
        let msg1 = build_link_msg(RTM_NEWLINK, 5, "eth0");
        let msg2 = build_link_msg(RTM_NEWLINK, 5, "eth0");
        let mut buf = msg1;
        buf.extend(msg2);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        // Both events coalesce into a single pending entry for ifindex 5.
        assert_eq!(pending.len(), 1, "two link events for the same ifindex → one pending entry");
        assert!(pending.contains_key(&5));
        let (_, kinds) = &pending[&5];
        // Two events are recorded before drain-time deduplication.
        assert_eq!(kinds.len(), 2, "both events are accumulated before the debounce drain");
    }

    // ── AC: Monitor ignores unmanaged interfaces — only managed ifaces emit ─────

    /// AC: Address message for ifindex that has no associated name (no NEWLINK seen,
    /// no name cache entry) leaves ifname=None in the pending entry. The daemon's
    /// event loop uses the ifname to filter unmanaged interfaces; None causes skip.
    #[test]
    fn test_process_buffer_addr_msg_with_unknown_ifindex_has_none_ifname() {
        // No preceding NEWLINK for ifindex 99 → name cache is empty for it.
        let addr_msg = build_addr_msg(RTM_NEWADDR, 99);
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&addr_msg, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending, "addr event must set has_pending");
        assert!(pending.contains_key(&99));
        let (ifname, _) = &pending[&99];
        assert!(
            ifname.is_none(),
            "ifname must be None when no link message has been seen for this ifindex"
        );
    }

    // ── Route message test helpers ────────────────────────────────────────────

    /// Build an RTM_NEWROUTE or RTM_DELROUTE buffer with an RTA_OIF attribute.
    ///
    /// Layout: nlmsghdr(16) + rtmsg(12) + RTA_OIF nlattr(8: nla_len=8,nla_type=7,ifindex u32)
    fn build_route_msg(msg_type: u16, oif_ifindex: u32) -> Vec<u8> {
        let attr_len: usize = 8; // nla_len(2) + nla_type(2) + u32(4)
        let msg_len = NLMSG_HDR_LEN + RTMSG_LEN + attr_len;
        let mut buf = vec![0u8; msg_len];

        // nlmsghdr
        buf[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes());
        buf[4..6].copy_from_slice(&msg_type.to_ne_bytes());

        // rtmsg is all zeros (family=AF_UNSPEC, etc.) — valid for testing

        // RTA_OIF nlattr at offset NLMSG_HDR_LEN + RTMSG_LEN
        let attr_off = NLMSG_HDR_LEN + RTMSG_LEN;
        buf[attr_off..attr_off + 2].copy_from_slice(&(attr_len as u16).to_ne_bytes()); // nla_len
        buf[attr_off + 2..attr_off + 4].copy_from_slice(&RTA_OIF.to_ne_bytes()); // nla_type
        buf[attr_off + 4..attr_off + 8].copy_from_slice(&oif_ifindex.to_ne_bytes()); // ifindex

        buf
    }

    // ── AC: ChangeKind route variants ────────────────────────────────────────

    #[test]
    fn test_change_kind_route_added_variant_exists() {
        assert!(matches!(ChangeKind::RouteAdded, ChangeKind::RouteAdded));
    }

    #[test]
    fn test_change_kind_route_removed_variant_exists() {
        assert!(matches!(ChangeKind::RouteRemoved, ChangeKind::RouteRemoved));
    }

    // ── AC: kind_eq for route variants ────────────────────────────────────────

    #[test]
    fn test_kind_eq_route_added_equals_route_added() {
        assert!(kind_eq(&ChangeKind::RouteAdded, &ChangeKind::RouteAdded));
    }

    #[test]
    fn test_kind_eq_route_removed_equals_route_removed() {
        assert!(kind_eq(&ChangeKind::RouteRemoved, &ChangeKind::RouteRemoved));
    }

    #[test]
    fn test_kind_eq_route_added_not_equal_to_route_removed() {
        assert!(!kind_eq(&ChangeKind::RouteAdded, &ChangeKind::RouteRemoved));
        assert!(!kind_eq(&ChangeKind::RouteRemoved, &ChangeKind::RouteAdded));
    }

    #[test]
    fn test_kind_eq_route_variants_not_equal_to_link_or_address() {
        assert!(!kind_eq(&ChangeKind::RouteAdded, &ChangeKind::LinkChanged));
        assert!(!kind_eq(&ChangeKind::RouteAdded, &ChangeKind::AddressAdded));
        assert!(!kind_eq(&ChangeKind::RouteAdded, &ChangeKind::AddressRemoved));
        assert!(!kind_eq(&ChangeKind::RouteRemoved, &ChangeKind::LinkChanged));
        assert!(!kind_eq(&ChangeKind::RouteRemoved, &ChangeKind::AddressAdded));
        assert!(!kind_eq(&ChangeKind::RouteRemoved, &ChangeKind::AddressRemoved));
    }

    // ── AC: process_buffer handles RTM_NEWROUTE ───────────────────────────────

    #[test]
    fn test_process_buffer_rtm_newroute_produces_route_added() {
        let buf = build_route_msg(RTM_NEWROUTE, 5);
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending, "RTM_NEWROUTE must set has_pending");
        assert!(pending.contains_key(&5), "pending must contain oif ifindex 5");
        let (_, kinds) = &pending[&5];
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::RouteAdded)),
            "RTM_NEWROUTE must produce RouteAdded"
        );
    }

    // ── AC: process_buffer handles RTM_DELROUTE ───────────────────────────────

    #[test]
    fn test_process_buffer_rtm_delroute_produces_route_removed() {
        let buf = build_route_msg(RTM_DELROUTE, 5);
        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending, "RTM_DELROUTE must set has_pending");
        assert!(pending.contains_key(&5));
        let (_, kinds) = &pending[&5];
        assert!(
            kinds.iter().any(|k| matches!(k, ChangeKind::RouteRemoved)),
            "RTM_DELROUTE must produce RouteRemoved"
        );
    }

    // ── AC: Route message without RTA_OIF is ignored ──────────────────────────

    #[test]
    fn test_process_buffer_route_message_without_rta_oif_is_ignored() {
        // Build a route message with no attributes (just nlmsghdr + rtmsg).
        let msg_len = NLMSG_HDR_LEN + RTMSG_LEN;
        let mut buf = vec![0u8; msg_len];
        buf[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes());
        buf[4..6].copy_from_slice(&RTM_NEWROUTE.to_ne_bytes());

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(!has_pending, "route message without RTA_OIF must be ignored");
        assert!(pending.is_empty());
    }

    // ── AC: Name cache resolves ifname for route messages ────────────────────

    #[test]
    fn test_name_cache_resolves_ifname_for_route_message() {
        // RTM_NEWLINK for ifindex=5 → populates name cache with "eth5"
        let link_msg = build_link_msg(RTM_NEWLINK, 5, "eth5");
        // RTM_NEWROUTE with RTA_OIF=5 → should resolve ifname from cache
        let route_msg = build_route_msg(RTM_NEWROUTE, 5);
        let mut buf = link_msg;
        buf.extend(route_msg);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert!(has_pending);
        assert!(pending.contains_key(&5));
        let (ifname, kinds) = &pending[&5];
        assert_eq!(
            ifname.as_deref(),
            Some("eth5"),
            "route message ifname must be resolved from name cache"
        );
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::RouteAdded)));
    }

    // ── AC: Route and link changes coalesce into one pending entry ────────────

    #[test]
    fn test_route_and_link_changes_coalesce_into_one_pending_entry() {
        let link_msg = build_link_msg(RTM_NEWLINK, 7, "eth7");
        let route_msg = build_route_msg(RTM_NEWROUTE, 7);
        let mut buf = link_msg;
        buf.extend(route_msg);

        let (mut pending, mut cache, mut has_pending, mut deadline) = empty_state();
        process_buffer(&buf, &mut pending, &mut cache, &mut has_pending, &mut deadline);

        assert_eq!(pending.len(), 1, "link and route for same ifindex must share one pending entry");
        let (_, kinds) = &pending[&7];
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::LinkChanged)));
        assert!(kinds.iter().any(|k| matches!(k, ChangeKind::RouteAdded)));
    }
}
