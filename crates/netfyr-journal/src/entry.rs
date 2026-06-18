use chrono::{DateTime, Utc};
use netfyr_policy::Policy;
use serde::{Deserialize, Serialize};

use crate::serializable::{SerializableDiff, SerializableStateSet};

pub type SequenceId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: SequenceId,
    pub timestamp: DateTime<Utc>,
    pub trigger: Trigger,
    pub active_policies: Vec<PolicySummary>,
    pub diff: SerializableDiff,
    pub state_after: SerializableStateSet,
    pub outcome: ApplyOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    PolicyApply { source: String },
    DhcpEvent { policy_name: String, event_kind: String },
    ExternalChange { changed_entities: Vec<String> },
    DaemonStartup,
    Revert { target_seq: SequenceId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicySummary {
    pub name: String,
    pub factory_type: String,
    pub priority: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied { succeeded: u32, failed: u32, skipped: u32 },
    Observed,
}

pub fn summarize_policies(policies: &[Policy]) -> Vec<PolicySummary> {
    policies
        .iter()
        .map(|p| PolicySummary {
            name: p.name.clone(),
            factory_type: format!("{:?}", p.factory_type).to_lowercase(),
            priority: p.priority,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serializable::{
        SerializableDiff, SerializableDiffOp, SerializableFieldChange, SerializableState,
        SerializableStateSet,
    };

    fn make_test_entry() -> JournalEntry {
        JournalEntry {
            seq: 42,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-21T10:00:00Z")
                .unwrap()
                .into(),
            trigger: Trigger::PolicyApply { source: "policy.yaml".to_string() },
            active_policies: vec![PolicySummary {
                name: "net-config".to_string(),
                factory_type: "static".to_string(),
                priority: 100,
            }],
            diff: SerializableDiff {
                operations: vec![SerializableDiffOp {
                    kind: "modify".to_string(),
                    entity_type: "ethernet".to_string(),
                    entity_name: "eth0".to_string(),
                    field_changes: vec![SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1500u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: None,
                    }],
                }],
            },
            state_after: SerializableStateSet {
                entities: vec![SerializableState {
                    entity_type: "ethernet".to_string(),
                    selector_name: "eth0".to_string(),
                    fields: serde_json::json!({ "mtu": 9000u64 }),
                }],
            },
            outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        }
    }

    /// AC: JournalEntry serializes to and from JSON — round-tripped entry equals the original.
    #[test]
    fn test_journal_entry_round_trips_through_json() {
        let entry = make_test_entry();
        let json = serde_json::to_string(&entry).expect("serialization should not fail");
        let restored: JournalEntry =
            serde_json::from_str(&json).expect("deserialization should not fail");

        assert_eq!(restored.seq, 42);
        assert_eq!(restored.active_policies.len(), 1);
        assert_eq!(restored.active_policies[0].name, "net-config");
        assert_eq!(restored.active_policies[0].factory_type, "static");
        assert_eq!(restored.active_policies[0].priority, 100);
        assert_eq!(restored.diff.operations.len(), 1);
        assert_eq!(restored.diff.operations[0].kind, "modify");
        assert_eq!(restored.diff.operations[0].entity_type, "ethernet");
        assert_eq!(restored.diff.operations[0].entity_name, "eth0");
        assert_eq!(restored.diff.operations[0].field_changes.len(), 1);
        assert_eq!(restored.diff.operations[0].field_changes[0].field_name, "mtu");
        assert_eq!(
            restored.diff.operations[0].field_changes[0].current,
            Some(serde_json::json!(1500u64))
        );
        assert_eq!(
            restored.diff.operations[0].field_changes[0].desired,
            Some(serde_json::json!(9000u64))
        );
        assert_eq!(restored.state_after.entities.len(), 1);
        assert_eq!(restored.state_after.entities[0].entity_type, "ethernet");
        assert_eq!(restored.state_after.entities[0].selector_name, "eth0");
        assert_eq!(restored.state_after.entities[0].fields["mtu"], serde_json::json!(9000u64));
        assert!(matches!(
            restored.outcome,
            ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 }
        ));
        assert!(matches!(restored.trigger, Trigger::PolicyApply { .. }));
    }

    /// AC: All trigger variants serialize correctly with the correct "type" discriminator.
    #[test]
    fn test_all_trigger_variants_serialize_with_correct_type_discriminator() {
        let cases: Vec<(Trigger, &str)> = vec![
            (
                Trigger::PolicyApply { source: "policy.yaml".to_string() },
                "policy_apply",
            ),
            (
                Trigger::DhcpEvent {
                    policy_name: "dhcp-eth0".to_string(),
                    event_kind: "lease_acquired".to_string(),
                },
                "dhcp_event",
            ),
            (
                Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] },
                "external_change",
            ),
            (Trigger::DaemonStartup, "daemon_startup"),
            (Trigger::Revert { target_seq: 3 }, "revert"),
        ];

        for (trigger, expected_type) in cases {
            let json = serde_json::to_string(&trigger)
                .unwrap_or_else(|e| panic!("failed to serialize {:?}: {}", expected_type, e));
            let value: serde_json::Value =
                serde_json::from_str(&json).expect("trigger JSON should be valid");
            let actual_type = value.get("type").and_then(|v| v.as_str());
            assert_eq!(
                actual_type,
                Some(expected_type),
                "trigger variant should have type=\"{}\" but got {:?}",
                expected_type,
                actual_type
            );
        }
    }

    /// AC: PolicyApply trigger contains source field.
    #[test]
    fn test_policy_apply_trigger_contains_source_field() {
        let trigger = Trigger::PolicyApply { source: "config.yaml".to_string() };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["source"].as_str(), Some("config.yaml"));
    }

    /// AC: DhcpEvent trigger contains policy_name and event_kind fields.
    #[test]
    fn test_dhcp_event_trigger_contains_policy_name_and_event_kind() {
        let trigger = Trigger::DhcpEvent {
            policy_name: "dhcp-eth0".to_string(),
            event_kind: "lease_acquired".to_string(),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["policy_name"].as_str(), Some("dhcp-eth0"));
        assert_eq!(value["event_kind"].as_str(), Some("lease_acquired"));
    }

    /// AC: Revert trigger contains target_seq field.
    #[test]
    fn test_revert_trigger_contains_target_seq() {
        let trigger = Trigger::Revert { target_seq: 42 };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["target_seq"].as_u64(), Some(42));
    }

    /// Scenario: Daemon records lease expiry in the journal with the correct trigger.
    ///
    /// When the daemon receives FactoryEvent::LeaseExpired and triggers
    /// re-reconciliation, it records a Trigger::DhcpEvent with
    /// event_kind="lease_expired". This verifies that trigger serializes correctly
    /// so that `netfyr history` can display "lease expired" events and tests like
    /// 403-dhcp-lease-renewal.sh can filter journal entries by event_kind.
    #[test]
    fn test_dhcp_event_trigger_with_lease_expired_event_kind_serializes_correctly() {
        let trigger = Trigger::DhcpEvent {
            policy_name: "e2e-lease-expiry".to_string(),
            event_kind: "lease_expired".to_string(),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            value["type"].as_str(),
            Some("dhcp_event"),
            "LeaseExpired journal trigger must have type='dhcp_event'"
        );
        assert_eq!(
            value["event_kind"].as_str(),
            Some("lease_expired"),
            "LeaseExpired journal trigger must have event_kind='lease_expired'"
        );
        assert_eq!(
            value["policy_name"].as_str(),
            Some("e2e-lease-expiry"),
            "LeaseExpired journal trigger must carry the correct policy_name"
        );
    }

    /// Scenario: Factory re-acquires lease after expiry.
    ///
    /// After LeaseExpired, the factory restarts DORA discovery. When a new lease
    /// is acquired, the daemon triggers re-reconciliation with a DhcpEvent of
    /// event_kind="lease_acquired". This verifies the re-acquisition trigger
    /// round-trips through JSON correctly.
    #[test]
    fn test_dhcp_event_trigger_lease_acquired_after_expiry_round_trips() {
        let trigger = Trigger::DhcpEvent {
            policy_name: "e2e-lease-expiry".to_string(),
            event_kind: "lease_acquired".to_string(),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let restored: Trigger = serde_json::from_str(&json).unwrap();

        // Verify the round-trip preserves the fields correctly.
        match restored {
            Trigger::DhcpEvent { policy_name, event_kind } => {
                assert_eq!(
                    policy_name, "e2e-lease-expiry",
                    "re-acquisition trigger must preserve policy_name"
                );
                assert_eq!(
                    event_kind, "lease_acquired",
                    "re-acquisition trigger must preserve event_kind='lease_acquired'"
                );
            }
            other => panic!(
                "expected DhcpEvent trigger after JSON round-trip, got: {:?}",
                other
            ),
        }
    }

    /// AC: Revert entry contains correct metadata.
    ///
    /// Creates a revert journal entry as run_revert_standalone would, then
    /// verifies trigger, diff, state_after, active_policies, and outcome.
    #[test]
    fn test_revert_journal_entry_contains_correct_metadata() {
        let target_state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 1400u64 }),
            }],
        };

        let diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "mtu".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!(1300u64)),
                    desired: Some(serde_json::json!(1400u64)),
                    outcome: None,
                }],
            }],
        };

        // Build the revert entry the same way run_revert_standalone does.
        let revert_entry = JournalEntry {
            seq: 0,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
                .unwrap()
                .into(),
            trigger: Trigger::Revert { target_seq: 5 },
            active_policies: vec![],
            diff,
            state_after: target_state_after,
            outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        };

        let json = serde_json::to_string(&revert_entry).expect("serialization must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("deserialization must succeed");

        // AC: trigger is "revert" with target_seq=5.
        assert_eq!(
            value["trigger"]["type"].as_str(),
            Some("revert"),
            "revert entry trigger.type must be 'revert'"
        );
        assert_eq!(
            value["trigger"]["target_seq"].as_u64(),
            Some(5),
            "revert entry trigger.target_seq must be 5"
        );

        // AC: diff shows the changes from current state to the target.
        let ops = value["diff"]["operations"]
            .as_array()
            .expect("diff.operations must be array");
        assert_eq!(ops.len(), 1, "diff must have 1 operation");
        assert_eq!(
            ops[0]["kind"].as_str(),
            Some("modify"),
            "diff operation must be 'modify'"
        );
        assert_eq!(ops[0]["entity_name"].as_str(), Some("eth0"));
        let field_changes = ops[0]["field_changes"]
            .as_array()
            .expect("field_changes must be array");
        assert_eq!(
            field_changes[0]["field_name"].as_str(),
            Some("mtu"),
            "field change must be for 'mtu'"
        );
        assert_eq!(
            field_changes[0]["current"].as_u64(),
            Some(1300),
            "current mtu must be 1300"
        );
        assert_eq!(
            field_changes[0]["desired"].as_u64(),
            Some(1400),
            "desired mtu must be 1400"
        );

        // AC: state_after matches the target entry's state_after.
        let entities = value["state_after"]["entities"]
            .as_array()
            .expect("state_after.entities must be array");
        assert_eq!(entities.len(), 1, "state_after must have 1 entity");
        assert_eq!(entities[0]["selector_name"].as_str(), Some("eth0"));
        assert_eq!(
            entities[0]["fields"]["mtu"].as_u64(),
            Some(1400),
            "state_after mtu must be 1400 (the target)"
        );

        // AC: active_policies is empty in daemon-free revert mode.
        let policies = value["active_policies"]
            .as_array()
            .expect("active_policies must be array");
        assert!(
            policies.is_empty(),
            "daemon-free revert entry must have empty active_policies"
        );

        // AC: outcome reflects the apply result.
        assert_eq!(
            value["outcome"]["kind"].as_str(),
            Some("applied"),
            "revert outcome kind must be 'applied'"
        );
        assert_eq!(
            value["outcome"]["succeeded"].as_u64(),
            Some(1),
            "revert outcome succeeded must be 1"
        );
        assert_eq!(
            value["outcome"]["failed"].as_u64(),
            Some(0),
            "revert outcome failed must be 0"
        );
    }

    /// AC: ExternalChange trigger contains changed_entities list.
    #[test]
    fn test_external_change_trigger_contains_changed_entities() {
        let trigger = Trigger::ExternalChange {
            changed_entities: vec!["eth0".to_string(), "eth1".to_string()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entities = value["changed_entities"].as_array().expect("changed_entities should be array");
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].as_str(), Some("eth0"));
        assert_eq!(entities[1].as_str(), Some("eth1"));
    }

    /// AC: ApplyOutcome::Applied serializes with kind "applied" and correct counts.
    #[test]
    fn test_apply_outcome_applied_serializes_with_kind_applied() {
        let outcome = ApplyOutcome::Applied { succeeded: 3, failed: 1, skipped: 2 };
        let json = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["kind"].as_str(), Some("applied"));
        assert_eq!(value["succeeded"].as_u64(), Some(3));
        assert_eq!(value["failed"].as_u64(), Some(1));
        assert_eq!(value["skipped"].as_u64(), Some(2));
    }

    /// AC: ApplyOutcome::Observed serializes with kind "observed".
    #[test]
    fn test_apply_outcome_observed_serializes_with_kind_observed() {
        let outcome = ApplyOutcome::Observed;
        let json = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["kind"].as_str(), Some("observed"));
    }
}
