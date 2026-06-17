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
}
