//! Router Advertisement parsing and ICMPv6 socket management.
//!
//! Implements userspace RA processing per RFC 4861 (RA/RS), RFC 8106 (RDNSS/DNSSL),
//! RFC 4191 (Route Information), and RFC 8781 (PREF64/NAT64 prefix).

use std::ffi::CString;
use std::net::{Ipv6Addr, SocketAddrV6};

use ipnetwork::Ipv6Network;
use socket2::{Domain, Protocol, Socket, Type};

// ── ICMPv6 types ─────────────────────────────────────────────────────────────

const ICMPV6_TYPE_RS: u8 = 133;
const ICMPV6_TYPE_RA: u8 = 134;

// ── RA option types (RFC 4861 / RFC 8106 / RFC 4191 / RFC 8781) ──────────────

const OPT_SOURCE_LINK_LAYER_ADDR: u8 = 1;
const OPT_MTU: u8 = 5;
const OPT_ROUTE_INFO: u8 = 24;
const OPT_RDNSS: u8 = 25;
const OPT_DNSSL: u8 = 31;
const OPT_PREF64: u8 = 38;
const OPT_PREFIX_INFO: u8 = 3;

// ── Parsed RA structures ──────────────────────────────────────────────────────

/// A fully parsed Router Advertisement message.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RaMessage {
    /// Recommended hop limit for outgoing packets.
    pub hop_limit: u8,
    /// M flag: router expects stateful DHCPv6 for address assignment.
    pub m_flag: bool,
    /// O flag: router expects stateless DHCPv6 for other configuration.
    pub o_flag: bool,
    /// Router Lifetime in seconds (0 = not a default router).
    pub router_lifetime: u16,
    /// Source link-local address of the sending router.
    pub source: Ipv6Addr,
    /// Parsed RA options.
    pub options: Vec<RaOption>,
}

/// A single parsed RA option.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RaOption {
    PrefixInfo {
        prefix: Ipv6Network,
        preferred_lft: u32,
        valid_lft: u32,
        on_link: bool,
        autonomous: bool,
    },
    Rdnss {
        addresses: Vec<Ipv6Addr>,
        lifetime: u32,
    },
    Dnssl {
        domains: Vec<String>,
        lifetime: u32,
    },
    RouteInfo {
        prefix: Ipv6Network,
        preference: u8,
        lifetime: u32,
    },
    Pref64 {
        prefix: Ipv6Network,
        lifetime: u32,
    },
    Mtu(u32),
    SourceLinkLayerAddress([u8; 6]),
}

// ── Socket management ─────────────────────────────────────────────────────────

/// Open an ICMPv6 raw socket bound to `interface`, filtered to receive only
/// Router Advertisement messages (ICMPv6 type 134).
///
/// Uses `AF_INET6/SOCK_RAW/IPPROTO_ICMPV6` with:
/// - `SO_BINDTODEVICE` to restrict reception to the named interface.
/// - `ICMP6_FILTER` to pass only type 134 (RA), blocking all others.
/// - Non-blocking mode for async operation via `AsyncFd`.
pub fn open_ra_socket(interface: &str) -> std::io::Result<Socket> {
    let socket = Socket::new(
        Domain::IPV6,
        Type::RAW,
        Some(Protocol::from(libc::IPPROTO_ICMPV6)),
    )?;

    // Bind to interface so we only receive packets from that link.
    let iface_cstr = CString::new(interface)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid interface name"))?;
    socket.bind_device(Some(iface_cstr.as_bytes()))?;

    // Set ICMP6_FILTER to accept only RA (type 134), block everything else.
    // icmp6_filter: 256 bits where bit N=1 means block type N, bit N=0 means pass.
    // ICMP6_FILTER = 1 (Linux kernel constant; not yet exported by libc 0.2.x).
    const ICMP6_FILTER: libc::c_int = 1;
    let mut filter = [!0u32; 8]; // All 256 types blocked.
    // Pass RA (type 134): unset bit 134.
    filter[134 / 32] &= !(1u32 << (134 % 32));
    let ret = unsafe {
        libc::setsockopt(
            std::os::unix::io::AsRawFd::as_raw_fd(&socket),
            libc::IPPROTO_ICMPV6,
            ICMP6_FILTER,
            filter.as_ptr() as *const libc::c_void,
            std::mem::size_of_val(&filter) as libc::socklen_t,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    socket.set_nonblocking(true)?;

    Ok(socket)
}

/// Send a Router Solicitation (ICMPv6 type 133) to the all-routers multicast
/// address `ff02::2` on the interface identified by `ifindex`.
///
/// The kernel fills in the ICMPv6 checksum automatically for IPPROTO_ICMPV6
/// raw sockets. The source address is determined by the kernel's routing table
/// (typically the interface's link-local address).
pub fn send_router_solicitation(socket: &Socket, ifindex: u32) -> std::io::Result<()> {
    // RS message: type(133) + code(0) + checksum(0, kernel fills) + reserved(0) = 8 bytes.
    let rs: [u8; 8] = [ICMPV6_TYPE_RS, 0, 0, 0, 0, 0, 0, 0];

    // Destination: ff02::2 (all-routers multicast), scope_id = ifindex for link-local.
    let dest = SocketAddrV6::new(
        Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 2),
        0,
        0,
        ifindex,
    );
    let dest_sock = socket2::SockAddr::from(dest);

    socket.send_to(&rs, &dest_sock)?;
    Ok(())
}

// ── RA parsing ────────────────────────────────────────────────────────────────

/// Parse a raw ICMPv6 RA packet buffer into an `RaMessage`.
///
/// `buf` is the ICMPv6 payload (starting at the type byte). `src` is the
/// sender's IPv6 address from the socket's `recvfrom` call.
///
/// Returns `None` if the buffer is too short or the ICMPv6 type is not 134 (RA).
pub fn parse_ra(buf: &[u8], src: Ipv6Addr) -> Option<RaMessage> {
    // RA minimum length: 4 bytes ICMPv6 header + 12 bytes RA-specific = 16 bytes.
    if buf.len() < 16 {
        return None;
    }
    if buf[0] != ICMPV6_TYPE_RA {
        return None;
    }

    let hop_limit = buf[4];
    let flags_byte = buf[5];
    let m_flag = (flags_byte & 0x80) != 0;
    let o_flag = (flags_byte & 0x40) != 0;
    let router_lifetime = u16::from_be_bytes([buf[6], buf[7]]);
    // Bytes 8-11: reachable time; 12-15: retrans timer (informational, not used).

    // Parse TLV options starting at offset 16.
    let options = parse_options(&buf[16..]);

    Some(RaMessage {
        hop_limit,
        m_flag,
        o_flag,
        router_lifetime,
        source: src,
        options,
    })
}

/// Parse RA TLV options from a byte slice. Unknown options are silently skipped.
fn parse_options(mut data: &[u8]) -> Vec<RaOption> {
    let mut options = Vec::new();

    while data.len() >= 2 {
        let opt_type = data[0];
        let opt_len_units = data[1] as usize; // Length in 8-byte units.
        if opt_len_units == 0 {
            break; // Length 0 is invalid; stop parsing.
        }
        let opt_len_bytes = opt_len_units * 8;
        if data.len() < opt_len_bytes {
            break; // Truncated option; stop parsing.
        }

        let opt_data = &data[2..opt_len_bytes]; // Option data (after type+length bytes).

        match opt_type {
            OPT_PREFIX_INFO => {
                if let Some(opt) = parse_prefix_info(opt_data) {
                    options.push(opt);
                }
            }
            OPT_RDNSS => {
                if let Some(opt) = parse_rdnss(opt_data) {
                    options.push(opt);
                }
            }
            OPT_DNSSL => {
                if let Some(opt) = parse_dnssl(opt_data) {
                    options.push(opt);
                }
            }
            OPT_ROUTE_INFO => {
                if let Some(opt) = parse_route_info(opt_data) {
                    options.push(opt);
                }
            }
            OPT_PREF64 => {
                if let Some(opt) = parse_pref64(opt_data) {
                    options.push(opt);
                }
            }
            OPT_MTU if opt_data.len() >= 6 => {
                let mtu = u32::from_be_bytes([
                    opt_data[2], opt_data[3], opt_data[4], opt_data[5],
                ]);
                options.push(RaOption::Mtu(mtu));
            }
            OPT_SOURCE_LINK_LAYER_ADDR if opt_data.len() >= 6 => {
                let mac = [
                    opt_data[0], opt_data[1], opt_data[2],
                    opt_data[3], opt_data[4], opt_data[5],
                ];
                options.push(RaOption::SourceLinkLayerAddress(mac));
            }
            _ => {} // Unknown option; skip.
        }

        data = &data[opt_len_bytes..];
    }

    options
}

/// Parse a Prefix Information option (RFC 4861 §4.6.2).
///
/// The option body (excluding the 2-byte type+length header) is 30 bytes:
/// - Prefix Length (1)
/// - L/A/reserved flags (1)
/// - Valid Lifetime (4)
/// - Preferred Lifetime (4)
/// - Reserved (4)
/// - Prefix (16)
fn parse_prefix_info(data: &[u8]) -> Option<RaOption> {
    // data starts right after the 2-byte type+length field.
    // We need at least 30 bytes (32 - 2 type+length bytes).
    if data.len() < 30 {
        return None;
    }
    let prefix_len = data[0];
    let flags = data[1];
    let on_link = (flags & 0x80) != 0;
    let autonomous = (flags & 0x40) != 0;
    let valid_lft = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let preferred_lft = u32::from_be_bytes([data[6], data[7], data[8], data[9]]);
    // Bytes 10-13: reserved.
    let prefix_bytes: [u8; 16] = data[14..30].try_into().ok()?;
    let prefix_addr = Ipv6Addr::from(prefix_bytes);

    let network = Ipv6Network::new(prefix_addr, prefix_len).ok()?;

    Some(RaOption::PrefixInfo {
        prefix: network,
        preferred_lft,
        valid_lft,
        on_link,
        autonomous,
    })
}

/// Parse an RDNSS option (RFC 8106).
///
/// Format (data after type+length):
/// - Reserved (2)
/// - Lifetime (4)
/// - N × 16-byte IPv6 addresses
fn parse_rdnss(data: &[u8]) -> Option<RaOption> {
    if data.len() < 6 {
        return None;
    }
    let lifetime = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let addr_data = &data[6..];
    if !addr_data.len().is_multiple_of(16) {
        return None;
    }
    let addresses: Vec<Ipv6Addr> = addr_data
        .chunks(16)
        .filter_map(|chunk| {
            let bytes: [u8; 16] = chunk.try_into().ok()?;
            Some(Ipv6Addr::from(bytes))
        })
        .collect();
    if addresses.is_empty() {
        return None;
    }
    Some(RaOption::Rdnss { addresses, lifetime })
}

/// Parse a DNSSL option (RFC 8106).
///
/// Format (data after type+length):
/// - Reserved (2)
/// - Lifetime (4)
/// - DNS-encoded domain name list (NUL-terminated labels, padded to 8-byte boundary)
fn parse_dnssl(data: &[u8]) -> Option<RaOption> {
    if data.len() < 6 {
        return None;
    }
    let lifetime = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let name_data = &data[6..];
    let domains = parse_dns_name_list(name_data);
    if domains.is_empty() {
        return None;
    }
    Some(RaOption::Dnssl { domains, lifetime })
}

/// Parse a DNS-encoded domain name list as used in DNSSL options.
///
/// Each domain name is a sequence of length-prefixed labels ending with a
/// zero-length label (NUL byte). Multiple names are concatenated. Padding NUL
/// bytes after the final domain are ignored.
fn parse_dns_name_list(mut data: &[u8]) -> Vec<String> {
    let mut domains = Vec::new();

    while !data.is_empty() {
        let mut labels: Vec<String> = Vec::new();
        let mut advance = 0;

        loop {
            if advance >= data.len() {
                break;
            }
            let label_len = data[advance] as usize;
            advance += 1;
            if label_len == 0 {
                break; // End of this domain name.
            }
            if advance + label_len > data.len() {
                break; // Truncated; stop.
            }
            let label = std::str::from_utf8(&data[advance..advance + label_len])
                .unwrap_or("")
                .to_string();
            labels.push(label);
            advance += label_len;
        }

        data = &data[advance..];

        if !labels.is_empty() {
            domains.push(labels.join("."));
        } else {
            // All-zero padding at end; stop.
            break;
        }
    }

    domains
}

/// Parse a Route Information option (RFC 4191).
///
/// Format (data after type+length, variable 6/14/22 bytes):
/// - Prefix Length (1)
/// - Pref/Reserved flags (1)
/// - Route Lifetime (4)
/// - Prefix (0/8/16 bytes depending on option length)
fn parse_route_info(data: &[u8]) -> Option<RaOption> {
    if data.len() < 6 {
        return None;
    }
    let prefix_len = data[0];
    let flags = data[1];
    let preference = (flags >> 3) & 0x03;
    let lifetime = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);

    // Prefix bytes follow. Length depends on option length field (0, 8, or 16 bytes).
    let prefix_bytes_slice = &data[6..];
    let prefix_addr = if prefix_bytes_slice.is_empty() {
        Ipv6Addr::UNSPECIFIED
    } else if prefix_bytes_slice.len() >= 16 {
        let bytes: [u8; 16] = prefix_bytes_slice[..16].try_into().ok()?;
        Ipv6Addr::from(bytes)
    } else if prefix_bytes_slice.len() >= 8 {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&prefix_bytes_slice[..8]);
        Ipv6Addr::from(bytes)
    } else {
        Ipv6Addr::UNSPECIFIED
    };

    let network = Ipv6Network::new(prefix_addr, prefix_len).ok()?;
    Some(RaOption::RouteInfo {
        prefix: network,
        preference,
        lifetime,
    })
}

/// Parse a PREF64 (NAT64 prefix) option (RFC 8781).
///
/// Format (data after type+length, 14 bytes total - 2 = 12 bytes):
/// - Scaled Lifetime/PLC (2 bytes): upper 13 bits = lifetime/8, lower 3 bits = PLC
/// - Prefix (12 bytes, representing the /32 to /96 prefix)
///
/// PLC (Prefix Length Code) encodes the prefix length:
///   0 → /96, 1 → /64, 2 → /56, 3 → /48, 4 → /40, 5 → /32
fn parse_pref64(data: &[u8]) -> Option<RaOption> {
    // data is after type+length bytes. PREF64 option is 16 bytes total, so data is 14 bytes.
    if data.len() < 12 {
        return None;
    }
    let scaled = u16::from_be_bytes([data[0], data[1]]);
    let lifetime_scaled = scaled >> 3;
    let lifetime = (lifetime_scaled as u32) * 8;
    let plc = (scaled & 0x07) as u8;

    let prefix_len: u8 = match plc {
        0 => 96,
        1 => 64,
        2 => 56,
        3 => 48,
        4 => 40,
        5 => 32,
        _ => return None, // Reserved PLC value.
    };

    // Extract prefix bytes. RFC 8781: the prefix field is 12 bytes, covering up to /96.
    let mut prefix_bytes = [0u8; 16];
    let copy_len = (prefix_len as usize).div_ceil(8);
    let src_len = copy_len.min(data.len() - 2).min(12);
    prefix_bytes[..src_len].copy_from_slice(&data[2..2 + src_len]);

    let prefix_addr = Ipv6Addr::from(prefix_bytes);
    let network = Ipv6Network::new(prefix_addr, prefix_len).ok()?;

    Some(RaOption::Pref64 { prefix: network, lifetime })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Construct a minimal 16-byte RA packet with no options.
    fn make_ra_header(hop_limit: u8, m: bool, o: bool, router_lifetime: u16) -> Vec<u8> {
        let flags = if m { 0x80 } else { 0 } | if o { 0x40 } else { 0 };
        vec![
            134, 0, 0, 0,               // type=RA, code=0, checksum=0
            hop_limit, flags,            // hop limit, M/O flags
            (router_lifetime >> 8) as u8, (router_lifetime & 0xff) as u8, // router lifetime
            0, 0, 0, 0,                  // reachable time
            0, 0, 0, 0,                  // retrans timer
        ]
    }

    /// Append a Prefix Information option (type=3, length=4 units=32 bytes).
    fn append_prefix_info(
        buf: &mut Vec<u8>,
        prefix_len: u8,
        on_link: bool,
        autonomous: bool,
        valid_lft: u32,
        preferred_lft: u32,
        prefix: Ipv6Addr,
    ) {
        let flags = if on_link { 0x80 } else { 0 } | if autonomous { 0x40 } else { 0 };
        buf.push(3); // type
        buf.push(4); // length = 4 * 8 = 32 bytes
        buf.push(prefix_len);
        buf.push(flags);
        buf.extend_from_slice(&valid_lft.to_be_bytes());
        buf.extend_from_slice(&preferred_lft.to_be_bytes());
        buf.extend_from_slice(&[0u8; 4]); // reserved
        buf.extend_from_slice(&prefix.octets());
    }

    /// Append an RDNSS option (type=25) with a single IPv6 address.
    fn append_rdnss(buf: &mut Vec<u8>, lifetime: u32, addr: Ipv6Addr) {
        buf.push(25); // type
        buf.push(3);  // length = 3 * 8 = 24 bytes (2 reserved + 4 lifetime + 16 addr)
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&lifetime.to_be_bytes());
        buf.extend_from_slice(&addr.octets());
    }

    /// Append a DNSSL option (type=31) with a single domain.
    /// The domain is DNS-encoded (length-prefixed labels, NUL-terminated).
    fn append_dnssl(buf: &mut Vec<u8>, lifetime: u32, domain: &str) {
        let mut name_bytes: Vec<u8> = Vec::new();
        for label in domain.split('.') {
            name_bytes.push(label.len() as u8);
            name_bytes.extend_from_slice(label.as_bytes());
        }
        name_bytes.push(0); // NUL terminator

        // Total option = type(1) + length(1) + reserved(2) + lifetime(4) + name_bytes + padding.
        // length field counts all bytes including type+length, in 8-byte units.
        let header_and_content = 8 + name_bytes.len();
        let total_bytes = header_and_content.div_ceil(8) * 8;
        let pad = total_bytes - header_and_content;
        let length_units = total_bytes / 8;

        buf.push(31); // type
        buf.push(length_units as u8);
        buf.extend_from_slice(&[0u8; 2]); // reserved
        buf.extend_from_slice(&lifetime.to_be_bytes());
        buf.extend_from_slice(&name_bytes);
        buf.extend(std::iter::repeat(0u8).take(pad));
    }

    /// Append a PREF64 option (type=38, length=2 units=16 bytes) for a /96 prefix.
    fn append_pref64_96(buf: &mut Vec<u8>, lifetime_secs: u32, prefix: Ipv6Addr) {
        // PLC=0 for /96; scaled = (lifetime_secs/8) << 3 | 0
        let lifetime_scaled = (lifetime_secs / 8) as u16;
        let scaled = lifetime_scaled << 3; // PLC=0
        buf.push(38); // type
        buf.push(2);  // length = 2 * 8 = 16 bytes
        buf.extend_from_slice(&scaled.to_be_bytes());
        // First 12 bytes of prefix (covers /96)
        buf.extend_from_slice(&prefix.octets()[..12]);
    }

    // ── parse_ra: minimal RA ──────────────────────────────────────────────────

    /// Scenario: parse_ra returns None for a buffer shorter than 16 bytes.
    #[test]
    fn test_parse_ra_too_short_returns_none() {
        let buf = [134u8; 15];
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(parse_ra(&buf, src).is_none(), "buffer < 16 bytes must return None");
    }

    /// Scenario: parse_ra returns None for a non-RA ICMPv6 type.
    #[test]
    fn test_parse_ra_wrong_type_returns_none() {
        let mut buf = make_ra_header(64, false, false, 1800);
        buf[0] = 133; // Router Solicitation, not RA
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(parse_ra(&buf, src).is_none(), "non-RA ICMPv6 type must return None");
    }

    /// Scenario: parse_ra parses minimal valid RA (no options).
    #[test]
    fn test_parse_ra_minimal_valid_ra_parsed() {
        let buf = make_ra_header(64, false, false, 1800);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).expect("minimal 16-byte RA must parse");
        assert_eq!(ra.hop_limit, 64);
        assert!(!ra.m_flag);
        assert!(!ra.o_flag);
        assert_eq!(ra.router_lifetime, 1800);
        assert_eq!(ra.source, src);
        assert!(ra.options.is_empty(), "no options in minimal RA");
    }

    // ── parse_ra: M/O flags ───────────────────────────────────────────────────

    /// Scenario: M flag parsed correctly when set.
    #[test]
    fn test_parse_ra_m_flag_set() {
        let buf = make_ra_header(64, true, false, 1800);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert!(ra.m_flag, "M flag must be true when bit 7 of flags byte is set");
        assert!(!ra.o_flag);
    }

    /// Scenario: O flag parsed correctly when set.
    #[test]
    fn test_parse_ra_o_flag_set() {
        let buf = make_ra_header(64, false, true, 1800);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert!(!ra.m_flag);
        assert!(ra.o_flag, "O flag must be true when bit 6 of flags byte is set");
    }

    /// Scenario: M/O flags are reported to daemon via FactoryEvent (both set)
    #[test]
    fn test_parse_ra_both_mo_flags_set() {
        let buf = make_ra_header(64, true, true, 1800);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert!(ra.m_flag, "M flag must be set");
        assert!(ra.o_flag, "O flag must be set");
    }

    /// Scenario: Neither M nor O flag set.
    #[test]
    fn test_parse_ra_neither_mo_flags_set() {
        let buf = make_ra_header(64, false, false, 1800);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert!(!ra.m_flag);
        assert!(!ra.o_flag);
    }

    /// Scenario: Router lifetime = 0 means router is not a default router.
    #[test]
    fn test_parse_ra_router_lifetime_zero() {
        let buf = make_ra_header(64, false, false, 0);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.router_lifetime, 0, "router_lifetime=0 means not a default router");
    }

    /// Scenario: Router lifetime is parsed as big-endian u16.
    #[test]
    fn test_parse_ra_router_lifetime_big_endian() {
        let buf = make_ra_header(64, false, false, 9000);
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.router_lifetime, 9000);
    }

    // ── parse_ra: Prefix Information option ──────────────────────────────────

    /// Scenario: RA with Prefix Info option (A flag set) is parsed correctly.
    #[test]
    fn test_parse_ra_prefix_info_autonomous_flag() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        append_prefix_info(&mut buf, 64, true, true, 86400, 14400, prefix);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 1);

        match &ra.options[0] {
            RaOption::PrefixInfo {
                prefix: pfx,
                preferred_lft,
                valid_lft,
                on_link,
                autonomous,
            } => {
                assert_eq!(pfx.prefix(), 64);
                assert_eq!(pfx.network().to_string(), "2001:db8::");
                assert_eq!(*valid_lft, 86400);
                assert_eq!(*preferred_lft, 14400);
                assert!(*on_link, "on_link must be true");
                assert!(*autonomous, "autonomous must be true");
            }
            other => panic!("expected PrefixInfo, got {:?}", other),
        }
    }

    /// Scenario: Prefix Info option with A flag clear is still parsed.
    #[test]
    fn test_parse_ra_prefix_info_not_autonomous() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        append_prefix_info(&mut buf, 64, true, false, 86400, 14400, prefix);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 1);
        match &ra.options[0] {
            RaOption::PrefixInfo { autonomous, .. } => {
                assert!(!autonomous, "A flag must be false");
            }
            other => panic!("expected PrefixInfo, got {:?}", other),
        }
    }

    /// Scenario: valid_lft=0 in prefix info is parsed correctly.
    #[test]
    fn test_parse_ra_prefix_info_valid_lft_zero() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        append_prefix_info(&mut buf, 64, true, true, 0, 0, prefix);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        match &ra.options[0] {
            RaOption::PrefixInfo { valid_lft, preferred_lft, .. } => {
                assert_eq!(*valid_lft, 0, "valid_lft=0 must be parsed correctly");
                assert_eq!(*preferred_lft, 0);
            }
            other => panic!("expected PrefixInfo, got {:?}", other),
        }
    }

    // ── parse_ra: RDNSS option ────────────────────────────────────────────────

    /// Scenario: RA with RDNSS option is parsed correctly.
    #[test]
    fn test_parse_ra_rdnss_single_address() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();
        append_rdnss(&mut buf, 3600, dns);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 1);
        match &ra.options[0] {
            RaOption::Rdnss { addresses, lifetime } => {
                assert_eq!(*lifetime, 3600);
                assert_eq!(addresses.len(), 1);
                assert_eq!(addresses[0], dns);
            }
            other => panic!("expected Rdnss, got {:?}", other),
        }
    }

    // ── parse_ra: DNSSL option ────────────────────────────────────────────────

    /// Scenario: RA with DNSSL option parses domain list correctly.
    #[test]
    fn test_parse_ra_dnssl_single_domain() {
        let mut buf = make_ra_header(64, false, false, 1800);
        append_dnssl(&mut buf, 3600, "example.com");

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 1);
        match &ra.options[0] {
            RaOption::Dnssl { domains, lifetime } => {
                assert_eq!(*lifetime, 3600);
                assert_eq!(domains.len(), 1);
                assert_eq!(domains[0], "example.com");
            }
            other => panic!("expected Dnssl, got {:?}", other),
        }
    }

    // ── parse_ra: PREF64 option ───────────────────────────────────────────────

    /// Scenario: RA with PREF64 option parses NAT64 prefix correctly.
    #[test]
    fn test_parse_ra_pref64_96_prefix() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let nat64: Ipv6Addr = "64:ff9b::".parse().unwrap();
        append_pref64_96(&mut buf, 3600, nat64);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 1);
        match &ra.options[0] {
            RaOption::Pref64 { prefix, lifetime } => {
                assert_eq!(prefix.prefix(), 96, "PREF64 prefix length must be 96");
                // Lifetime is quantised to 8s multiples; 3600 is exactly 450*8
                assert_eq!(*lifetime, 3600);
                // Verify prefix address matches
                let expected: Ipv6Addr = "64:ff9b::".parse().unwrap();
                assert_eq!(prefix.network(), expected);
            }
            other => panic!("expected Pref64, got {:?}", other),
        }
    }

    // ── parse_ra: multiple options ────────────────────────────────────────────

    /// Scenario: RA with multiple options (prefix + RDNSS + DNSSL + PREF64) parsed.
    #[test]
    fn test_parse_ra_multiple_options_all_parsed() {
        let mut buf = make_ra_header(64, false, false, 1800);
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        append_prefix_info(&mut buf, 64, true, true, 86400, 14400, prefix);
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();
        append_rdnss(&mut buf, 3600, dns);
        append_dnssl(&mut buf, 3600, "example.com");
        let nat64: Ipv6Addr = "64:ff9b::".parse().unwrap();
        append_pref64_96(&mut buf, 3600, nat64);

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.options.len(), 4, "all four options must be parsed");

        let has_prefix = ra.options.iter().any(|o| matches!(o, RaOption::PrefixInfo { .. }));
        let has_rdnss = ra.options.iter().any(|o| matches!(o, RaOption::Rdnss { .. }));
        let has_dnssl = ra.options.iter().any(|o| matches!(o, RaOption::Dnssl { .. }));
        let has_pref64 = ra.options.iter().any(|o| matches!(o, RaOption::Pref64 { .. }));
        assert!(has_prefix, "PrefixInfo option must be present");
        assert!(has_rdnss, "Rdnss option must be present");
        assert!(has_dnssl, "Dnssl option must be present");
        assert!(has_pref64, "Pref64 option must be present");
    }

    /// Scenario: Unknown option type is silently skipped.
    #[test]
    fn test_parse_ra_unknown_option_skipped() {
        let mut buf = make_ra_header(64, false, false, 1800);
        // Insert a fake option with type 99 (unknown), length 1 unit = 8 bytes.
        buf.push(99);
        buf.push(1); // 1 * 8 = 8 bytes total
        buf.extend_from_slice(&[0u8; 6]); // 6 more bytes = 8 total

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert!(ra.options.is_empty(), "unknown option type must be silently skipped");
    }

    /// Scenario: Truncated option stops parsing without panic.
    #[test]
    fn test_parse_ra_truncated_option_stops_gracefully() {
        let mut buf = make_ra_header(64, false, false, 1800);
        // Claim length=4 (32 bytes) but only provide 10 bytes
        buf.push(3); // Prefix Info type
        buf.push(4); // claims 32 bytes
        buf.extend_from_slice(&[0u8; 10]); // only 10 bytes provided (truncated)

        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        // Truncated option is skipped; no panic.
        assert!(ra.options.is_empty(), "truncated option must be silently skipped");
    }

    /// Scenario: Source address stored in RaMessage matches input src.
    #[test]
    fn test_parse_ra_source_address_preserved() {
        let buf = make_ra_header(64, false, false, 1800);
        let src: Ipv6Addr = "fe80::dead:beef".parse().unwrap();
        let ra = parse_ra(&buf, src).unwrap();
        assert_eq!(ra.source, src, "source address must be preserved from recvfrom");
    }
}
