//! Netlink-based `NetworkBackend` implementation for Linux.
//!
//! Provides `NetlinkBackend`, which queries and applies changes to kernel
//! networking state via the `rtnetlink` crate.

pub mod apply;
pub mod ethernet;
pub mod interface;
pub mod query;

use async_trait::async_trait;
use netfyr_state::{entity_types::{ETHERNET, WIFI}, EntityType, Selector, StateDiff, StateSet};

use crate::{ApplyReport, BackendError, DryRunReport, NetworkBackend};

use query::establish_connection;

// ── NetlinkBackend ────────────────────────────────────────────────────────────

/// `NetworkBackend` implementation backed by Linux netlink (rtnetlink).
///
/// Supports the `"ethernet"` and `"wifi"` entity types. A new netlink connection
/// is opened per query call — see [`query::establish_connection`] for rationale.
pub struct NetlinkBackend {
    supported_entities: Vec<EntityType>,
}

impl NetlinkBackend {
    /// Create a new `NetlinkBackend` with the default supported entity types.
    pub fn new() -> Self {
        Self {
            supported_entities: vec![ETHERNET.to_string(), WIFI.to_string()],
        }
    }
}

impl Default for NetlinkBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NetworkBackend for NetlinkBackend {
    fn supported_entities(&self) -> &[EntityType] {
        &self.supported_entities
    }

    async fn query(
        &self,
        entity_type: &EntityType,
        selector: Option<&Selector>,
    ) -> Result<StateSet, BackendError> {
        match entity_type.as_str() {
            ETHERNET => {
                let handle = establish_connection().await?;
                interface::query_interfaces(&handle, Some(ETHERNET), selector).await
            }
            WIFI => {
                let handle = establish_connection().await?;
                interface::query_interfaces(&handle, Some(WIFI), selector).await
            }
            _ => Err(BackendError::UnsupportedEntityType(entity_type.clone())),
        }
    }

    async fn query_all(&self) -> Result<StateSet, BackendError> {
        // Iterates all supported entity types and merges results.
        let mut merged = StateSet::new();
        for entity_type in &self.supported_entities {
            let result = self.query(entity_type, None).await?;
            // Merge by inserting — StateSet::insert overwrites on same key.
            for state in result.iter() {
                merged.insert(state.clone());
            }
        }
        Ok(merged)
    }

    async fn apply(&self, diff: &StateDiff) -> Result<ApplyReport, BackendError> {
        let handle = establish_connection().await?;
        apply::apply_ethernet(&handle, diff).await
    }

    async fn dry_run(&self, diff: &StateDiff) -> Result<DryRunReport, BackendError> {
        let handle = establish_connection().await?;
        apply::dry_run_ethernet(&handle, diff).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: NetlinkBackend supports entity type "ethernet" as required by the spec.
    #[test]
    fn test_netlink_backend_new_supports_ethernet_entity_type() {
        let backend = NetlinkBackend::new();
        assert!(
            backend.supported_entities().contains(&"ethernet".to_string()),
            "NetlinkBackend::new() must include 'ethernet' in supported_entities"
        );
    }

    /// Scenario: NetlinkBackend supports entity type "wifi".
    #[test]
    fn test_netlink_backend_supports_wifi_entity_type() {
        let backend = NetlinkBackend::new();
        assert!(
            backend.supported_entities().contains(&"wifi".to_string()),
            "NetlinkBackend::new() must include 'wifi' in supported_entities"
        );
    }

    /// Scenario: query_all includes all interfaces — the backend must advertise
    /// exactly two entity types ("ethernet" and "wifi").
    #[test]
    fn test_netlink_backend_supported_entities_has_exactly_two_entries() {
        let backend = NetlinkBackend::new();
        let entities = backend.supported_entities();
        assert_eq!(
            entities.len(),
            2,
            "NetlinkBackend must support exactly two entity types; got: {:?}",
            entities
        );
        assert!(entities.contains(&"ethernet".to_string()));
        assert!(entities.contains(&"wifi".to_string()));
    }

    /// NetlinkBackend::default() must produce the same supported_entities as ::new().
    #[test]
    fn test_netlink_backend_default_has_same_supported_entities_as_new() {
        let via_new = NetlinkBackend::new();
        let via_default = NetlinkBackend::default();
        assert_eq!(
            via_new.supported_entities(),
            via_default.supported_entities(),
            "Default::default() and ::new() must report the same supported_entities"
        );
    }

    /// Querying an unsupported entity type returns UnsupportedEntityType immediately
    /// (no netlink connection is opened).
    #[tokio::test]
    async fn test_query_unsupported_entity_type_returns_error() {
        let backend = NetlinkBackend::new();
        let result = backend.query(&"bond".to_string(), None).await;
        match result {
            Err(BackendError::UnsupportedEntityType(t)) => {
                assert_eq!(t, "bond", "error must name the unsupported entity type");
            }
            other => panic!("expected UnsupportedEntityType, got {:?}", other),
        }
    }
}
