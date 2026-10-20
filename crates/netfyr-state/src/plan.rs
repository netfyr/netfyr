//! Shared observations and operation plans.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Instant;

use crate::{Source, State, Value};

/// Identity of an open Linux network namespace, from its device and inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NamespaceId {
    pub device: u64,
    pub inode: u64,
}

/// A discovered interface; names and interface indices alone may be reused.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceId {
    pub namespace: NamespaceId,
    pub index: u32,
    /// Backend-issued incarnation token, invalidated when the link disappears.
    pub generation: u64,
    pub name: String,
    pub device_type: String,
    pub mac: Option<String>,
}

/// One host address, retaining its host bits and requested lifetimes in seconds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ipv4Address {
    pub ip: Ipv4Addr,
    pub prefix_len: u8,
    pub valid_lft: Option<u32>,
    pub preferred_lft: Option<u32>,
}

impl Ipv4Address {
    /// Masked prefix group, or `None` for an invalid prefix length.
    pub fn group(&self) -> Option<(Ipv4Addr, u8)> {
        masked(self.ip, self.prefix_len)
    }

    pub fn same_identity(&self, other: &Ipv4Address) -> bool {
        self.ip == other.ip && self.prefix_len == other.prefix_len
    }

    /// Resolve omitted lifetimes: valid defaults to forever, preferred to valid.
    /// Linux represents forever as `u32::MAX`.
    pub fn lifetimes(&self) -> (u32, u32) {
        let valid = self.valid_lft.unwrap_or(u32::MAX);
        (valid, self.preferred_lft.unwrap_or(valid))
    }

    /// Validate an address for addition or replacement. An observation may
    /// contain an expired address, but adding a zero valid lifetime is invalid.
    pub fn validate(&self) -> Result<(), AddressError> {
        if self.prefix_len > 32 {
            return Err(AddressError::PrefixTooLong);
        }
        let (valid, preferred) = self.lifetimes();
        if valid == 0 {
            return Err(AddressError::ZeroValidLifetime);
        }
        if preferred > valid {
            return Err(AddressError::PreferredExceedsValid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressError {
    PrefixTooLong,
    ZeroValidLifetime,
    PreferredExceedsValid,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PrefixTooLong => "IPv4 prefix length exceeds 32",
            Self::ZeroValidLifetime => "IPv4 valid lifetime must be greater than zero",
            Self::PreferredExceedsValid => "IPv4 preferred lifetime exceeds valid lifetime",
        })
    }
}

impl std::error::Error for AddressError {}

fn masked(ip: Ipv4Addr, prefix_len: u8) -> Option<(Ipv4Addr, u8)> {
    if prefix_len > 32 {
        return None;
    }
    let mask = u32::MAX
        .checked_shl(u32::from(32 - prefix_len))
        .unwrap_or(0);
    Some((Ipv4Addr::from(u32::from(ip) & mask), prefix_len))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressObservation {
    pub address: Ipv4Address,
    /// Linux `IFA_F_*` bits.
    pub flags: u32,
    /// Linux `RT_SCOPE_*` value.
    pub scope: u8,
    pub peer: Option<Ipv4Addr>,
}

impl AddressObservation {
    /// Primary/secondary prefix group, using the peer when present.
    pub fn kernel_group(&self) -> Option<(Ipv4Addr, u8)> {
        masked(
            self.peer.unwrap_or(self.address.ip),
            self.address.prefix_len,
        )
    }
}

#[derive(Clone, Debug)]
pub struct DeviceObservation {
    pub identity: InterfaceId,
    pub state: State,
    pub addresses: Vec<AddressObservation>,
    /// False forbids destructive planning based on this address inventory.
    pub addresses_complete: bool,
    /// The reference time for the remaining lifetimes in `addresses`.
    pub observed_at: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct Observation {
    pub devices: Vec<DeviceObservation>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub target: Option<InterfaceId>,
    pub field: Option<String>,
    pub message: String,
}

/// Complete, ordered prefix group snapshot for the first operation. Later operations
/// use the executor's updated snapshot instead of their own preconditions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressGroupPrecondition {
    pub addresses: Vec<AddressObservation>,
    pub observed_at: Instant,
}

/// Whole-entity actions are reserved and never imply address deconfiguration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationKind {
    SetMtu(u32),
    SetEnabled(bool),
    AddIpv4(Ipv4Address),
    RemoveIpv4(Ipv4Address),
    ReplaceIpv4(Ipv4Address),
    /// A schema field assignment. Writable `mtu` and `enabled` decode to
    /// `SetMtu` and `SetEnabled`; read-only fields are skipped.
    SetField {
        field: String,
        value: Value,
    },
    AddEntity,
    RemoveEntity,
}

impl OperationKind {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::SetMtu(_) => "set_mtu",
            Self::SetEnabled(_) => "set_enabled",
            Self::AddIpv4(_) => "add_ipv4",
            Self::RemoveIpv4(_) => "remove_ipv4",
            Self::ReplaceIpv4(_) => "replace_ipv4",
            Self::SetField { .. } => "set_field",
            Self::AddEntity => "add_entity",
            Self::RemoveEntity => "remove_entity",
        }
    }

    pub fn ipv4_address(&self) -> Option<&Ipv4Address> {
        match self {
            Self::AddIpv4(address) | Self::RemoveIpv4(address) | Self::ReplaceIpv4(address) => {
                Some(address)
            }
            _ => None,
        }
    }

    pub fn field(&self) -> Option<&str> {
        match self {
            Self::SetMtu(_) => Some("mtu"),
            Self::SetEnabled(_) => Some("enabled"),
            Self::AddIpv4(_) | Self::RemoveIpv4(_) | Self::ReplaceIpv4(_) => Some("ipv4.addresses"),
            Self::SetField { field, .. } => Some(field),
            Self::AddEntity | Self::RemoveEntity => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub target: InterfaceId,
    pub kind: OperationKind,
    /// Indices of earlier operations in the same `StateDiff`.
    pub depends_on: Vec<usize>,
    pub address_group: Option<AddressGroupPrecondition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldChange {
    pub target: InterfaceId,
    pub field: String,
    pub old: Option<Value>,
    pub desired: Option<Value>,
    pub source: Source,
}

/// Operations are executed in list order; no implicit reordering or additional
/// removals can be inferred from the desired field values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDiff {
    pub changes: Vec<FieldChange>,
    pub operations: Vec<Operation>,
    pub diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
mod tests {
    use super::{AddressError, AddressObservation, Ipv4Address, OperationKind};
    use std::net::Ipv4Addr;

    fn address(prefix_len: u8) -> Ipv4Address {
        Ipv4Address {
            ip: Ipv4Addr::new(192, 0, 2, 129),
            prefix_len,
            valid_lft: None,
            preferred_lft: None,
        }
    }

    #[test]
    fn prefix_groups_keep_host_identity_and_handle_boundaries() {
        for (prefix, network) in [
            (0, Ipv4Addr::UNSPECIFIED),
            (1, Ipv4Addr::new(128, 0, 0, 0)),
            (24, Ipv4Addr::new(192, 0, 2, 0)),
            (25, Ipv4Addr::new(192, 0, 2, 128)),
            (31, Ipv4Addr::new(192, 0, 2, 128)),
            (32, Ipv4Addr::new(192, 0, 2, 129)),
        ] {
            let addr = address(prefix);
            assert_eq!(addr.group(), Some((network, prefix)));
            assert_eq!(addr.ip, Ipv4Addr::new(192, 0, 2, 129));
        }
        assert_eq!(address(33).group(), None);
        assert_eq!(address(u8::MAX).group(), None);
    }

    #[test]
    fn lifetimes_preserve_explicit_deprecation_and_finite_defaults() {
        let mut addr = address(24);
        assert_eq!(addr.lifetimes(), (u32::MAX, u32::MAX));
        addr.valid_lft = Some(3600);
        assert_eq!(addr.lifetimes(), (3600, 3600));
        addr.preferred_lft = Some(0);
        assert_eq!(addr.lifetimes(), (3600, 0));
        assert_eq!(addr.validate(), Ok(()));
        addr.valid_lft = None;
        assert_eq!(addr.lifetimes(), (u32::MAX, 0));
    }

    #[test]
    fn addition_validation_rejects_invalid_prefix_and_lifetime_order() {
        assert_eq!(address(33).validate(), Err(AddressError::PrefixTooLong));
        let mut addr = address(24);
        addr.valid_lft = Some(0);
        assert_eq!(addr.validate(), Err(AddressError::ZeroValidLifetime));
        addr.valid_lft = Some(60);
        addr.preferred_lft = Some(61);
        assert_eq!(addr.validate(), Err(AddressError::PreferredExceedsValid));
        addr.preferred_lft = Some(60);
        assert_eq!(addr.validate(), Ok(()));
    }

    #[test]
    fn identity_ignores_lifetimes_but_not_prefix() {
        let mut other = address(24);
        other.valid_lft = Some(5);
        assert!(address(24).same_identity(&other));
        assert!(!address(24).same_identity(&address(25)));
    }

    #[test]
    fn kernel_group_follows_the_peer_when_present() {
        let mut observed = AddressObservation {
            address: address(24),
            flags: 0,
            scope: 0,
            peer: None,
        };
        assert_eq!(
            observed.kernel_group(),
            Some((Ipv4Addr::new(192, 0, 2, 0), 24))
        );
        observed.peer = Some(Ipv4Addr::new(198, 51, 100, 9));
        assert_eq!(
            observed.kernel_group(),
            Some((Ipv4Addr::new(198, 51, 100, 0), 24))
        );
        observed.address.prefix_len = 33;
        assert_eq!(observed.kernel_group(), None);
    }

    #[test]
    fn operation_names_fields_and_addresses_are_reported_per_kind() {
        let addr = address(24);
        let cases = [
            (OperationKind::SetMtu(1500), "set_mtu", Some("mtu"), None),
            (
                OperationKind::SetEnabled(true),
                "set_enabled",
                Some("enabled"),
                None,
            ),
            (
                OperationKind::AddIpv4(addr.clone()),
                "add_ipv4",
                Some("ipv4.addresses"),
                Some(&addr),
            ),
            (
                OperationKind::RemoveIpv4(addr.clone()),
                "remove_ipv4",
                Some("ipv4.addresses"),
                Some(&addr),
            ),
            (
                OperationKind::ReplaceIpv4(addr.clone()),
                "replace_ipv4",
                Some("ipv4.addresses"),
                Some(&addr),
            ),
            (
                OperationKind::SetField {
                    field: "ethernet.speed".into(),
                    value: crate::Value::U64(1),
                },
                "set_field",
                Some("ethernet.speed"),
                None,
            ),
            (OperationKind::AddEntity, "add_entity", None, None),
            (OperationKind::RemoveEntity, "remove_entity", None, None),
        ];
        for (kind, name, field, address) in cases {
            assert_eq!(kind.kind_name(), name);
            assert_eq!(kind.field(), field, "{name}");
            assert_eq!(kind.ipv4_address(), address, "{name}");
        }
    }
}
