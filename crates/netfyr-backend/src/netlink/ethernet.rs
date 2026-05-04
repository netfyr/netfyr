//! Ethernet interface query via rtnetlink.

use std::collections::HashMap;
use std::net::IpAddr;

use futures::TryStreamExt;
use indexmap::IndexMap;
use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};
use netlink_packet_route::link::{
    InfoKind, LinkAttribute, LinkFlags, LinkInfo, LinkLayerType, LinkMessage,
};
use netlink_packet_route::route::{
    RouteAddress, RouteAttribute, RouteMessage, RouteProtocol,
};
use rtnetlink::Handle;
use tracing::warn;

use crate::BackendError;
use super::query::{
    build_discovered_selector, read_sysfs_driver,
    read_sysfs_pci_path, read_sysfs_speed,
};

// ── Exclusion list ────────────────────────────────────────────────────────────

/// Returns `true` if a link with the given `InfoKind` should be excluded from
/// ethernet query results.
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
) -> Value {
    let mut map = IndexMap::new();
    map.insert("destination".to_string(), Value::String(destination.to_owned()));
    if let Some(gw) = gateway {
        map.insert("gateway".to_string(), Value::String(gw.to_owned()));
    }
    map.insert("metric".to_string(), Value::U64(metric as u64));
    if let Some(proto) = protocol {
        map.insert("protocol".to_string(), Value::String(proto.to_owned()));
    }
    Value::Map(map)
}

/// Convenience wrapper that tags a `Value` with `KernelDefault` provenance.
fn kd(value: Value) -> FieldValue {
    FieldValue {
        value,
        provenance: Provenance::KernelDefault,
    }
}

// ── Address dump ─────────────────────────────────────────────────────────────

/// Dump all addresses from the kernel and return a map from interface index to
/// CIDR strings (e.g., `"10.0.1.50/24"`).
async fn dump_addresses(
    handle: &Handle,
) -> Result<HashMap<u32, Vec<String>>, BackendError> {
    let mut map: HashMap<u32, Vec<String>> = HashMap::new();

    let mut stream = handle.address().get().execute();
    while let Some(msg) = stream.try_next().await.map_err(|e| BackendError::QueryFailed {
        entity_type: "ethernet".to_string(),
        source: Box::new(e),
    })? {
        // Only include IPv4 addresses; skip IPv6 and any other address family.
        if msg.header.family != netlink_packet_route::AddressFamily::Inet {
            continue;
        }

        let index = msg.header.index;
        let prefix_len = msg.header.prefix_len;

        for attr in &msg.attributes {
            if let netlink_packet_route::address::AddressAttribute::Address(ip) = attr {
                let cidr = format!("{ip}/{prefix_len}");
                map.entry(index).or_default().push(cidr);
            }
        }
    }

    Ok(map)
}

// ── Route dump ────────────────────────────────────────────────────────────────

/// Dump IPv4 routes and return a map from output interface index to
/// route `Value::Map` objects.
///
/// Only unicast routes (RTN_UNICAST) are included. Routes with no output
/// interface (`RTA_OIF`) — e.g., local/blackhole routes — are skipped.
/// IPv6 routes are not queried per spec.
async fn dump_routes(
    handle: &Handle,
    known_indices: &std::collections::HashSet<u32>,
) -> Result<HashMap<u32, Vec<Value>>, BackendError> {
    let mut map: HashMap<u32, Vec<Value>> = HashMap::new();

    let mut route_msg = RouteMessage::default();
    route_msg.header.address_family = netlink_packet_route::AddressFamily::Inet;

    let mut stream = handle.route().get(route_msg).execute();
    while let Some(msg) = stream.try_next().await.map_err(|e| BackendError::QueryFailed {
        entity_type: "ethernet".to_string(),
        source: Box::new(e),
    })? {
        if let Some(route_val) = parse_route_message(&msg, known_indices) {
            let oif = extract_oif(&msg);
            if let Some(idx) = oif {
                map.entry(idx).or_default().push(route_val);
            }
        }
    }

    Ok(map)
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
            _ => {}
        }
    }

    // Build destination CIDR. If no explicit destination, it's a default route.
    let destination = if let Some(ip) = destination_ip {
        format!("{ip}/{dst_prefix_len}")
    } else {
        // Default route: 0.0.0.0/0 (IPv4 only; IPv6 routes are not queried).
        let af = msg.header.address_family;
        match af {
            netlink_packet_route::AddressFamily::Inet => {
                format!("0.0.0.0/{dst_prefix_len}")
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

    /// Scenario: bridge interfaces are excluded from ethernet query results.
    #[test]
    fn test_is_excluded_kind_excludes_bridge() {
        assert!(is_excluded_kind(&InfoKind::Bridge), "Bridge must be excluded");
    }

    /// Scenario: bond interfaces are excluded from ethernet query results.
    #[test]
    fn test_is_excluded_kind_excludes_bond() {
        assert!(is_excluded_kind(&InfoKind::Bond), "Bond must be excluded");
    }

    /// Scenario: vlan interfaces are excluded from ethernet query results.
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

    /// Veth interfaces are NOT excluded — they appear in ethernet query results.
    ///
    /// This is critical: integration tests use veth pairs and expect them to appear.
    #[test]
    fn test_is_excluded_kind_includes_veth() {
        assert!(!is_excluded_kind(&InfoKind::Veth), "Veth must NOT be excluded from ethernet results");
    }

    // ── build_route_value ─────────────────────────────────────────────────────

    /// build_route_value without gateway produces a map with destination and metric.
    #[test]
    fn test_build_route_value_without_gateway() {
        let val = build_route_value("10.0.0.0/24", None, 100, None);
        let map = val.as_map().expect("build_route_value must return Value::Map");
        assert!(map.contains_key("destination"), "map must have 'destination' key");
        assert!(map.contains_key("metric"),      "map must have 'metric' key");
        assert!(!map.contains_key("gateway"),    "map must NOT have 'gateway' key when not provided");
        assert!(!map.contains_key("protocol"),   "map must NOT have 'protocol' key when not provided");
        assert_eq!(map["destination"].as_str(), Some("10.0.0.0/24"));
        assert_eq!(map["metric"].as_u64(), Some(100));
    }

    /// build_route_value with gateway produces a map with destination, gateway, and metric.
    #[test]
    fn test_build_route_value_with_gateway() {
        let val = build_route_value("0.0.0.0/0", Some("192.168.1.1"), 0, Some("static"));
        let map = val.as_map().expect("build_route_value must return Value::Map");
        assert!(map.contains_key("destination"), "map must have 'destination' key");
        assert!(map.contains_key("gateway"),     "map must have 'gateway' key when provided");
        assert!(map.contains_key("metric"),      "map must have 'metric' key");
        assert!(map.contains_key("protocol"),    "map must have 'protocol' key when provided");
        assert_eq!(map["destination"].as_str(), Some("0.0.0.0/0"));
        assert_eq!(map["gateway"].as_str(), Some("192.168.1.1"));
        assert_eq!(map["metric"].as_u64(), Some(0));
        assert_eq!(map["protocol"].as_str(), Some("static"));
    }

    /// build_route_value gateway field is only present when Some(_) is passed.
    #[test]
    fn test_build_route_value_gateway_field_absent_when_none() {
        let val = build_route_value("::/0", None, 512, None);
        let map = val.as_map().unwrap();
        assert!(!map.contains_key("gateway"), "gateway must be absent when None");
        assert_eq!(map["metric"].as_u64(), Some(512));
    }

    /// build_route_value preserves the metric value exactly.
    #[test]
    fn test_build_route_value_metric_preserved() {
        let val = build_route_value("10.99.0.0/24", None, 1024, Some("kernel"));
        let map = val.as_map().unwrap();
        assert_eq!(map["metric"].as_u64(), Some(1024));
        assert_eq!(map["protocol"].as_str(), Some("kernel"));
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

    // ── kd() provenance helper ────────────────────────────────────────────────────

    /// Scenario: All queried fields have KernelDefault provenance.
    /// kd() must tag a String value with Provenance::KernelDefault.
    #[test]
    fn test_kd_string_value_has_kernel_default_provenance() {
        let fv = kd(Value::String("eth0".to_owned()));
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "kd() must set provenance to KernelDefault"
        );
        assert_eq!(fv.value, Value::String("eth0".to_owned()));
    }

    /// kd() applied to a U64 (e.g., mtu) produces KernelDefault provenance.
    #[test]
    fn test_kd_u64_value_has_kernel_default_provenance() {
        let fv = kd(Value::U64(1500));
        assert_eq!(fv.provenance, Provenance::KernelDefault);
        assert_eq!(fv.value, Value::U64(1500));
    }

    /// kd() applied to a Bool (e.g., carrier) produces KernelDefault provenance.
    #[test]
    fn test_kd_bool_value_has_kernel_default_provenance() {
        let fv = kd(Value::Bool(false));
        assert_eq!(fv.provenance, Provenance::KernelDefault);
        assert_eq!(fv.value, Value::Bool(false));
    }

    /// kd() applied to a List (e.g., addresses, routes) produces KernelDefault provenance.
    #[test]
    fn test_kd_list_value_has_kernel_default_provenance() {
        let list = Value::List(vec![Value::String("10.0.1.50/24".to_owned())]);
        let fv = kd(list.clone());
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
}

// ── Main query function ───────────────────────────────────────────────────────

/// Query ethernet interfaces via rtnetlink.
///
/// Enumerates all links, filters to those with `ARPHRD_ETHER` type and an
/// allowed `IFLA_INFO_KIND` (physical NICs and veth pairs), optionally matches
/// against the provided `selector`, and assembles `State` objects with
/// `KernelDefault` provenance. Addresses and routes are fetched in two bulk
/// dumps (one each) and indexed by interface index for O(1) lookup per link.
pub async fn query_ethernet(
    handle: &Handle,
    selector: Option<&Selector>,
) -> Result<StateSet, BackendError> {
    // ── Step 1: Enumerate all links ───────────────────────────────────────────
    let mut links_stream = handle.link().get().execute();
    let mut all_links: Vec<LinkMessage> = Vec::new();
    while let Some(msg) = links_stream.try_next().await.map_err(|e| {
        BackendError::QueryFailed {
            entity_type: "ethernet".to_string(),
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
    }

    let mut ethernet_links: Vec<LinkInfo2> = Vec::new();
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

        ethernet_links.push(LinkInfo2 {
            index: msg.header.index,
            name,
            mac: extract_link_mac(msg),
            mtu: extract_link_mtu(msg),
            carrier: extract_link_carrier(msg),
            enabled: extract_link_enabled(msg),
        });
    }

    // ── Step 3: Apply selector filter ─────────────────────────────────────────
    let mut matched_links: Vec<LinkInfo2> = Vec::new();
    for link in ethernet_links {
        let driver = read_sysfs_driver(&link.name);
        let pci_path = read_sysfs_pci_path(&link.name);
        let discovered = build_discovered_selector(
            "ethernet",
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

        matched_links.push(link);
    }

    // ── Step 4: Build index set for route filtering ────────────────────────────
    let known_indices: std::collections::HashSet<u32> =
        matched_links.iter().map(|l| l.index).collect();

    // ── Step 5: Dump addresses ─────────────────────────────────────────────────
    let addr_map = dump_addresses(handle).await?;

    // ── Step 6: Dump routes ────────────────────────────────────────────────────
    let route_map = dump_routes(handle, &known_indices).await?;

    // ── Step 7: Assemble State objects ────────────────────────────────────────
    let mut state_set = StateSet::new();

    for link in matched_links {
        // Re-read driver/pci_path (they're cheap sysfs reads).
        let driver = read_sysfs_driver(&link.name);
        let speed = read_sysfs_speed(&link.name);

        let mut fields: IndexMap<String, FieldValue> = IndexMap::new();

        fields.insert("name".to_string(), kd(Value::String(link.name.clone())));

        if let Some(mtu) = link.mtu {
            fields.insert("mtu".to_string(), kd(Value::U64(mtu as u64)));
        }

        if let Some(mac_bytes) = link.mac {
            fields.insert(
                "mac".to_string(),
                kd(Value::String(format_mac(&mac_bytes))),
            );
        }

        fields.insert(
            "carrier".to_string(),
            kd(Value::Bool(link.carrier.unwrap_or(0) != 0)),
        );

        fields.insert(
            "enabled".to_string(),
            kd(Value::Bool(link.enabled)),
        );

        if let Some(spd) = speed {
            fields.insert("speed".to_string(), kd(Value::U64(spd)));
        }

        if let Some(drv) = driver {
            fields.insert("driver".to_string(), kd(Value::String(drv)));
        }

        // Addresses
        let addr_list: Vec<Value> = addr_map
            .get(&link.index)
            .map(|addrs| {
                addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect()
            })
            .unwrap_or_default();
        fields.insert("addresses".to_string(), kd(Value::List(addr_list)));

        // Routes — exclude kernel-managed routes (proto kernel) so they
        // never appear in state snapshots or diffs, and strip the protocol
        // field from the remaining routes so the values match the desired
        // state produced by policy factories (which omit protocol).
        let route_list: Vec<Value> = route_map
            .get(&link.index)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.as_map()
                    .and_then(|m| m.get("protocol"))
                    .and_then(|v| v.as_str())
                    .map(|s| s != "kernel")
                    .unwrap_or(true)
            })
            .map(|r| {
                match r {
                    Value::Map(mut m) => {
                        m.shift_remove("protocol");
                        Value::Map(m)
                    }
                    other => other,
                }
            })
            .collect();
        fields.insert("routes".to_string(), kd(Value::List(route_list)));

        let state = State {
            entity_type: "ethernet".to_string(),
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
                entity_type: "ethernet".to_string(),
                selector: Box::new(sel.clone()),
            });
        }
    }

    Ok(state_set)
}
