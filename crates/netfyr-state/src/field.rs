use crate::{Provenance, Value};
use serde::{Deserialize, Serialize};

/// A field's value paired with its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldValue {
    pub value: Value,
    pub provenance: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario: FieldValue pairs a value with provenance ───────────────────

    #[test]
    fn test_field_value_stores_value_and_provenance_user_configured() {
        let fv = FieldValue {
            value: Value::U64(9000),
            provenance: Provenance::UserConfigured {
                policy_ref: "bond0".to_string(),
            },
        };
        assert_eq!(fv.value, Value::U64(9000));
        assert_eq!(
            fv.provenance,
            Provenance::UserConfigured {
                policy_ref: "bond0".to_string()
            }
        );
    }

    // ── Scenario: All types serialize and deserialize with serde ─────────────

    #[test]
    fn test_field_value_json_round_trip_user_configured() {
        let fv = FieldValue {
            value: Value::U64(9000),
            provenance: Provenance::UserConfigured {
                policy_ref: "bond0".to_string(),
            },
        };
        let json = serde_json::to_string(&fv).expect("must serialize");
        let restored: FieldValue = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(fv, restored);
    }

    #[test]
    fn test_field_value_json_round_trip_kernel_default() {
        let fv = FieldValue {
            value: Value::Bool(true),
            provenance: Provenance::KernelDefault,
        };
        let json = serde_json::to_string(&fv).expect("must serialize");
        let restored: FieldValue = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(fv, restored);
    }

    #[test]
    fn test_field_value_json_round_trip_derived() {
        let fv = FieldValue {
            value: Value::String("computed-value".to_string()),
            provenance: Provenance::Derived {
                reason: "auto-broadcast".to_string(),
            },
        };
        let json = serde_json::to_string(&fv).expect("must serialize");
        let restored: FieldValue = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(fv, restored);
    }

    #[test]
    fn test_field_value_json_round_trip_external_tool() {
        let fv = FieldValue {
            value: Value::String("1500".to_string()),
            provenance: Provenance::ExternalTool {
                tool: "iproute2".to_string(),
                detected_at: chrono::Utc::now(),
            },
        };
        let json = serde_json::to_string(&fv).expect("must serialize");
        let restored: FieldValue = serde_json::from_str(&json).expect("must deserialize");
        assert_eq!(fv, restored);
    }

    // ── Scenario: FieldValue implements Clone, Debug, PartialEq ─────────────

    #[test]
    fn test_field_value_clone_equals_original() {
        let fv = FieldValue {
            value: Value::U64(9000),
            provenance: Provenance::UserConfigured {
                policy_ref: "bond0".to_string(),
            },
        };
        let cloned = fv.clone();
        assert_eq!(fv, cloned);
    }

    #[test]
    fn test_field_value_debug_produces_non_empty_string() {
        let fv = FieldValue {
            value: Value::U64(1500),
            provenance: Provenance::KernelDefault,
        };
        assert!(!format!("{:?}", fv).is_empty());
    }
}
