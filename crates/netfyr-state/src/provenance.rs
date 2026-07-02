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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    // ── Scenario: All types serialize and deserialize with serde ─────────────

    #[test]
    fn test_provenance_user_configured_json_round_trip() {
        let p = Provenance::UserConfigured {
            policy_ref: "my-policy".to_string(),
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        let restored: Provenance = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(p, restored);
    }

    #[test]
    fn test_provenance_kernel_default_json_round_trip() {
        let p = Provenance::KernelDefault;
        let json = serde_json::to_string(&p).expect("must serialize");
        let restored: Provenance = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(p, restored);
    }

    #[test]
    fn test_provenance_external_tool_json_round_trip() {
        let ts = Utc::now();
        let p = Provenance::ExternalTool {
            tool: "iproute2".to_string(),
            detected_at: ts,
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        let restored: Provenance = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(p, restored);
    }

    #[test]
    fn test_provenance_derived_json_round_trip() {
        let p = Provenance::Derived {
            reason: "auto-broadcast".to_string(),
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        let restored: Provenance = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(p, restored);
    }

    // ── Verify internally-tagged serde representation ────────────────────────

    #[test]
    fn test_provenance_user_configured_has_source_tag() {
        let p = Provenance::UserConfigured {
            policy_ref: "bond0".to_string(),
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        assert!(
            json.contains("\"source\""),
            "Provenance must use 'source' as the tag key; got: {json}"
        );
        assert!(
            json.contains("user_configured"),
            "UserConfigured variant must serialize to 'user_configured'; got: {json}"
        );
        assert!(
            json.contains("\"policy_ref\""),
            "policy_ref field must be present; got: {json}"
        );
    }

    #[test]
    fn test_provenance_kernel_default_has_source_tag() {
        let p = Provenance::KernelDefault;
        let json = serde_json::to_string(&p).expect("must serialize");
        assert!(
            json.contains("\"source\""),
            "Provenance must use 'source' as the tag key; got: {json}"
        );
        assert!(
            json.contains("kernel_default"),
            "KernelDefault must serialize to 'kernel_default'; got: {json}"
        );
    }

    #[test]
    fn test_provenance_external_tool_has_source_tag() {
        let ts = Utc::now();
        let p = Provenance::ExternalTool {
            tool: "NetworkManager".to_string(),
            detected_at: ts,
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        assert!(json.contains("external_tool"), "ExternalTool must serialize with 'external_tool'; got: {json}");
        assert!(json.contains("\"tool\""), "tool field must appear; got: {json}");
        assert!(json.contains("\"detected_at\""), "detected_at field must appear; got: {json}");
    }

    #[test]
    fn test_provenance_derived_has_source_tag() {
        let p = Provenance::Derived {
            reason: "auto-broadcast".to_string(),
        };
        let json = serde_json::to_string(&p).expect("must serialize");
        assert!(json.contains("derived"), "Derived must serialize with 'derived'; got: {json}");
        assert!(json.contains("\"reason\""), "reason field must appear; got: {json}");
    }

    // ── Scenario: Provenance enum captures all source types (field-level) ─────

    #[test]
    fn test_provenance_user_configured_policy_ref_field() {
        let p = Provenance::UserConfigured {
            policy_ref: "my-policy".to_string(),
        };
        match p {
            Provenance::UserConfigured { policy_ref } => {
                assert_eq!(policy_ref, "my-policy");
            }
            other => panic!("expected UserConfigured, got {:?}", other),
        }
    }

    #[test]
    fn test_provenance_kernel_default_is_unit_variant() {
        let p = Provenance::KernelDefault;
        // KernelDefault has no associated data; pattern match confirms it.
        assert!(matches!(p, Provenance::KernelDefault));
        // Clone and Debug must work.
        let cloned = p.clone();
        assert_eq!(p, cloned);
        assert!(!format!("{:?}", p).is_empty());
    }

    #[test]
    fn test_provenance_external_tool_fields() {
        let ts = Utc::now();
        let p = Provenance::ExternalTool {
            tool: "iproute2".to_string(),
            detected_at: ts,
        };
        match &p {
            Provenance::ExternalTool { tool, detected_at } => {
                assert_eq!(tool, "iproute2");
                assert_eq!(*detected_at, ts);
            }
            other => panic!("expected ExternalTool, got {:?}", other),
        }
    }

    #[test]
    fn test_provenance_derived_reason_field() {
        let p = Provenance::Derived {
            reason: "auto-broadcast".to_string(),
        };
        match &p {
            Provenance::Derived { reason } => {
                assert_eq!(reason, "auto-broadcast");
            }
            other => panic!("expected Derived, got {:?}", other),
        }
    }

    // ── Scenario: All types implement Clone, Debug, PartialEq ────────────────

    #[test]
    fn test_provenance_all_variants_clone_and_partial_eq() {
        let ts = Utc::now();
        let variants = vec![
            Provenance::UserConfigured {
                policy_ref: "p".to_string(),
            },
            Provenance::KernelDefault,
            Provenance::ExternalTool {
                tool: "iproute2".to_string(),
                detected_at: ts,
            },
            Provenance::Derived {
                reason: "auto-broadcast".to_string(),
            },
        ];
        for v in &variants {
            let cloned = v.clone();
            assert_eq!(v, &cloned, "Clone must equal original for {:?}", v);
            assert!(!format!("{:?}", v).is_empty());
        }
    }
}
