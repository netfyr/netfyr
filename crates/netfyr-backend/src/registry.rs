//! `BackendRegistry`: maps entity types to `NetworkBackend` implementations and
//! dispatches operations to the correct backend.

use std::collections::HashMap;
use std::sync::Arc;

use netfyr_state::{union, EntityType, Selector, StateDiff, StateSet};

use crate::{ApplyReport, BackendError, DiffOpKind, FailedOperation, NetworkBackend};

// ── BackendRegistry ───────────────────────────────────────────────────────────

/// Routes entity-type-specific operations to the correct `NetworkBackend`.
///
/// Internally stores a `HashMap<EntityType, Arc<dyn NetworkBackend>>` so that
/// look-up is O(1). A backend that handles N entity types is stored under N keys,
/// all sharing the same `Arc` allocation.
#[derive(Default)]
pub struct BackendRegistry {
    backends: HashMap<EntityType, Arc<dyn NetworkBackend>>,
}

impl BackendRegistry {
    /// Returns a new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a backend for all of its `supported_entities()`.
    ///
    /// The check is all-or-nothing: if *any* entity type is already registered to
    /// a *different* backend, the call returns an error and the registry is left
    /// unchanged.
    ///
    /// Registering the same `Arc` twice for the same entity type is a no-op (the
    /// arc's allocation address is used for identity comparison).
    pub fn register(&mut self, backend: Arc<dyn NetworkBackend>) -> Result<(), BackendError> {
        let new_ptr = Arc::as_ptr(&backend) as *const ();
        let entity_types = backend.supported_entities();

        // Check for conflicts before mutating.
        for entity_type in entity_types {
            if let Some(existing) = self.backends.get(entity_type) {
                let existing_ptr = Arc::as_ptr(existing) as *const ();
                if existing_ptr != new_ptr {
                    return Err(BackendError::Internal(format!(
                        "entity type '{entity_type}' is already registered to a different backend",
                    )));
                }
            }
        }

        // No conflicts — insert all.
        for entity_type in entity_types {
            self.backends
                .insert(entity_type.clone(), Arc::clone(&backend));
        }

        Ok(())
    }

    /// Look up the backend registered for `entity_type`, if any.
    pub fn get(&self, entity_type: &EntityType) -> Option<Arc<dyn NetworkBackend>> {
        self.backends.get(entity_type).map(Arc::clone)
    }

    /// Returns all registered entity types in unspecified order.
    pub fn supported_entities(&self) -> Vec<EntityType> {
        self.backends.keys().cloned().collect()
    }

    /// Query entities of a specific type via the registered backend.
    ///
    /// Returns `BackendError::UnsupportedEntityType` if no backend is registered
    /// for the given entity type.
    pub async fn query(
        &self,
        entity_type: &EntityType,
        selector: Option<&Selector>,
    ) -> Result<StateSet, BackendError> {
        let backend = self
            .backends
            .get(entity_type)
            .ok_or_else(|| BackendError::UnsupportedEntityType(entity_type.clone()))?;
        backend.query(entity_type, selector).await
    }

    /// Query all registered backends and merge their results into one `StateSet`.
    ///
    /// Each unique backend (deduplicated by `Arc` allocation address) is queried
    /// once. Results are merged with `netfyr_state::union`. A `ConflictError` from
    /// `union` is converted to `BackendError::Internal` — this should not happen if
    /// backends cover disjoint entity types, but is handled gracefully.
    pub async fn query_all(&self) -> Result<StateSet, BackendError> {
        let unique_backends = self.unique_backends();
        let mut merged = StateSet::new();
        for backend in unique_backends {
            let result = backend.query_all().await?;
            merged = union(&merged, &result)
                .map_err(|e| BackendError::Internal(format!("conflict merging state: {e}")))?;
        }
        Ok(merged)
    }

    /// Apply a `StateDiff` across all registered backends.
    ///
    /// The diff is partitioned by entity type. Known entity types are dispatched to
    /// their backend; unknown ones produce `FailedOperation` entries. All results are
    /// merged into a single `ApplyReport`. This method always returns `Ok` — every
    /// failure is captured in the report.
    pub async fn apply(&self, diff: &StateDiff) -> Result<ApplyReport, BackendError> {
        // Partition ops by entity type.
        let mut partitioned: HashMap<String, Vec<_>> = HashMap::new();
        for op in diff.ops() {
            partitioned
                .entry(op.entity_type().to_string())
                .or_default()
                .push(op.clone());
        }

        let mut merged = ApplyReport::new();

        for (entity_type, ops) in partitioned {
            match self.backends.get(&entity_type) {
                Some(backend) => {
                    let sub_diff = StateDiff::new(ops);
                    match backend.apply(&sub_diff).await {
                        Ok(report) => merged.merge(report),
                        Err(e) => {
                            // Systemic backend failure: record every op in this batch as failed.
                            let msg = e.to_string();
                            for op in sub_diff.ops() {
                                merged.failed.push(FailedOperation {
                                    operation: DiffOpKind::from(op),
                                    entity_type: op.entity_type().to_string(),
                                    selector: op.selector().clone(),
                                    error: BackendError::Internal(msg.clone()),
                                    fields: vec![],
                                });
                            }
                        }
                    }
                }
                None => {
                    // No backend registered for this entity type.
                    for op in &ops {
                        merged.failed.push(FailedOperation {
                            operation: DiffOpKind::from(op),
                            entity_type: op.entity_type().to_string(),
                            selector: op.selector().clone(),
                            error: BackendError::UnsupportedEntityType(entity_type.clone()),
                            fields: vec![],
                        });
                    }
                }
            }
        }

        Ok(merged)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Collects one `Arc` per unique backend (deduplicated by allocation address).
    fn unique_backends(&self) -> Vec<Arc<dyn NetworkBackend>> {
        let mut seen: Vec<*const ()> = Vec::new();
        let mut unique: Vec<Arc<dyn NetworkBackend>> = Vec::new();
        for arc in self.backends.values() {
            // Cast to thin pointer (data address only) to strip the vtable component
            // of the fat pointer. Two Arcs cloned from the same source share the same
            // allocation address regardless of vtable.
            let ptr = Arc::as_ptr(arc) as *const ();
            if !seen.contains(&ptr) {
                seen.push(ptr);
                unique.push(Arc::clone(arc));
            }
        }
        unique
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use netfyr_state::{EntityType, Selector, State, StateDiff, StateMetadata, StateSet};
    use netfyr_state::diff::DiffOp;
    use indexmap::IndexMap;

    use crate::{AppliedOperation, ApplyReport, BackendError, DiffOpKind, DryRunReport, NetworkBackend};

    use super::BackendRegistry;

    struct MockBackend {
        supported: Vec<EntityType>,
        state: StateSet,
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
                report.succeeded.push(AppliedOperation {
                    operation: DiffOpKind::from(op),
                    entity_type: op.entity_type().to_string(),
                    selector: op.selector().clone(),
                    fields_changed: vec![],
                });
            }
            Ok(report)
        }

        async fn dry_run(&self, _diff: &StateDiff) -> Result<DryRunReport, BackendError> {
            Ok(DryRunReport::new())
        }
    }

    fn make_state(entity_type: &str, name: &str) -> State {
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: IndexMap::new(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        }
    }

    fn make_mock(supported: &[&str], states: Vec<State>) -> Arc<dyn NetworkBackend> {
        let mut state_set = StateSet::new();
        for s in states {
            state_set.insert(s);
        }
        Arc::new(MockBackend {
            supported: supported.iter().map(|s| s.to_string()).collect(),
            state: state_set,
        })
    }

    /// Scenario: Register a backend and look it up by entity type.
    #[test]
    fn test_register_and_get_by_entity_type() {
        let mut registry = BackendRegistry::new();
        let mock = make_mock(&["ethernet", "bond"], vec![]);
        registry.register(mock).unwrap();

        assert!(registry.get(&"ethernet".to_string()).is_some());
        assert!(registry.get(&"bond".to_string()).is_some());
        assert!(registry.get(&"vlan".to_string()).is_none());
    }

    /// Scenario: Register two backends for different entity types; each lookup returns
    /// the correct backend.
    #[test]
    fn test_register_two_backends_disjoint_types() {
        let mut registry = BackendRegistry::new();
        let mock_a = make_mock(&["ethernet"], vec![]);
        let mock_b = make_mock(&["firewall-rule"], vec![]);
        registry.register(mock_a).unwrap();
        registry.register(mock_b).unwrap();

        let backend_eth = registry.get(&"ethernet".to_string()).unwrap();
        assert!(
            backend_eth.supported_entities().contains(&"ethernet".to_string()),
            "get(ethernet) must return the ethernet backend"
        );
        assert!(
            !backend_eth.supported_entities().contains(&"firewall-rule".to_string()),
            "get(ethernet) must not return the firewall backend"
        );

        let backend_fw = registry.get(&"firewall-rule".to_string()).unwrap();
        assert!(
            backend_fw.supported_entities().contains(&"firewall-rule".to_string()),
            "get(firewall-rule) must return the firewall backend"
        );
    }

    /// Scenario: Registering a conflicting entity type fails, and the original
    /// registration is preserved.
    #[test]
    fn test_register_conflicting_entity_type_fails() {
        let mut registry = BackendRegistry::new();
        let mock_a = make_mock(&["ethernet"], vec![]);
        let mock_b = make_mock(&["ethernet"], vec![]);

        registry.register(Arc::clone(&mock_a)).unwrap();
        let result = registry.register(mock_b);
        assert!(result.is_err(), "registering a conflicting entity type must fail");

        // Original registration is preserved.
        let backend = registry.get(&"ethernet".to_string()).unwrap();
        assert!(
            backend.supported_entities().contains(&"ethernet".to_string()),
            "original ethernet backend must still be registered"
        );
    }

    /// Scenario: supported_entities returns all registered entity types.
    #[test]
    fn test_supported_entities_returns_all_types() {
        let mut registry = BackendRegistry::new();
        registry.register(make_mock(&["ethernet", "bond"], vec![])).unwrap();
        registry.register(make_mock(&["vlan", "firewall-rule"], vec![])).unwrap();

        let types = registry.supported_entities();
        assert_eq!(types.len(), 4);
        assert!(types.contains(&"ethernet".to_string()));
        assert!(types.contains(&"bond".to_string()));
        assert!(types.contains(&"vlan".to_string()));
        assert!(types.contains(&"firewall-rule".to_string()));
    }

    /// Scenario: Registry query_all queries all registered backends and merges results.
    #[tokio::test]
    async fn test_query_all_merges_results() {
        let mut registry = BackendRegistry::new();
        let mock_a = make_mock(&["ethernet"], vec![make_state("ethernet", "eth0")]);
        let mock_b = make_mock(&["firewall-rule"], vec![make_state("firewall-rule", "fw0")]);
        registry.register(mock_a).unwrap();
        registry.register(mock_b).unwrap();

        let result = registry.query_all().await.unwrap();
        assert_eq!(result.len(), 2, "query_all must merge results from both backends");
    }

    /// Scenario: Registry apply dispatches by entity type; unknown types produce
    /// FailedOperation with UnsupportedEntityType.
    #[tokio::test]
    async fn test_apply_dispatches_by_entity_type() {
        let mut registry = BackendRegistry::new();
        registry.register(make_mock(&["ethernet"], vec![])).unwrap();

        let diff = StateDiff::new(vec![
            DiffOp::Add {
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                fields: IndexMap::new(),
            },
            DiffOp::Add {
                entity_type: "wifi".to_string(),
                selector: Selector::with_name("wlan0"),
                fields: IndexMap::new(),
            },
        ]);

        let report = registry.apply(&diff).await.unwrap();
        assert_eq!(report.succeeded.len(), 1, "ethernet op must succeed");
        assert_eq!(report.failed.len(), 1, "wifi op must fail (unregistered)");
        assert!(
            matches!(&report.failed[0].error, BackendError::UnsupportedEntityType(t) if t == "wifi"),
            "failed op must have UnsupportedEntityType(wifi)"
        );
    }

    /// Scenario: Registering the same Arc twice is a no-op (no error, no duplicate).
    #[test]
    fn test_register_same_arc_twice_is_noop() {
        let mut registry = BackendRegistry::new();
        let mock = make_mock(&["ethernet"], vec![]);
        registry.register(Arc::clone(&mock)).unwrap();
        let result = registry.register(Arc::clone(&mock));
        assert!(result.is_ok(), "registering the same Arc twice must not error");
        assert_eq!(registry.supported_entities().len(), 1);
    }

    /// Scenario: Registry apply partitions diff by entity type across two backends.
    /// Ethernet ops go to the ethernet backend, firewall-rule ops go to the
    /// firewall backend, and both reports are merged into the single returned report.
    #[tokio::test]
    async fn test_apply_partitions_diff_between_two_backends() {
        let mut registry = BackendRegistry::new();
        registry.register(make_mock(&["ethernet"], vec![])).unwrap();
        registry.register(make_mock(&["firewall-rule"], vec![])).unwrap();

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
                entity_type: "firewall-rule".to_string(),
                selector: Selector::with_name("allow-http"),
                fields: IndexMap::new(),
            },
        ]);

        let report = registry.apply(&diff).await.unwrap();

        assert_eq!(
            report.succeeded.len(),
            3,
            "all three ops must succeed when both backends are registered"
        );
        assert!(report.failed.is_empty(), "no ops must fail with both backends registered");
        assert!(report.is_success(), "report must indicate full success");

        // Verify that both entity types appear in the succeeded entries.
        let succeeded_types: Vec<&str> =
            report.succeeded.iter().map(|op| op.entity_type.as_str()).collect();
        assert!(
            succeeded_types.contains(&"ethernet"),
            "ethernet operations must appear in succeeded"
        );
        assert!(
            succeeded_types.contains(&"firewall-rule"),
            "firewall-rule operations must appear in succeeded"
        );
    }

    /// Scenario: Registry query delegates to the registered backend and returns its StateSet.
    #[tokio::test]
    async fn test_registry_query_delegates_to_backend() {
        let mut registry = BackendRegistry::new();
        registry
            .register(make_mock(&["ethernet"], vec![make_state("ethernet", "eth0")]))
            .unwrap();

        let result = registry
            .query(&"ethernet".to_string(), None)
            .await
            .unwrap();
        assert_eq!(result.len(), 1, "registry query must return states from the registered backend");
    }

    /// Scenario: Registry query for an unregistered entity type returns UnsupportedEntityType.
    #[tokio::test]
    async fn test_registry_query_unregistered_type_returns_error() {
        let registry = BackendRegistry::new();
        let result = registry.query(&"ethernet".to_string(), None).await;
        assert!(
            matches!(result, Err(BackendError::UnsupportedEntityType(ref t)) if t == "ethernet"),
            "expected UnsupportedEntityType(ethernet); got: {result:?}"
        );
    }
}
