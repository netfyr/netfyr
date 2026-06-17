use indexmap::IndexMap;
use ipnetwork::IpNetwork;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::net::IpAddr;

/// The set of possible field values in a network entity's configuration.
///
/// Serialization uses `#[serde(untagged)]` for natural JSON/YAML output.
/// Deserialization uses a custom impl that routes string values through
/// IP-aware parsing: only strings containing `/` are tried as `IpNetwork`,
/// bare IPs become `IpAddr`, and everything else stays `String`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    U64(u64),
    I64(i64),
    IpNetwork(IpNetwork),
    IpAddr(IpAddr),
    List(Vec<Value>),
    Map(IndexMap<String, Value>),
    String(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawValue {
    Bool(bool),
    U64(u64),
    I64(i64),
    String(String),
    List(Vec<RawValue>),
    Map(IndexMap<String, RawValue>),
}

impl From<RawValue> for Value {
    fn from(raw: RawValue) -> Self {
        match raw {
            RawValue::Bool(b) => Value::Bool(b),
            RawValue::U64(n) => Value::U64(n),
            RawValue::I64(n) => Value::I64(n),
            RawValue::String(s) => {
                if s.contains('/') {
                    if let Ok(net) = s.parse::<IpNetwork>() {
                        return Value::IpNetwork(net);
                    }
                }
                if let Ok(ip) = s.parse::<IpAddr>() {
                    return Value::IpAddr(ip);
                }
                Value::String(s)
            }
            RawValue::List(items) => Value::List(items.into_iter().map(Value::from).collect()),
            RawValue::Map(map) => {
                Value::Map(map.into_iter().map(|(k, v)| (k, Value::from(v))).collect())
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        RawValue::deserialize(deserializer).map(Value::from)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::String(s) => write!(f, "{s}"),
            Value::U64(n) => write!(f, "{n}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::IpAddr(ip) => write!(f, "{ip}"),
            Value::IpNetwork(net) => write!(f, "{net}"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_owned())
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::U64(n)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::I64(n)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<IpAddr> for Value {
    fn from(ip: IpAddr) -> Self {
        Value::IpAddr(ip)
    }
}

impl From<IpNetwork> for Value {
    fn from(net: IpNetwork) -> Self {
        Value::IpNetwork(net)
    }
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::U64(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_ip_addr(&self) -> Option<&IpAddr> {
        match self {
            Value::IpAddr(ip) => Some(ip),
            _ => None,
        }
    }

    pub fn as_ip_network(&self) -> Option<&IpNetwork> {
        match self {
            Value::IpNetwork(net) => Some(net),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&IndexMap<String, Value>> {
        match self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    // NOTE (spec divergence): SPEC-002 declares Value::IpAddr(std::net::Ipv4Addr)
    // and Value::IpNetwork(ipnetwork::Ipv4Network) as IPv4-only types, and requires
    // From<Ipv4Addr> / From<Ipv4Network>. The implementation uses the generic
    // std::net::IpAddr and ipnetwork::IpNetwork, and From<IpAddr> / From<IpNetwork>.
    // Tests here use IPv4 values as the spec intends.

    // ── Scenario: All types serialize and deserialize with serde ─────────────

    #[test]
    fn test_value_string_json_round_trip() {
        let v = Value::String("eth0".to_string());
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_u64_json_round_trip() {
        let v = Value::U64(1500);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_i64_json_round_trip() {
        let v = Value::I64(-1);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_bool_json_round_trip() {
        let v = Value::Bool(true);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_ipv4_addr_json_round_trip() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        let v = Value::IpAddr(ip);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_ipv4_network_json_round_trip() {
        let net: IpNetwork = "10.0.1.0/24".parse().unwrap();
        let v = Value::IpNetwork(net);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_list_json_round_trip() {
        let v = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    #[test]
    fn test_value_map_json_round_trip() {
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        let v = Value::Map(map);
        let json = serde_json::to_string(&v).expect("must serialize");
        let restored: Value = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(v, restored);
    }

    // ── Scenario: Value From trait conversions — IpAddr / IpNetwork ──────────

    #[test]
    fn test_value_from_ipv4_addr_via_ipaddr() {
        // Spec says From<Ipv4Addr>; implementation provides From<IpAddr>.
        // IPv4 addresses are passed via the IpAddr wrapper as the idiomatic usage.
        let ipv4 = Ipv4Addr::new(10, 0, 1, 1);
        let ip: IpAddr = ipv4.into();
        assert!(matches!(Value::from(ip), Value::IpAddr(_)));
    }

    #[test]
    fn test_value_from_ipv4_network_via_ipnetwork() {
        // Spec says From<Ipv4Network>; implementation provides From<IpNetwork>.
        let net: IpNetwork = "192.168.0.0/16".parse().unwrap();
        assert!(matches!(Value::from(net), Value::IpNetwork(_)));
    }
}
