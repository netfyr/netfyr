use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Identity and tracking metadata for a state instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateMetadata {
    /// UUIDv7 (time-ordered) unique identifier for this state instance.
    pub id: Uuid,
    /// Stable across versions of the same logical entity.
    pub timeline_id: Uuid,
    /// When this state was created.
    pub created_at: DateTime<Utc>,
    /// User-defined key-value labels.
    pub labels: HashMap<String, String>,
    /// Optional human-readable description.
    pub description: Option<String>,
}

impl StateMetadata {
    pub fn new() -> Self {
        Self {
            id: Uuid::now_v7(),
            timeline_id: Uuid::now_v7(),
            created_at: Utc::now(),
            labels: HashMap::new(),
            description: None,
        }
    }
}

impl Default for StateMetadata {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario: All types serialize and deserialize with serde ─────────────

    #[test]
    fn test_state_metadata_json_round_trip_empty() {
        let m = StateMetadata::new();
        let json = serde_json::to_string(&m).expect("must serialize");
        let restored: StateMetadata = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(m, restored);
    }

    #[test]
    fn test_state_metadata_json_round_trip_with_labels_and_description() {
        let mut m = StateMetadata::new();
        m.labels.insert("role".to_string(), "uplink".to_string());
        m.labels.insert("env".to_string(), "prod".to_string());
        m.description = Some("primary uplink interface".to_string());

        let json = serde_json::to_string(&m).expect("must serialize");
        let restored: StateMetadata = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(m, restored);
        assert_eq!(restored.labels.get("role").map(|s| s.as_str()), Some("uplink"));
        assert_eq!(restored.description.as_deref(), Some("primary uplink interface"));
    }

    // ── Scenario: StateMetadata implements Clone, Debug, PartialEq ───────────

    #[test]
    fn test_state_metadata_clone_equals_original() {
        let m = StateMetadata::new();
        let cloned = m.clone();
        assert_eq!(m, cloned);
    }

    #[test]
    fn test_state_metadata_debug_produces_non_empty_string() {
        let m = StateMetadata::new();
        assert!(!format!("{:?}", m).is_empty());
    }

    // ── Scenario: StateMetadata generates unique IDs ──────────────────────────

    #[test]
    fn test_state_metadata_new_generates_unique_id_fields() {
        let m1 = StateMetadata::new();
        let m2 = StateMetadata::new();
        assert_ne!(m1.id, m2.id, "Two StateMetadata instances must have different id values");
        assert_ne!(
            m1.timeline_id,
            m2.timeline_id,
            "Two StateMetadata instances must have different timeline_id values"
        );
    }

    #[test]
    fn test_state_metadata_new_ids_are_uuidv7() {
        let m = StateMetadata::new();
        // UUIDv7 has version nibble = 7.
        assert_eq!(m.id.get_version_num(), 7, "id must be a UUIDv7");
        assert_eq!(m.timeline_id.get_version_num(), 7, "timeline_id must be a UUIDv7");
    }

    #[test]
    fn test_state_metadata_new_created_at_is_recent() {
        let before = Utc::now();
        let m = StateMetadata::new();
        let after = Utc::now();
        assert!(
            m.created_at >= before && m.created_at <= after,
            "created_at must be within the current second; got {:?}",
            m.created_at
        );
    }

    #[test]
    fn test_state_metadata_new_labels_is_empty() {
        let m = StateMetadata::new();
        assert!(m.labels.is_empty(), "labels must start as an empty HashMap");
    }

    #[test]
    fn test_state_metadata_new_description_is_none() {
        let m = StateMetadata::new();
        assert!(m.description.is_none(), "description must start as None");
    }
}
