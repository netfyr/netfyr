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
