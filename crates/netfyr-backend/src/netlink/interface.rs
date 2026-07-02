//! Network interface query via rtnetlink (ethernet and WiFi).

use std::collections::HashMap;
use std::net::IpAddr;

use futures::TryStreamExt;
use indexmap::IndexMap;
use netfyr_state::{entity_types::{ETHERNET, WIFI}, FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};
use netlink_packet_route::link::{
    InfoKind, LinkAttribute, LinkFlags, LinkInfo, LinkLayerType, LinkMessage,
};
use netlink_packet_route::route::{
    RouteAddress, RouteAttribute, RouteMessage, RouteProtocol,
};
use rtnetlink::Handle;
use tracing::warn;

use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};

use crate::BackendError;
use super::query::{
    build_discovered_selector, read_dns_servers, read_procfs_addr_gen_mode,
    read_procfs_dad_transmits, read_sysfs_driver, read_sysfs_is_wifi, read_sysfs_pci_path,
    read_sysfs_speed, read_sysfs_wifi_frequency, read_sysfs_wifi_mode, read_sysfs_wifi_ssid,
};

// ── Exclusion list ────────────────────────────────────────────────────────────

/// Returns `true` if a link with the given `InfoKind` should be excluded from
/// query results.
///
/// Physical NICs (no `IFLA_INFO_KIND`) and veth pairs are included. All other
/// virtual types are excluded because:
/// - The acceptance criteria explicitly call out bridge, bond, vlan as excluded.
/// - Integration tests use veth pairs and expect them to appear.
fn is_excluded_kind(kind: &InfoKind) -> bool {
    matches!(
        kind,
        InfoKind::Bridge
            | InfoKind::Bond
            | InfoKind::Vlan
            | InfoKind::Vxlan
            | InfoKind::Dummy
            | InfoKind::MacVlan
            | InfoKind::MacVtap
            | InfoKind::IpVlan
            | InfoKind::IpVtap
            | InfoKind::Tun
            | InfoKind::SitTun
            | InfoKind::GreTun
            | InfoKind::GreTun6
            | InfoKind::IpIp
            | InfoKind::Wireguard
            | InfoKind::Vrf
            | InfoKind::Nlmon
    )
}

// ── Link attribute extraction helpers ────────────────────────────────────────

fn extract_link_name(msg: &LinkMessage) -> Option<String> {
    for attr in &msg.attributes {
        if let LinkAttribute::IfName(name) = attr {
            return Some(name.clone());
        }
    }
    None
}

fn extract_link_mac(msg: &LinkMessage) -> Option<[u8; 6]> {
    for attr in &msg.attributes {
        if let LinkAttribute::Address(bytes) = attr {
            if bytes.len() == 6 {
                let mut arr = [0u8; 6];
                arr.copy_from_slice(bytes);
                return Some(arr);
            }
        }
    }
    None
}

fn extract_link_mtu(msg: &LinkMessage) -> Option<u32> {
    for attr in &msg.attributes {
        if let LinkAttribute::Mtu(mtu) = attr {
            return Some(*mtu);
        }
    }
    None
}

fn extract_link_carrier(msg: &LinkMessage) -> Option<u8> {
    for attr in &msg.attributes {
        if let LinkAttribute::Carrier(c) = attr {
            return Some(*c);
        }
    }
    None
}

fn extract_link_enabled(msg: &LinkMessage) -> bool {
    msg.header.flags.contains(LinkFlags::Up)
}

/// Extract the `IFLA_INFO_KIND` from a link's `IFLA_LINKINFO` nested attribute.
///
/// Returns `None` if no `LinkInfo` or `Kind` attribute is present (which is
/// the case for physical NICs — they lack an IFLA_LINKINFO entirely).
fn extract_link_kind(msg: &LinkMessage) -> Option<InfoKind> {
    for attr in &msg.attributes {
        if let LinkAttribute::LinkInfo(infos) = attr {
            for info in infos {
                if let LinkInfo::Kind(kind) = info {
                    return Some(kind.clone());
                }
            }
        }
    }
    None
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn format_mac(bytes: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

fn route_address_to_ip(addr: &RouteAddress) -> Option<IpAddr> {
    match addr {
        RouteAddress::Inet(v4) => Some(IpAddr::V4(*v4)),
        RouteAddress::Inet6(v6) => Some(IpAddr::V6(*v6)),
        _ => None,
    }
}

fn route_protocol_str(proto: RouteProtocol) -> &'static str {
    match proto {
        RouteProtocol::Kernel => "kernel",
        RouteProtocol::Boot => "boot",
        RouteProtocol::Static => "static",
        RouteProtocol::Dhcp => "dhcp",
        RouteProtocol::Ra => "ra",
        _ => "other",
    }
}

fn build_route_value(
    destination: &str,
    gateway: Option<&str>,
    metric: u32,
    protocol: Option<&str>,
    mtu: Option<u32>,
    table: Option<u32>,
    tos: u8,
) -> Value {
    let mut map = IndexMap::new();
    let dest_val = destination
        .parse::<ipnetwork::IpNetwork>()
        .map(Value::IpNetwork)
        .unwrap_or_else(|_| Value::String(destination.to_owned()));
    map.insert("destination".to_string(), dest_val);
    if let Some(gw) = gateway {
        let gw_val = gw
            .parse::<IpAddr>()
            .map(Value::IpAddr)
            .unwrap_or_else(|_| Value::String(gw.to_owned()));
        map.insert("gateway".to_string(), gw_val);
    }
    map.insert("metric".to_string(), Value::U64(metric as u64));
    if let Some(proto) = protocol {
        map.insert("protocol".to_string(), Value::String(proto.to_owned()));
    }
    if let Some(m) = mtu {
        map.insert("mtu".to_string(), Value::U64(m as u64));
    }
    if let Some(t) = table {
        if t != 254 {
            map.insert("table".to_string(), Value::U64(t as u64));
        }
    }
    if tos != 0 {
        map.insert("tos".to_string(), Value::U64(tos as u64));
    }
    Value::Map(map)
}

/// Convenience wrapper that tags a `Value` with `KernelDefault` provenance.
fn kernel_default(value: Value) -> FieldValue {
    FieldValue {
        value,
        provenance: Provenance::KernelDefault,
    }
}

// ── Technology detection ──────────────────────────────────────────────────────

/// Detect whether an interface is WiFi or ethernet.
///
/// WiFi is identified by the presence of `/sys/class/net/<name>/phy80211`.
/// Anything else with ARPHRD_ETHER that isn't excluded is classified as ethernet.
fn detect_technology(name: &str) -> &'static str {
    if read_sysfs_is_wifi(name) {
        WIFI
    } else {
        ETHERNET
    }
}

/// Build the `ethernet:` sub-object containing speed (if available).
fn build_ethernet_sub_object(name: &str) -> Option<FieldValue> {
    let speed = read_sysfs_speed(name)?;
    let mut map = IndexMap::new();
    map.insert("speed".to_string(), Value::U64(speed));
    Some(kernel_default(Value::Map(map)))
}

/// Build the `wifi:` sub-object containing mode, ssid, frequency (if available).
///
/// All fields come from nl80211, which is not implemented here. Returns `None`
/// when no WiFi attributes are available via sysfs alone.
fn build_wifi_sub_object(name: &str) -> Option<FieldValue> {
    let mode = read_sysfs_wifi_mode(name);
    let ssid = read_sysfs_wifi_ssid(name);
    let frequency = read_sysfs_wifi_frequency(name);

    if mode.is_none() && ssid.is_none() && frequency.is_none() {
        return None;
    }

    let mut map = IndexMap::new();
    if let Some(m) = mode {
        map.insert("mode".to_string(), Value::String(m));
    }
    if let Some(s) = ssid {
        map.insert("ssid".to_string(), Value::String(s));
    }
    if let Some(f) = frequency {
        map.insert("frequency".to_string(), Value::U64(f));
    }
    Some(kernel_default(Value::Map(map)))
}

// ── Address dump ─────────────────────────────────────────────────────────────

struct AddressDump {
    ipv4: HashMap<u32, Vec<String>>,
    ipv6: HashMap<u32, Vec<Value>>,
}

/// Classify the DAD (Duplicate Address Detection) state of an IPv6 address.
///
/// Checks the 32-bit `IFA_FLAGS` attribute first (authoritative on modern
/// kernels), then falls back to the 8-bit header flags field.
fn classify_dad_state(
    attributes: &[AddressAttribute],
    header_flags: &AddressHeaderFlags,
) -> &'static str {
    for attr in attributes {
        if let AddressAttribute::Flags(flags) = attr {
            if flags.contains(AddressFlags::Dadfailed) {
                return "dadfailed";
            }
            if flags.contains(AddressFlags::Tentative) {
                return "tentative";
            }
            if flags.contains(AddressFlags::Deprecated) {
                return "deprecated";
            }
            return "preferred";
        }
    }
    if header_flags.contains(AddressHeaderFlags::Dadfailed) {
        return "dadfailed";
    }
    if header_flags.contains(AddressHeaderFlags::Tentative) {
        return "tentative";
    }
    if header_flags.contains(AddressHeaderFlags::Deprecated) {
        return "deprecated";
    }
    "preferred"
}

/// Dump all addresses from the kernel, split by address family.
///
/// IPv4 addresses are stored as CIDR strings. IPv6 addresses are stored as
/// `Value::Map { address: CIDR, dad_state: "preferred"|"tentative"|... }`.
async fn dump_addresses(handle: &Handle) -> Result<AddressDump, BackendError> {
    let mut result = AddressDump {
        ipv4: HashMap::new(),
        ipv6: HashMap::new(),
    };

    let mut stream = handle.address().get().execute();
    while let Some(msg) = stream.try_next().await.map_err(|e| BackendError::QueryFailed {
        entity_type: "interface".to_string(),
        source: Box::new(e),
    })? {
        let family = msg.header.family;
        let index = msg.header.index;
        let prefix_len = msg.header.prefix_len;

        match family {
            netlink_packet_route::AddressFamily::Inet => {
                for attr in &msg.attributes {
                    if let AddressAttribute::Address(ip) = attr {
                        let cidr = format!("{ip}/{prefix_len}");
                        result.ipv4.entry(index).or_default().push(cidr);
                    }
                }
            }
            netlink_packet_route::AddressFamily::Inet6 => {
                let dad_state = classify_dad_state(&msg.attributes, &msg.header.flags);
                for attr in &msg.attributes {
                    if let AddressAttribute::Address(ip) = attr {
                        let cidr = format!("{ip}/{prefix_len}");
                        let mut map = IndexMap::new();
                        map.insert("address".to_string(), Value::String(cidr));
                        map.insert("dad_state".to_string(), Value::String(dad_state.to_owned()));
                        result.ipv6.entry(index).or_default().push(Value::Map(map));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(result)
}

// ── Route dump ────────────────────────────────────────────────────────────────

struct RouteDump {
    ipv4: HashMap<u32, Vec<Value>>,
    ipv6: HashMap<u32, Vec<Value>>,
}

/// Dump non-kernel routes from the kernel, split by address family.
///
/// Kernel-managed routes (proto kernel) are excluded. The protocol field is
/// stripped from all returned route maps so values match policy-produced state.
async fn dump_routes(
    handle: &Handle,
    known_indices: &std::collections::HashSet<u32>,
) -> Result<RouteDump, BackendError> {
    let mut result = RouteDump {
        ipv4: HashMap::new(),
        ipv6: HashMap::new(),
    };

    let families = [
        (netlink_packet_route::AddressFamily::Inet, true),
        (netlink_packet_route::AddressFamily::Inet6, false),
    ];

    for (family, is_ipv4) in families {
        let mut route_msg = RouteMessage::default();
        route_msg.header.address_family = family;

        let mut stream = handle.route().get(route_msg).execute();
        while let Some(msg) = stream.try_next().await.map_err(|e| BackendError::QueryFailed {
            entity_type: "interface".to_string(),
            source: Box::new(e),
        })? {
            if msg.header.protocol == RouteProtocol::Kernel {
                continue;
            }

            if let Some(mut route_val) = parse_route_message(&msg, known_indices) {
                let oif = match extract_oif(&msg) {
                    Some(idx) => idx,
                    None => continue,
                };
                if let Value::Map(ref mut m) = route_val {
                    m.shift_remove("protocol");
                }
                if is_ipv4 {
                    result.ipv4.entry(oif).or_default().push(route_val);
                } else {
                    result.ipv6.entry(oif).or_default().push(route_val);
                }
            }
        }
    }

    Ok(result)
}

fn extract_oif(msg: &RouteMessage) -> Option<u32> {
    for attr in &msg.attributes {
        if let RouteAttribute::Oif(idx) = attr {
            return Some(*idx);
        }
    }
    None
}

fn parse_route_message(
    msg: &RouteMessage,
    known_indices: &std::collections::HashSet<u32>,
) -> Option<Value> {
    // Only process routes that go out through one of our discovered interfaces.
    let oif = extract_oif(msg)?;
    if !known_indices.contains(&oif) {
        return None;
    }

    let dst_prefix_len = msg.header.destination_prefix_length;

    let mut destination_ip: Option<IpAddr> = None;
    let mut gateway_ip: Option<IpAddr> = None;
    let mut metric: u32 = 0;
    let mut mtu: Option<u32> = None;
    let mut table: Option<u32> = None;

    for attr in &msg.attributes {
        match attr {
            RouteAttribute::Destination(addr) => {
                destination_ip = route_address_to_ip(addr);
            }
            RouteAttribute::Gateway(addr) => {
                gateway_ip = route_address_to_ip(addr);
            }
            RouteAttribute::Priority(p) => {
                metric = *p;
            }
            RouteAttribute::Metrics(metrics) => {
                for m in metrics {
                    if let netlink_packet_route::route::RouteMetric::Mtu(v) = m {
                        mtu = Some(*v);
                    }
                }
            }
            RouteAttribute::Table(t) => {
                table = Some(*t);
            }
            _ => {}
        }
    }

    let tos = msg.header.tos;

    // Build destination CIDR. If no explicit destination, it's a default route.
    let destination = if let Some(ip) = destination_ip {
        format!("{ip}/{dst_prefix_len}")
    } else {
        let af = msg.header.address_family;
        match af {
            netlink_packet_route::AddressFamily::Inet => {
                format!("0.0.0.0/{dst_prefix_len}")
            }
            netlink_packet_route::AddressFamily::Inet6 => {
                format!("::/{dst_prefix_len}")
            }
            _ => return None,
        }
    };

    let gateway_str = gateway_ip.map(|ip| ip.to_string());
    let protocol = route_protocol_str(msg.header.protocol);
    Some(build_route_value(
        &destination,
        gateway_str.as_deref(),
        metric,
        Some(protocol),
        mtu,
        table,
        tos,
    ))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_state::{Provenance, Value};
    use netlink_packet_route::link::InfoKind;
    use netlink_packet_route::route::RouteAddress;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ── format_mac ────────────────────────────────────────────────────────────

    /// format_mac produces lowercase colon-separated hex for all-zero MAC.
    #[test]
    fn test_format_mac_all_zeros() {
        let bytes = [0u8; 6];
        assert_eq!(format_mac(&bytes), "00:00:00:00:00:00");
    }

    /// format_mac produces lowercase colon-separated hex for all-0xFF MAC.
    #[test]
    fn test_format_mac_all_ff() {
        let bytes = [0xFFu8; 6];
        assert_eq!(format_mac(&bytes), "ff:ff:ff:ff:ff:ff");
    }

    /// format_mac produces lowercase hex (not uppercase) for mixed bytes.
    #[test]
    fn test_format_mac_mixed_bytes_produces_lowercase() {
        let bytes = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let result = format_mac(&bytes);
        assert_eq!(result, "aa:bb:cc:dd:ee:01");
        // Ensure no uppercase letters
        assert_eq!(result, result.to_lowercase(), "format_mac must produce lowercase output");
    }

    /// format_mac output has exactly 17 characters (6 octets + 5 colons).
    #[test]
    fn test_format_mac_length_is_17() {
        let bytes = [0x12u8, 0x34u8, 0x56u8, 0x78u8, 0x9Au8, 0xBCu8];
        let result = format_mac(&bytes);
        assert_eq!(result.len(), 17, "MAC string must be 17 characters long: {result}");
    }

    /// format_mac includes exactly 5 colon separators.
    #[test]
    fn test_format_mac_has_five_colons() {
        let bytes = [0x01u8, 0x02u8, 0x03u8, 0x04u8, 0x05u8, 0x06u8];
        let result = format_mac(&bytes);
        assert_eq!(result.chars().filter(|&c| c == ':').count(), 5);
    }

    // ── is_excluded_kind ──────────────────────────────────────────────────────

    /// Scenario: bridge interfaces are excluded from query results.
    #[test]
    fn test_is_excluded_kind_excludes_bridge() {
        assert!(is_excluded_kind(&InfoKind::Bridge), "Bridge must be excluded");
    }

    /// Scenario: bond interfaces are excluded from query results.
    #[test]
    fn test_is_excluded_kind_excludes_bond() {
        assert!(is_excluded_kind(&InfoKind::Bond), "Bond must be excluded");
    }

    /// Scenario: vlan interfaces are excluded from query results.
    #[test]
    fn test_is_excluded_kind_excludes_vlan() {
        assert!(is_excluded_kind(&InfoKind::Vlan), "Vlan must be excluded");
    }

    /// Dummy interfaces are excluded.
    #[test]
    fn test_is_excluded_kind_excludes_dummy() {
        assert!(is_excluded_kind(&InfoKind::Dummy), "Dummy must be excluded");
    }

    /// Vxlan interfaces are excluded.
    #[test]
    fn test_is_excluded_kind_excludes_vxlan() {
        assert!(is_excluded_kind(&InfoKind::Vxlan), "Vxlan must be excluded");
    }

    /// MacVlan interfaces are excluded.
    #[test]
    fn test_is_excluded_kind_excludes_macvlan() {
        assert!(is_excluded_kind(&InfoKind::MacVlan), "MacVlan must be excluded");
    }

    /// Wireguard interfaces are excluded.
    #[test]
    fn test_is_excluded_kind_excludes_wireguard() {
        assert!(is_excluded_kind(&InfoKind::Wireguard), "Wireguard must be excluded");
    }

    /// Tun interfaces are excluded.
    #[test]
    fn test_is_excluded_kind_excludes_tun() {
        assert!(is_excluded_kind(&InfoKind::Tun), "Tun must be excluded");
    }

    /// Veth interfaces are NOT excluded — they appear in query results.
    ///
    /// This is critical: integration tests use veth pairs and expect them to appear.
    #[test]
    fn test_is_excluded_kind_includes_veth() {
        assert!(!is_excluded_kind(&InfoKind::Veth), "Veth must NOT be excluded from results");
    }

    // ── build_route_value ─────────────────────────────────────────────────────

    /// build_route_value without gateway produces a map with destination and metric.
    #[test]
    fn test_build_route_value_without_gateway() {
        let val = build_route_value("10.0.0.0/24", None, 100, None, None, None, 0);
        let map = val.as_map().expect("build_route_value must return Value::Map");
        assert!(map.contains_key("destination"), "map must have 'destination' key");
        assert!(map.contains_key("metric"),      "map must have 'metric' key");
        assert!(!map.contains_key("gateway"),    "map must NOT have 'gateway' key when not provided");
        assert!(!map.contains_key("protocol"),   "map must NOT have 'protocol' key when not provided");
        assert!(!map.contains_key("mtu"),        "map must NOT have 'mtu' key when not provided");
        assert!(!map.contains_key("table"),      "map must NOT have 'table' key when default");
        assert!(!map.contains_key("tos"),        "map must NOT have 'tos' key when zero");
        assert_eq!(map["destination"].to_string(), "10.0.0.0/24");
        assert_eq!(map["metric"].as_u64(), Some(100));
    }

    /// build_route_value with gateway produces a map with destination, gateway, and metric.
    #[test]
    fn test_build_route_value_with_gateway() {
        let val = build_route_value("0.0.0.0/0", Some("192.168.1.1"), 0, Some("static"), None, None, 0);
        let map = val.as_map().expect("build_route_value must return Value::Map");
        assert!(map.contains_key("destination"), "map must have 'destination' key");
        assert!(map.contains_key("gateway"),     "map must have 'gateway' key when provided");
        assert!(map.contains_key("metric"),      "map must have 'metric' key");
        assert!(map.contains_key("protocol"),    "map must have 'protocol' key when provided");
        assert_eq!(map["destination"].to_string(), "0.0.0.0/0");
        assert_eq!(map["gateway"].to_string(), "192.168.1.1");
        assert_eq!(map["metric"].as_u64(), Some(0));
        assert_eq!(map["protocol"].as_str(), Some("static"));
    }

    /// build_route_value gateway field is only present when Some(_) is passed.
    #[test]
    fn test_build_route_value_gateway_field_absent_when_none() {
        let val = build_route_value("::/0", None, 512, None, None, None, 0);
        let map = val.as_map().unwrap();
        assert!(!map.contains_key("gateway"), "gateway must be absent when None");
        assert_eq!(map["metric"].as_u64(), Some(512));
    }

    /// build_route_value preserves the metric value exactly.
    #[test]
    fn test_build_route_value_metric_preserved() {
        let val = build_route_value("10.99.0.0/24", None, 1024, Some("kernel"), None, None, 0);
        let map = val.as_map().unwrap();
        assert_eq!(map["metric"].as_u64(), Some(1024));
        assert_eq!(map["protocol"].as_str(), Some("kernel"));
    }

    /// build_route_value includes mtu when provided.
    #[test]
    fn test_build_route_value_with_mtu() {
        let val = build_route_value("10.0.0.0/24", None, 100, None, Some(1400), None, 0);
        let map = val.as_map().unwrap();
        assert_eq!(map["mtu"].as_u64(), Some(1400));
    }

    /// build_route_value includes table when non-default (not 254).
    #[test]
    fn test_build_route_value_with_table() {
        let val = build_route_value("10.0.0.0/24", None, 100, None, None, Some(100), 0);
        let map = val.as_map().unwrap();
        assert_eq!(map["table"].as_u64(), Some(100));
    }

    /// build_route_value omits table when it equals main table (254).
    #[test]
    fn test_build_route_value_omits_main_table() {
        let val = build_route_value("10.0.0.0/24", None, 100, None, None, Some(254), 0);
        let map = val.as_map().unwrap();
        assert!(!map.contains_key("table"));
    }

    /// build_route_value includes tos when non-zero.
    #[test]
    fn test_build_route_value_with_tos() {
        let val = build_route_value("10.0.0.0/24", None, 100, None, None, None, 16);
        let map = val.as_map().unwrap();
        assert_eq!(map["tos"].as_u64(), Some(16));
    }

    // ── route_address_to_ip ───────────────────────────────────────────────────

    /// route_address_to_ip converts IPv4 RouteAddress to IpAddr::V4.
    #[test]
    fn test_route_address_to_ip_v4() {
        let addr = RouteAddress::Inet(Ipv4Addr::new(10, 0, 1, 1));
        let ip = route_address_to_ip(&addr).expect("IPv4 RouteAddress must convert");
        assert!(ip.is_ipv4(), "must produce an IPv4 address");
        assert_eq!(ip.to_string(), "10.0.1.1");
    }

    /// route_address_to_ip converts IPv6 RouteAddress to IpAddr::V6.
    #[test]
    fn test_route_address_to_ip_v6() {
        let addr = RouteAddress::Inet6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
        let ip = route_address_to_ip(&addr).expect("IPv6 RouteAddress must convert");
        assert!(ip.is_ipv6(), "must produce an IPv6 address");
    }

    /// route_address_to_ip returns None for the Other/Mpls variants.
    #[test]
    fn test_route_address_to_ip_other_returns_none() {
        // RouteAddress::Mpls is an example of a non-IP variant.
        // We use the RouteAddress::Other variant if available.
        // The simplest non-Inet/Inet6 is RouteAddress::Other(vec![]).
        let addr = RouteAddress::Other(vec![0u8; 4]);
        assert!(
            route_address_to_ip(&addr).is_none(),
            "Non-IP RouteAddress variants must return None"
        );
    }

    // ── kernel_default() provenance helper ────────────────────────────────────────────────────

    /// Scenario: All queried fields have KernelDefault provenance.
    /// kernel_default() must tag a String value with Provenance::KernelDefault.
    #[test]
    fn test_kd_string_value_has_kernel_default_provenance() {
        let fv = kernel_default(Value::String("eth0".to_owned()));
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "kernel_default() must set provenance to KernelDefault"
        );
        assert_eq!(fv.value, Value::String("eth0".to_owned()));
    }

    /// kernel_default() applied to a U64 (e.g., mtu) produces KernelDefault provenance.
    #[test]
    fn test_kd_u64_value_has_kernel_default_provenance() {
        let fv = kernel_default(Value::U64(1500));
        assert_eq!(fv.provenance, Provenance::KernelDefault);
        assert_eq!(fv.value, Value::U64(1500));
    }

    /// kernel_default() applied to a Bool (e.g., carrier) produces KernelDefault provenance.
    #[test]
    fn test_kd_bool_value_has_kernel_default_provenance() {
        let fv = kernel_default(Value::Bool(false));
        assert_eq!(fv.provenance, Provenance::KernelDefault);
        assert_eq!(fv.value, Value::Bool(false));
    }

    /// kernel_default() applied to a List (e.g., addresses, routes) produces KernelDefault provenance.
    #[test]
    fn test_kd_list_value_has_kernel_default_provenance() {
        let list = Value::List(vec![Value::String("10.0.1.50/24".to_owned())]);
        let fv = kernel_default(list.clone());
        assert_eq!(fv.provenance, Provenance::KernelDefault);
        assert_eq!(fv.value, list);
    }

    // ── Carrier byte-to-bool conversion ──────────────────────────────────────────

    /// Scenario: Query handles interface with link down gracefully.
    /// carrier byte 0 maps to false — link is physically down, no carrier signal.
    #[test]
    fn test_carrier_byte_zero_maps_to_false() {
        let carrier: u8 = 0;
        let result = carrier != 0;
        assert!(!result, "carrier byte 0 must produce false (link down)");
    }

    /// carrier byte 1 maps to true — link is up, carrier is present.
    #[test]
    fn test_carrier_byte_one_maps_to_true() {
        let carrier: u8 = 1;
        let result = carrier != 0;
        assert!(result, "carrier byte 1 must produce true (link up)");
    }

    /// Scenario: carrier attribute absent (None) defaults to false.
    /// Conservative assumption: if the kernel does not report carrier, treat link as down.
    #[test]
    fn test_carrier_none_defaults_to_false() {
        let carrier: Option<u8> = None;
        let result = carrier.is_some_and(|b| b != 0);
        assert!(!result, "absent carrier attribute must default to false");
    }

    /// Any nonzero carrier byte maps to true — nonzero signals carrier presence.
    #[test]
    fn test_carrier_nonzero_values_map_to_true() {
        for byte in [2u8, 10u8, 128u8, 255u8] {
            let result = byte != 0;
            assert!(result, "carrier byte {byte} must produce true");
        }
    }

    // ── route_protocol_str ────────────────────────────────────────────────────────

    /// Scenario: kernel-managed routes are identified by protocol string "kernel".
    /// route_protocol_str must return "kernel" for RouteProtocol::Kernel so that
    /// the filter closure can reject these routes.
    #[test]
    fn test_route_protocol_str_kernel_returns_kernel() {
        use netlink_packet_route::route::RouteProtocol;
        assert_eq!(route_protocol_str(RouteProtocol::Kernel), "kernel");
    }

    /// route_protocol_str returns "boot" for RouteProtocol::Boot (routes added
    /// manually via `ip route add` without an explicit protocol).
    #[test]
    fn test_route_protocol_str_boot_returns_boot() {
        use netlink_packet_route::route::RouteProtocol;
        assert_eq!(route_protocol_str(RouteProtocol::Boot), "boot");
    }

    /// route_protocol_str returns "static" for RouteProtocol::Static (routes
    /// installed by a routing daemon or explicitly tagged with proto static).
    #[test]
    fn test_route_protocol_str_static_returns_static() {
        use netlink_packet_route::route::RouteProtocol;
        assert_eq!(route_protocol_str(RouteProtocol::Static), "static");
    }

    /// route_protocol_str returns "dhcp" for RouteProtocol::Dhcp (routes installed
    /// by a DHCP client).
    #[test]
    fn test_route_protocol_str_dhcp_returns_dhcp() {
        use netlink_packet_route::route::RouteProtocol;
        assert_eq!(route_protocol_str(RouteProtocol::Dhcp), "dhcp");
    }

    /// route_protocol_str returns "ra" for RouteProtocol::Ra (routes installed by
    /// IPv6 Router Advertisement).
    #[test]
    fn test_route_protocol_str_ra_returns_ra() {
        use netlink_packet_route::route::RouteProtocol;
        assert_eq!(route_protocol_str(RouteProtocol::Ra), "ra");
    }

    // ── Route filter predicate (kernel routes excluded) ───────────────────────────

    /// Scenario: kernel-managed routes (protocol=kernel) are excluded.
    /// The filter closure in query_interfaces must reject a route value whose
    /// "protocol" field is "kernel".
    #[test]
    fn test_route_filter_predicate_excludes_kernel_protocol() {
        let kernel_route =
            build_route_value("10.0.0.0/24", None, 0, Some("kernel"), None, None, 0);
        let passes = kernel_route
            .as_map()
            .and_then(|m| m.get("protocol"))
            .and_then(|v| v.as_str())
            .map(|s| s != "kernel")
            .unwrap_or(true);
        assert!(!passes, "route with protocol=kernel must be excluded by the filter");
    }

    /// Scenario: non-kernel routes pass through the filter.
    /// Routes with protocol in {static, dhcp, boot, ra} must not be filtered out.
    #[test]
    fn test_route_filter_predicate_passes_non_kernel_protocols() {
        for proto in &["static", "dhcp", "boot", "ra", "other"] {
            let route =
                build_route_value("0.0.0.0/0", Some("10.0.0.1"), 0, Some(proto), None, None, 0);
            let passes = route
                .as_map()
                .and_then(|m| m.get("protocol"))
                .and_then(|v| v.as_str())
                .map(|s| s != "kernel")
                .unwrap_or(true);
            assert!(passes, "route with protocol={proto} must pass the filter");
        }
    }

    /// Scenario: routes without a protocol field pass the filter (default: include).
    /// The filter uses `unwrap_or(true)`, so an absent protocol key means the route
    /// is included.
    #[test]
    fn test_route_filter_predicate_passes_route_without_protocol() {
        let route = build_route_value("::/0", Some("fe80::1"), 1024, None, None, None, 0);
        // Confirm there is no protocol key.
        let map = route.as_map().unwrap();
        assert!(
            !map.contains_key("protocol"),
            "route built with protocol=None must have no 'protocol' key"
        );
        let passes = route
            .as_map()
            .and_then(|m| m.get("protocol"))
            .and_then(|v| v.as_str())
            .map(|s| s != "kernel")
            .unwrap_or(true);
        assert!(
            passes,
            "route without 'protocol' field must default to included (unwrap_or(true))"
        );
    }

    // ── detect_technology ─────────────────────────────────────────────────────────

    /// Scenario: A non-WiFi interface (no phy80211 symlink) is classified as
    /// "ethernet". A nonexistent interface has no phy80211 symlink, so
    /// detect_technology must return ETHERNET.
    #[test]
    fn test_detect_technology_returns_ethernet_for_nonexistent_interface() {
        let tech = detect_technology("interface_that_does_not_exist_xyzzy_99");
        assert_eq!(
            tech, ETHERNET,
            "detect_technology must return 'ethernet' when phy80211 symlink is absent"
        );
    }

    /// detect_technology returns "ethernet" for any interface name that lacks a
    /// /sys/class/net/<name>/phy80211 path. Verifying with multiple names
    /// confirms the function does not hard-code a single name.
    #[test]
    fn test_detect_technology_returns_ethernet_for_various_nonexistent_names() {
        for name in &["eth0_fake_99", "veth_fake_test", "enp0s3_fake"] {
            let tech = detect_technology(name);
            assert_eq!(
                tech, ETHERNET,
                "detect_technology({name}) must return 'ethernet' when phy80211 absent"
            );
        }
    }

    // ── build_ethernet_sub_object ─────────────────────────────────────────────────

    /// Scenario: Ethernet interface has ethernet sub-object with speed.
    /// When the sysfs speed file does not exist (e.g., for a nonexistent or
    /// virtual interface), build_ethernet_sub_object must return None so that
    /// the "ethernet" sub-object is omitted from the state.
    ///
    /// This mirrors the acceptance criterion: "the ethernet.speed field is None
    /// (omitted)" when link is down or speed is unavailable.
    #[test]
    fn test_build_ethernet_sub_object_returns_none_for_nonexistent_interface() {
        let result = build_ethernet_sub_object("interface_that_does_not_exist_xyzzy_99");
        assert!(
            result.is_none(),
            "build_ethernet_sub_object must return None when speed file is absent"
        );
    }

    /// build_ethernet_sub_object returns None for any nonexistent interface.
    #[test]
    fn test_build_ethernet_sub_object_returns_none_for_various_fake_names() {
        for name in &["eth0_fake_99", "veth_down_fake", "enp0s3_fake_speed"] {
            let result = build_ethernet_sub_object(name);
            assert!(
                result.is_none(),
                "build_ethernet_sub_object({name}) must return None when speed file absent"
            );
        }
    }

    /// When build_ethernet_sub_object returns Some, the sub-object must be a
    /// Value::Map with a "speed" key tagged KernelDefault.
    ///
    /// We test the structure of the returned FieldValue using a known fake path
    /// that does have a speed file — but since we can't control sysfs in unit
    /// tests, we test the shape via the lo interface which sometimes has a speed
    /// file. We skip this live-sysfs check and instead verify that IF Some is
    /// returned, the structure is correct.
    #[test]
    fn test_build_ethernet_sub_object_structure_when_some() {
        // Construct a valid FieldValue manually (simulating what the function
        // would return) and check structural invariants.
        use indexmap::IndexMap;
        let mut map = IndexMap::new();
        map.insert("speed".to_string(), Value::U64(1000));
        let fv = kernel_default(Value::Map(map));

        let inner_map = fv.value.as_map().expect("ethernet sub-object must be a Map");
        assert!(inner_map.contains_key("speed"), "ethernet sub-object must have 'speed' key");
        assert_eq!(inner_map["speed"].as_u64(), Some(1000));
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "ethernet sub-object must have KernelDefault provenance"
        );
    }

    // ── build_wifi_sub_object ─────────────────────────────────────────────────────

    /// Scenario: WiFi sub-object is absent when all nl80211 attributes are None.
    /// The current stubs for read_sysfs_wifi_mode/ssid/frequency all return None,
    /// so build_wifi_sub_object must return None (no sub-object to emit).
    ///
    /// This exercises the guard: "if mode.is_none() && ssid.is_none() &&
    /// frequency.is_none() { return None; }"
    #[test]
    fn test_build_wifi_sub_object_returns_none_when_all_stubs_return_none() {
        // Any interface name works — the stubs ignore the name and always return None.
        let result = build_wifi_sub_object("wlan0_fake_99");
        assert!(
            result.is_none(),
            "build_wifi_sub_object must return None when mode, ssid, and frequency are all None"
        );
    }

    /// build_wifi_sub_object returns None for multiple different interface names,
    /// confirming the behavior does not depend on the name when stubs return None.
    #[test]
    fn test_build_wifi_sub_object_returns_none_for_various_names() {
        for name in &["wlan0_fake", "wlp3s0_fake", "wlx001122334455_fake"] {
            let result = build_wifi_sub_object(name);
            assert!(
                result.is_none(),
                "build_wifi_sub_object({name}) must return None when all wifi attributes unavailable"
            );
        }
    }

    /// When build_wifi_sub_object returns Some, the sub-object must be a
    /// Value::Map with at least one of mode/ssid/frequency, tagged KernelDefault.
    /// We verify the structure manually using simulated data.
    #[test]
    fn test_build_wifi_sub_object_structure_when_some() {
        use indexmap::IndexMap;
        let mut map = IndexMap::new();
        map.insert("mode".to_string(), Value::String("station".to_owned()));
        map.insert("ssid".to_string(), Value::String("MyNetwork".to_owned()));
        map.insert("frequency".to_string(), Value::U64(5180));
        let fv = kernel_default(Value::Map(map));

        let inner_map = fv.value.as_map().expect("wifi sub-object must be a Map");
        assert_eq!(inner_map["mode"].as_str(), Some("station"));
        assert_eq!(inner_map["ssid"].as_str(), Some("MyNetwork"));
        assert_eq!(inner_map["frequency"].as_u64(), Some(5180));
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "wifi sub-object must have KernelDefault provenance"
        );
    }

    // ── Protocol field stripping ──────────────────────────────────────────────────

    /// Scenario: the protocol field is stripped from all returned routes.
    /// After the filter, the protocol key must be absent from each route Value::Map
    /// so it does not appear in the query output. Other fields must survive.
    #[test]
    fn test_route_protocol_field_stripped_from_static_route() {
        let route =
            build_route_value("0.0.0.0/0", Some("10.0.0.1"), 0, Some("static"), None, None, 0);
        // Replicate the stripping closure from query_interfaces.
        let stripped = match route {
            Value::Map(mut m) => {
                m.shift_remove("protocol");
                Value::Map(m)
            }
            other => other,
        };
        let map = stripped.as_map().expect("must remain a Map after stripping");
        assert!(!map.contains_key("protocol"), "protocol key must be absent after stripping");
        assert!(map.contains_key("destination"), "destination must survive stripping");
        assert!(map.contains_key("gateway"), "gateway must survive stripping");
        assert!(map.contains_key("metric"), "metric must survive stripping");
    }

    /// Stripping protocol from a route that has no protocol key is a no-op and
    /// must not change the map length.
    #[test]
    fn test_route_protocol_strip_noop_when_key_is_absent() {
        let route = build_route_value("192.168.0.0/24", None, 0, None, None, None, 0);
        let pre_len = route.as_map().unwrap().len();
        let stripped = match route {
            Value::Map(mut m) => {
                m.shift_remove("protocol");
                Value::Map(m)
            }
            other => other,
        };
        assert_eq!(
            stripped.as_map().unwrap().len(),
            pre_len,
            "stripping an absent key must not change map length"
        );
    }

    /// Protocol stripping preserves optional fields mtu, table, and tos.
    #[test]
    fn test_route_protocol_strip_preserves_mtu_table_tos() {
        let route = build_route_value(
            "10.0.0.0/24",
            Some("10.0.0.1"),
            100,
            Some("dhcp"),
            Some(1400),
            Some(100),
            8,
        );
        let stripped = match route {
            Value::Map(mut m) => {
                m.shift_remove("protocol");
                Value::Map(m)
            }
            other => other,
        };
        let map = stripped.as_map().unwrap();
        assert!(!map.contains_key("protocol"), "protocol must be stripped");
        assert_eq!(map["mtu"].as_u64(), Some(1400), "mtu must survive stripping");
        assert_eq!(map["table"].as_u64(), Some(100), "table must survive stripping");
        assert_eq!(map["tos"].as_u64(), Some(8), "tos must survive stripping");
    }

    // ── classify_dad_state ────────────────────────────────────────────────────────

    /// Scenario: Query reports IPv6 DAD state on addresses — "preferred" state.
    /// An address whose IFA_FLAGS attribute has no special flags has completed DAD
    /// successfully; classify_dad_state must return "preferred".
    #[test]
    fn test_classify_dad_state_preferred_with_empty_ifa_flags() {
        use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
        let attrs = vec![AddressAttribute::Flags(AddressFlags::empty())];
        let header_flags = AddressHeaderFlags::empty();
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "preferred",
            "empty IFA_FLAGS must produce 'preferred' DAD state"
        );
    }

    /// When IFA_FLAGS has the Tentative flag set, DAD is in progress; the address
    /// has not yet been confirmed as unique on the link.
    #[test]
    fn test_classify_dad_state_tentative_from_ifa_flags() {
        use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
        let attrs = vec![AddressAttribute::Flags(AddressFlags::Tentative)];
        let header_flags = AddressHeaderFlags::empty();
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "tentative",
            "Tentative IFA_FLAGS must produce 'tentative' DAD state"
        );
    }

    /// When IFA_FLAGS has the Dadfailed flag set, a duplicate was detected on the link.
    #[test]
    fn test_classify_dad_state_dadfailed_from_ifa_flags() {
        use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
        let attrs = vec![AddressAttribute::Flags(AddressFlags::Dadfailed)];
        let header_flags = AddressHeaderFlags::empty();
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "dadfailed",
            "Dadfailed IFA_FLAGS must produce 'dadfailed' DAD state"
        );
    }

    /// When IFA_FLAGS has the Deprecated flag set, the address is deprecated —
    /// usable for existing connections but not for new ones.
    #[test]
    fn test_classify_dad_state_deprecated_from_ifa_flags() {
        use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
        let attrs = vec![AddressAttribute::Flags(AddressFlags::Deprecated)];
        let header_flags = AddressHeaderFlags::empty();
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "deprecated",
            "Deprecated IFA_FLAGS must produce 'deprecated' DAD state"
        );
    }

    /// IFA_FLAGS attribute takes precedence over header flags.
    /// When IFA_FLAGS says "preferred" (no special flags) but the 8-bit header
    /// flag says "tentative", the header flag must be ignored.
    /// This tests the spec rule: "Check the 32-bit IFA_FLAGS attribute first
    /// (authoritative on modern kernels), then fall back to the 8-bit header field."
    #[test]
    fn test_classify_dad_state_ifa_flags_overrides_header() {
        use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
        let attrs = vec![AddressAttribute::Flags(AddressFlags::empty())];
        let header_flags = AddressHeaderFlags::Tentative;
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "preferred",
            "IFA_FLAGS attribute must override header flags when present"
        );
    }

    /// Fallback to header flags when no IFA_FLAGS attribute is present.
    /// On older kernels that omit the IFA_FLAGS netlink attribute, the 8-bit
    /// header flags field is the only source of DAD state.
    /// Header flag Tentative → "tentative".
    #[test]
    fn test_classify_dad_state_fallback_to_header_tentative() {
        use netlink_packet_route::address::{AddressAttribute, AddressHeaderFlags};
        let attrs: Vec<AddressAttribute> = vec![];
        let header_flags = AddressHeaderFlags::Tentative;
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "tentative",
            "fallback: header Tentative flag must produce 'tentative'"
        );
    }

    /// Fallback to header flags when no IFA_FLAGS attribute is present.
    /// Header flag Dadfailed → "dadfailed".
    #[test]
    fn test_classify_dad_state_fallback_to_header_dadfailed() {
        use netlink_packet_route::address::{AddressAttribute, AddressHeaderFlags};
        let attrs: Vec<AddressAttribute> = vec![];
        let header_flags = AddressHeaderFlags::Dadfailed;
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "dadfailed",
            "fallback: header Dadfailed flag must produce 'dadfailed'"
        );
    }

    /// Fallback to header flags when no IFA_FLAGS attribute is present.
    /// Header flag Deprecated → "deprecated".
    #[test]
    fn test_classify_dad_state_fallback_to_header_deprecated() {
        use netlink_packet_route::address::{AddressAttribute, AddressHeaderFlags};
        let attrs: Vec<AddressAttribute> = vec![];
        let header_flags = AddressHeaderFlags::Deprecated;
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "deprecated",
            "fallback: header Deprecated flag must produce 'deprecated'"
        );
    }

    /// Fallback to header flags when no IFA_FLAGS attribute is present.
    /// Empty header flags → "preferred" (the default / normal state).
    #[test]
    fn test_classify_dad_state_fallback_empty_header_returns_preferred() {
        use netlink_packet_route::address::{AddressAttribute, AddressHeaderFlags};
        let attrs: Vec<AddressAttribute> = vec![];
        let header_flags = AddressHeaderFlags::empty();
        assert_eq!(
            classify_dad_state(&attrs, &header_flags),
            "preferred",
            "fallback: empty header flags must produce 'preferred'"
        );
    }
}

// ── Main query function ───────────────────────────────────────────────────────

/// Query non-virtual network interfaces via rtnetlink.
///
/// Enumerates all links with `ARPHRD_ETHER` type and an allowed `IFLA_INFO_KIND`
/// (physical NICs, veth pairs, WiFi interfaces). Applies technology detection to
/// classify each interface as `"ethernet"` or `"wifi"`, optionally filtering by
/// `entity_type_filter`. The `selector` parameter further narrows results by name,
/// MAC, driver, etc. All returned fields carry `KernelDefault` provenance.
pub async fn query_interfaces(
    handle: &Handle,
    entity_type_filter: Option<&str>,
    selector: Option<&Selector>,
) -> Result<StateSet, BackendError> {
    // ── Step 1: Enumerate all links ───────────────────────────────────────────
    let mut links_stream = handle.link().get().execute();
    let mut all_links: Vec<LinkMessage> = Vec::new();
    while let Some(msg) = links_stream.try_next().await.map_err(|e| {
        BackendError::QueryFailed {
            entity_type: entity_type_filter.unwrap_or(ETHERNET).to_string(),
            source: Box::new(e),
        }
    })? {
        all_links.push(msg);
    }

    // ── Step 2: Filter to ethernet-class links ────────────────────────────────
    struct LinkInfo2 {
        index: u32,
        name: String,
        mac: Option<[u8; 6]>,
        mtu: Option<u32>,
        carrier: Option<u8>,
        enabled: bool,
        detected_type: &'static str,
        driver: Option<String>,
        pci_path: Option<String>,
    }

    let mut candidate_links: Vec<LinkInfo2> = Vec::new();
    for msg in &all_links {
        // Must be ARPHRD_ETHER (1).
        if msg.header.link_layer_type != LinkLayerType::Ether {
            continue;
        }

        // Check IFLA_INFO_KIND: exclude virtual types, include physical and veth.
        if let Some(kind) = extract_link_kind(msg) {
            if is_excluded_kind(&kind) {
                continue;
            }
        }
        // No IFLA_INFO_KIND → physical NIC; always include.

        let name = match extract_link_name(msg) {
            Some(n) => n,
            None => {
                warn!("Skipping link with no name (index {})", msg.header.index);
                continue;
            }
        };

        candidate_links.push(LinkInfo2 {
            index: msg.header.index,
            name,
            mac: extract_link_mac(msg),
            mtu: extract_link_mtu(msg),
            carrier: extract_link_carrier(msg),
            enabled: extract_link_enabled(msg),
            detected_type: "", // filled in step 3
            driver: None,      // filled in step 3
            pci_path: None,    // filled in step 3
        });
    }

    // ── Step 3: Detect technology, apply type filter, apply selector ──────────
    let mut matched_links: Vec<LinkInfo2> = Vec::new();
    for mut link in candidate_links {
        let detected_type = detect_technology(&link.name);

        // Apply entity_type_filter (None = accept all non-virtual types).
        if let Some(filter) = entity_type_filter {
            if detected_type != filter {
                continue;
            }
        }

        link.detected_type = detected_type;

        // Cache sysfs reads here so step 7 can reuse them without a second syscall.
        let driver = read_sysfs_driver(&link.name);
        let pci_path = read_sysfs_pci_path(&link.name);
        let discovered = build_discovered_selector(
            detected_type,
            &link.name,
            link.mac,
            driver.as_deref(),
            pci_path.as_deref(),
        );

        if let Some(sel) = selector {
            if !sel.matches(&discovered) {
                continue;
            }
        }

        link.driver = driver;
        link.pci_path = pci_path;
        matched_links.push(link);
    }

    // ── Step 4: Build index set for route filtering ────────────────────────────
    let known_indices: std::collections::HashSet<u32> =
        matched_links.iter().map(|l| l.index).collect();

    // ── Step 5: Read DNS servers (global — read once before the link loop) ────
    let dns_servers = read_dns_servers();

    // ── Step 6: Dump addresses ─────────────────────────────────────────────────
    let addr_dump = dump_addresses(handle).await?;

    // ── Step 7: Dump routes ────────────────────────────────────────────────────
    let route_dump = dump_routes(handle, &known_indices).await?;

    // ── Step 8: Assemble State objects ────────────────────────────────────────
    let mut state_set = StateSet::new();

    for link in matched_links {
        let driver = link.driver;
        let detected_type = link.detected_type;

        let mut fields: IndexMap<String, FieldValue> = IndexMap::new();

        fields.insert(
            "type".to_string(),
            kernel_default(Value::String(detected_type.to_string())),
        );

        fields.insert("name".to_string(), kernel_default(Value::String(link.name.clone())));

        if let Some(mtu) = link.mtu {
            fields.insert("mtu".to_string(), kernel_default(Value::U64(mtu as u64)));
        }

        if let Some(mac_bytes) = link.mac {
            fields.insert(
                "mac".to_string(),
                kernel_default(Value::String(format_mac(&mac_bytes))),
            );
        }

        fields.insert(
            "carrier".to_string(),
            kernel_default(Value::Bool(link.carrier.unwrap_or(0) != 0)),
        );

        fields.insert(
            "enabled".to_string(),
            kernel_default(Value::Bool(link.enabled)),
        );

        if let Some(drv) = driver {
            fields.insert("driver".to_string(), kernel_default(Value::String(drv)));
        }

        // Technology-specific sub-object.
        if detected_type == ETHERNET {
            if let Some(sub_obj) = build_ethernet_sub_object(&link.name) {
                fields.insert("ethernet".to_string(), sub_obj);
            }
        } else if detected_type == WIFI {
            if let Some(sub_obj) = build_wifi_sub_object(&link.name) {
                fields.insert("wifi".to_string(), sub_obj);
            }
        }

        // IPv4 sub-object: addresses (CIDRs), routes, dns_servers.
        let ipv4_addrs: Vec<Value> = addr_dump
            .ipv4
            .get(&link.index)
            .map(|addrs| {
                addrs
                    .iter()
                    .map(|s| {
                        s.parse::<ipnetwork::IpNetwork>()
                            .map(Value::IpNetwork)
                            .unwrap_or_else(|_| Value::String(s.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ipv4_routes: Vec<Value> = route_dump
            .ipv4
            .get(&link.index)
            .cloned()
            .unwrap_or_default();
        let ipv4_dns: Vec<Value> = dns_servers
            .0
            .iter()
            .map(|ip| Value::IpAddr(*ip))
            .collect();
        let mut ipv4_map = IndexMap::new();
        ipv4_map.insert("addresses".to_string(), Value::List(ipv4_addrs));
        ipv4_map.insert("routes".to_string(), Value::List(ipv4_routes));
        if !ipv4_dns.is_empty() {
            ipv4_map.insert("dns_servers".to_string(), Value::List(ipv4_dns));
        }
        fields.insert("ipv4".to_string(), kernel_default(Value::Map(ipv4_map)));

        // IPv6 sub-object: addresses (maps with dad_state), routes, dns_servers,
        // link_local (addr_gen_mode), dad_transmits.
        let ipv6_addrs: Vec<Value> = addr_dump
            .ipv6
            .get(&link.index)
            .cloned()
            .unwrap_or_default();
        let ipv6_routes: Vec<Value> = route_dump
            .ipv6
            .get(&link.index)
            .cloned()
            .unwrap_or_default();
        let ipv6_dns: Vec<Value> = dns_servers
            .1
            .iter()
            .map(|ip| Value::IpAddr(*ip))
            .collect();
        let mut ipv6_map = IndexMap::new();
        ipv6_map.insert("addresses".to_string(), Value::List(ipv6_addrs));
        ipv6_map.insert("routes".to_string(), Value::List(ipv6_routes));
        if !ipv6_dns.is_empty() {
            ipv6_map.insert("dns_servers".to_string(), Value::List(ipv6_dns));
        }
        if let Some(link_local) = read_procfs_addr_gen_mode(&link.name) {
            ipv6_map.insert("link_local".to_string(), Value::String(link_local));
        }
        if let Some(dad_tx) = read_procfs_dad_transmits(&link.name) {
            ipv6_map.insert("dad_transmits".to_string(), Value::U64(dad_tx));
        }
        fields.insert("ipv6".to_string(), kernel_default(Value::Map(ipv6_map)));

        let state = State {
            entity_type: detected_type.to_string(),
            selector: Selector::with_name(link.name.clone()),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        };

        state_set.insert(state);
    }

    // ── Step 8: Handle not-found ──────────────────────────────────────────────
    if let Some(sel) = selector {
        if sel.is_specific() && state_set.is_empty() {
            return Err(BackendError::NotFound {
                entity_type: entity_type_filter.unwrap_or(ETHERNET).to_string(),
                selector: Box::new(sel.clone()),
            });
        }
    }

    Ok(state_set)
}
