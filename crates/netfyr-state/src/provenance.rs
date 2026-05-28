use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tracks where a field value originated.
///
/// Uses internally tagged serde representation (`{"source": "kernel_default"}` etc.)
/// which is self-documenting in JSON/YAML and handles unit-like variants cleanly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Provenance {
    /// Explicitly set by a user in a policy.
    UserConfigured { policy_ref: String },
    /// Never changed; reflects the kernel's initial value.
    KernelDefault,
    /// Change detected from an external tool (e.g., iproute2, NetworkManager).
    ExternalTool {
        tool: String,
        detected_at: DateTime<Utc>,
    },
    /// Computed by netfyr (e.g., auto-calculated broadcast address).
    Derived { reason: String },
}
