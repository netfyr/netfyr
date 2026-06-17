//! The `NetworkBackend` async trait.

use async_trait::async_trait;
use netfyr_state::{EntityType, Selector, StateDiff, StateSet};

use crate::{ApplyReport, BackendError, DryRunReport};

// ── NetworkBackend ────────────────────────────────────────────────────────────

/// Uniform interface for interacting with a kernel subsystem that manages a
/// specific set of network entity types.
///
/// Implementors must store the list of supported entity types (e.g., as a
/// `Vec<EntityType>` field) and return a slice from `supported_entities`.
///
/// The `async-trait` macro desugars each async method to return
/// `Pin<Box<dyn Future + Send>>`, which enables `dyn NetworkBackend` trait
/// objects across async boundaries.
#[async_trait]
pub trait NetworkBackend: Send + Sync {
    /// Query entities of a specific type, optionally filtered by selector.
    ///
    /// Returns a `StateSet` containing the current system state for matching
    /// entities. Returns `BackendError::UnsupportedEntityType` when the entity
    /// type is not handled by this backend.
    async fn query(
        &self,
        entity_type: &EntityType,
        selector: Option<&Selector>,
    ) -> Result<StateSet, BackendError>;

    /// Query all entities supported by this backend.
    ///
    /// Returns a `StateSet` containing the current system state across all
    /// entity types this backend handles.
    async fn query_all(&self) -> Result<StateSet, BackendError>;

    /// Apply a `StateDiff` to the system.
    ///
    /// Executes each add/modify/remove operation and returns a report that
    /// categorises every operation as succeeded, failed, or skipped. Individual
    /// operation failures are captured in the report rather than returned as
    /// `Err`; `Err` is reserved for systemic failures (e.g., cannot reach the
    /// kernel subsystem at all).
    async fn apply(&self, diff: &StateDiff) -> Result<ApplyReport, BackendError>;

    /// Simulate applying a `StateDiff` without making any system changes.
    ///
    /// Returns a report of what would happen, including per-field before/after
    /// values.
    async fn dry_run(&self, diff: &StateDiff) -> Result<DryRunReport, BackendError>;

    /// Return the list of entity types this backend can handle.
    ///
    /// Implementors must store the list as an owned collection and return a
    /// slice; constructing the list on the fly is not possible because the
    /// method returns a borrowed reference.
    fn supported_entities(&self) -> &[EntityType];
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use indexmap::IndexMap;
    use netfyr_state::{
        DiffOp, EntityType, FieldValue, Provenance, Selector, State, StateDiff, StateMetadata,
        StateSet, Value,
    };

    use crate::{
        AppliedOperation, ApplyReport, BackendError, DiffOpKind, DryRunReport, FailedOperation,
        FieldChange, FieldChangeKind, NetworkBackend, PlannedChange, SkippedOperation,
    };

    struct MockBackend {
        supported: Vec<EntityType>,
        state: StateSet,
        fail_selectors: HashSet<String>,
        skip_selectors: HashSet<String>,
    }

    #[async_trait]
    impl NetworkBackend for MockBackend {
        fn supported_entities(&self) -> &[EntityType] {
            &self.supported
        }

        async fn query(
            &self,
            entity_type: &EntityType,
            selector: Option<&Selector>,
        ) -> Result<StateSet, BackendError> {
            if !self.supported.contains(entity_type) {
                return Err(BackendError::UnsupportedEntityType(entity_type.clone()));
            }
            let mut result = StateSet::new();
            for state in self.state.iter() {
                if &state.entity_type != entity_type {
                    continue;
                }
                if let Some(sel) = selector {
                    if !sel.matches(&state.selector) {
                        continue;
                    }
                }
                result.insert(state.clone());
            }
            Ok(result)
        }

        async fn query_all(&self) -> Result<StateSet, BackendError> {
            let mut result = StateSet::new();
            for state in self.state.iter() {
                result.insert(state.clone());
            }
            Ok(result)
        }

        async fn apply(&self, diff: &StateDiff) -> Result<ApplyReport, BackendError> {
            let mut report = ApplyReport::new();
            for op in diff.ops() {
                let selector_key = op.selector().key();
                if self.fail_selectors.contains(&selector_key) {
                    let fields = match op {
                        DiffOp::Add { fields, .. } => fields.keys().cloned().collect(),
                        DiffOp::Modify { changed_fields, removed_fields, .. } => {
                            changed_fields.keys().chain(removed_fields.iter()).cloned().collect()
                        }
                        DiffOp::Remove { .. } => vec![],
                    };
                    report.failed.push(FailedOperation {
                        operation: DiffOpKind::from(op),
                        entity_type: op.entity_type().to_string(),
                        selector: op.selector().clone(),
                        error: BackendError::Internal("mock apply failure".to_string()),
                        fields,
                    });
                } else if self.skip_selectors.contains(&selector_key) {
                    report.skipped.push(SkippedOperation {
                        operation: DiffOpKind::from(op),
                        entity_type: op.entity_type().to_string(),
                        selector: op.selector().clone(),
                        reason: "entity already in desired state".to_string(),
                    });
                } else {
                    let fields_changed = match op {
                        DiffOp::Add { fields, .. } => fields.keys().cloned().collect(),
                        DiffOp::Modify { changed_fields, removed_fields, .. } => {
                            changed_fields.keys().chain(removed_fields.iter()).cloned().collect()
                        }
                        DiffOp::Remove { .. } => vec![],
                    };
                    report.succeeded.push(AppliedOperation {
                        operation: DiffOpKind::from(op),
                        entity_type: op.entity_type().to_string(),
                        selector: op.selector().clone(),
                        fields_changed,
                    });
                }
            }
            Ok(report)
        }

        async fn dry_run(&self, diff: &StateDiff) -> Result<DryRunReport, BackendError> {
            let mut report = DryRunReport::new();
            for op in diff.ops() {
                let planned = match op {
                    DiffOp::Add { entity_type, selector, fields } => PlannedChange {
                        operation: DiffOpKind::Add,
                        entity_type: entity_type.clone(),
                        selector: selector.clone(),
                        field_changes: fields
                            .iter()
                            .map(|(name, fv)| FieldChange {
                                field: name.clone(),
                                current: None,
                                desired: Some(fv.value.clone()),
                                kind: FieldChangeKind::Set,
                            })
                            .collect(),
                    },
                    DiffOp::Modify { entity_type, selector, changed_fields, removed_fields } => {
                        let current_state = self.state.get(entity_type, &selector.key());
                        let mut field_changes: Vec<FieldChange> = changed_fields
                            .iter()
                            .map(|(name, fv)| {
                                let current = current_state
                                    .and_then(|s| s.fields.get(name))
                                    .map(|cfv| cfv.value.clone());
                                FieldChange {
                                    field: name.clone(),
                                    current,
                                    desired: Some(fv.value.clone()),
                                    kind: FieldChangeKind::Modify,
                                }
                            })
                            .collect();
                        for name in removed_fields {
                            let current = current_state
                                .and_then(|s| s.fields.get(name))
                                .map(|cfv| cfv.value.clone());
                            field_changes.push(FieldChange {
                                field: name.clone(),
                                current,
                                desired: None,
                                kind: FieldChangeKind::Unset,
                            });
                        }
                        PlannedChange {
                            operation: DiffOpKind::Modify,
                            entity_type: entity_type.clone(),
                            selector: selector.clone(),
                            field_changes,
                        }
                    }
                    DiffOp::Remove { entity_type, selector } => PlannedChange {
                        operation: DiffOpKind::Remove,
                        entity_type: entity_type.clone(),
                        selector: selector.clone(),
                        field_changes: vec![],
                    },
                };
                report.changes.push(planned);
            }
            Ok(report)
        }
    }

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut state_fields = IndexMap::new();
        for (key, value) in fields {
            state_fields.insert(
                key.to_string(),
                FieldValue { value, provenance: Provenance::KernelDefault },
            );
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: state_fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        }
    }

    fn make_mock(supported: &[&str], states: Vec<State>) -> MockBackend {
        let mut state_set = StateSet::new();
        for s in states {
            state_set.insert(s);
        }
        MockBackend {
            supported: supported.iter().map(|s| s.to_string()).collect(),
            state: state_set,
            fail_selectors: HashSet::new(),
            skip_selectors: HashSet::new(),
        }
    }

    /// Scenario: A backend implements all required trait methods and can be used as
    /// `dyn NetworkBackend`.
    #[test]
    fn test_mock_backend_compiles_as_dyn_network_backend() {
        let mock = make_mock(&["ethernet"], vec![]);
        let backend: Arc<dyn NetworkBackend> = Arc::new(mock);
        assert_eq!(backend.supported_entities(), &["ethernet".to_string()]);
    }

    /// Scenario: Backend query returns a StateSet for a supported entity type.
    #[tokio::test]
    async fn test_query_returns_state_set_for_supported_type() {
        let mock = make_mock(
            &["ethernet"],
            vec![
                make_state("ethernet", "eth0", vec![]),
                make_state("ethernet", "eth1", vec![]),
            ],
        );
        let result = mock.query(&"ethernet".to_string(), None).await.unwrap();
        assert_eq!(result.len(), 2);
    }

    /// Scenario: Backend query with selector filters results to matching entities.
    #[tokio::test]
    async fn test_query_with_selector_filters_results() {
        let mock = make_mock(
            &["ethernet"],
            vec![
                make_state("ethernet", "eth0", vec![]),
                make_state("ethernet", "eth1", vec![]),
            ],
        );
        let sel = Selector::with_name("eth0");
        let result = mock.query(&"ethernet".to_string(), Some(&sel)).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(
            result.get("ethernet", &Selector::with_name("eth0").key()).is_some(),
            "eth0 must be in the result"
        );
    }

    /// Scenario: Backend query for unsupported entity type returns UnsupportedEntityType.
    #[tokio::test]
    async fn test_query_unsupported_type_returns_error() {
        let mock = make_mock(&["ethernet"], vec![]);
        let result = mock.query(&"bond".to_string(), None).await;
        assert!(
            matches!(result, Err(BackendError::UnsupportedEntityType(ref t)) if t == "bond"),
            "expected UnsupportedEntityType(\"bond\"); got: {result:?}"
        );
    }

    /// Scenario: Backend apply with one failing selector produces a partial report.
    #[tokio::test]
    async fn test_apply_partial_success() {
        let mut mock = make_mock(&["ethernet"], vec![]);
        mock.fail_selectors.insert(Selector::with_name("eth0").key());

        let diff = StateDiff::new(vec![
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                fields: IndexMap::new(),
            },
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth1"),
                fields: IndexMap::new(),
            },
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth2"),
                fields: IndexMap::new(),
            },
        ]);

        let report = mock.apply(&diff).await.unwrap();
        assert_eq!(report.succeeded.len(), 2);
        assert_eq!(report.failed.len(), 1);
        assert!(report.is_partial(), "is_partial must be true when some succeed and some fail");
        assert!(!report.is_success(), "is_success must be false when there are failures");
    }

    /// Scenario: dry_run returns PlannedChange with current and desired field values.
    #[tokio::test]
    async fn test_dry_run_returns_field_changes_with_current_and_desired() {
        let mock = make_mock(
            &["ethernet"],
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let mut changed_fields = IndexMap::new();
        changed_fields.insert(
            "mtu".to_string(),
            FieldValue { value: Value::U64(9000), provenance: Provenance::KernelDefault },
        );
        let diff = StateDiff::new(vec![DiffOp::Modify {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            changed_fields,
            removed_fields: vec![],
        }]);

        let report = mock.dry_run(&diff).await.unwrap();
        assert_eq!(report.changes.len(), 1);
        let change = &report.changes[0];
        assert_eq!(change.field_changes.len(), 1);
        let fc = &change.field_changes[0];
        assert_eq!(fc.field, "mtu");
        assert_eq!(fc.current, Some(Value::U64(1500)));
        assert_eq!(fc.desired, Some(Value::U64(9000)));
    }

    /// Scenario: dry_run on an empty diff returns an empty DryRunReport.
    #[tokio::test]
    async fn test_dry_run_empty_diff_returns_empty_report() {
        let mock = make_mock(&["ethernet"], vec![]);
        let diff = StateDiff::new(vec![]);
        let report = mock.dry_run(&diff).await.unwrap();
        assert!(report.is_empty(), "report must be empty for an empty diff");
    }

    /// Scenario: ApplyReport with skipped operations.
    /// Given a StateDiff with 3 operations, when apply produces 1 succeeded, 1 failed,
    /// and 1 skipped, the report contains correct counts and each skipped entry has a
    /// non-empty reason string.
    #[tokio::test]
    async fn test_apply_with_skipped_operations() {
        let mut mock = make_mock(&["ethernet"], vec![]);
        mock.fail_selectors.insert(Selector::with_name("eth1").key());
        mock.skip_selectors.insert(Selector::with_name("eth2").key());

        let diff = StateDiff::new(vec![
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                fields: IndexMap::new(),
            },
            DiffOp::Modify {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth1"),
                changed_fields: IndexMap::new(),
                removed_fields: vec![],
            },
            DiffOp::Remove {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth2"),
            },
        ]);

        let report = mock.apply(&diff).await.unwrap();
        assert_eq!(report.succeeded.len(), 1, "exactly one operation must succeed");
        assert_eq!(report.failed.len(), 1, "exactly one operation must fail");
        assert_eq!(report.skipped.len(), 1, "exactly one operation must be skipped");

        let skipped = &report.skipped[0];
        assert!(
            !skipped.reason.is_empty(),
            "skipped entry must have a non-empty reason; got: {:?}",
            skipped.reason
        );

        assert!(report.is_partial(), "is_partial must be true with 1 succeeded and 1 failed");
        assert!(!report.is_success(), "is_success must be false when there are failures");
        assert!(!report.is_total_failure(), "is_total_failure must be false when some succeeded");
    }

    /// Scenario: dry_run lists planned changes for add, modify, and remove operations.
    /// Verifies that each op kind produces a PlannedChange with the correct DiffOpKind.
    #[tokio::test]
    async fn test_dry_run_reports_all_op_kinds() {
        let mock = make_mock(
            &["ethernet"],
            vec![make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))])],
        );

        let mut changed_fields = IndexMap::new();
        changed_fields.insert(
            "mtu".to_string(),
            FieldValue { value: Value::U64(9000), provenance: Provenance::KernelDefault },
        );

        let diff = StateDiff::new(vec![
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                fields: IndexMap::new(),
            },
            DiffOp::Modify {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth1"),
                changed_fields,
                removed_fields: vec![],
            },
            DiffOp::Remove {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth2"),
            },
        ]);

        let report = mock.dry_run(&diff).await.unwrap();
        assert_eq!(report.changes.len(), 3, "dry_run must plan one change per op");
        assert!(!report.is_empty(), "report must not be empty when there are changes");

        let kinds: Vec<DiffOpKind> = report.changes.iter().map(|c| c.operation).collect();
        assert!(kinds.contains(&DiffOpKind::Add), "must include an Add planned change");
        assert!(kinds.contains(&DiffOpKind::Modify), "must include a Modify planned change");
        assert!(kinds.contains(&DiffOpKind::Remove), "must include a Remove planned change");
    }
}
