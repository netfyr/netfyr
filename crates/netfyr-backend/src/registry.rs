use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use futures_util::future::join_all;
use netfyr_state::Match;
use netfyr_state::plan::{NamespaceId, Observation, Operation, StateDiff};

use crate::{BackendError, BackendErrorKind, NetworkBackend, OperationContext};

/// Routes entity types within each namespace to their owning backends.
/// Dispatch whole plans to preserve dependencies and address preconditions.
#[derive(Default)]
pub struct BackendRegistry {
    backends: Vec<Arc<dyn NetworkBackend>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a backend. Two backends cannot own the same entity type in one namespace.
    pub fn register(&mut self, backend: Arc<dyn NetworkBackend>) -> Result<(), BackendError> {
        let entities = backend.supported_entities();
        for registered in &self.backends {
            if registered.namespace() == backend.namespace()
                && registered
                    .supported_entities()
                    .iter()
                    .any(|entity| entities.contains(entity))
            {
                return Err(BackendError::new(
                    BackendErrorKind::Conflict,
                    "an entity type already has a backend in this namespace",
                ));
            }
        }
        self.backends.push(backend);
        Ok(())
    }

    /// Query every registered backend, keeping the type owner's view of each device.
    pub async fn query(&self, selector: &Match) -> Result<Observation, BackendError> {
        self.query_in_namespace(selector, None).await
    }

    pub async fn query_all(&self) -> Result<Observation, BackendError> {
        self.query(&Match::default()).await
    }

    /// Resolve an operation's backend from discovery; apply revalidates the target.
    pub async fn resolve(
        &self,
        index: usize,
        operation: &Operation,
    ) -> Result<Arc<dyn NetworkBackend>, BackendError> {
        let context = OperationContext::new(index, operation);
        let selector = Match {
            name: Some(operation.target.name.clone()),
            ..Match::default()
        };
        let observation = self
            .query_in_namespace(&selector, Some(operation.target.namespace))
            .await
            .map_err(|error| error.at(&context))?;
        self.select(&context, &observation)
    }

    /// Resolve a whole plan; reject operations that span backends.
    pub async fn resolve_plan(
        &self,
        diff: &StateDiff,
    ) -> Result<Arc<dyn NetworkBackend>, BackendError> {
        let mut observations: HashMap<NamespaceId, Observation> = HashMap::new();
        let mut resolved: Option<Arc<dyn NetworkBackend>> = None;
        for (index, operation) in diff.operations.iter().enumerate() {
            let context = OperationContext::new(index, operation);
            let observation = match observations.entry(operation.target.namespace) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(
                    self.query_in_namespace(&Match::default(), Some(operation.target.namespace))
                        .await
                        .map_err(|error| error.at(&context))?,
                ),
            };
            let backend = self.select(&context, observation)?;
            match &resolved {
                Some(first) if !Arc::ptr_eq(first, &backend) => {
                    return Err(BackendError::new(
                        BackendErrorKind::UnsupportedOperation,
                        "plan operations resolve to different backends; split it by backend",
                    )
                    .at(&context));
                }
                Some(_) => {}
                None => resolved = Some(backend),
            }
        }
        resolved
            .ok_or_else(|| BackendError::new(BackendErrorKind::NotFound, "plan has no operations"))
    }

    fn select(
        &self,
        context: &OperationContext,
        observation: &Observation,
    ) -> Result<Arc<dyn NetworkBackend>, BackendError> {
        let target = &context.target;
        let device = observation
            .devices
            .iter()
            .find(|device| {
                device.identity.namespace == target.namespace && device.identity.name == target.name
            })
            .ok_or_else(|| {
                BackendError::new(BackendErrorKind::NotFound, "target interface was not found")
                    .at(context)
            })?;
        let backend = self
            .backends
            .iter()
            .find(|backend| {
                backend.namespace() == device.identity.namespace
                    && backend
                        .supported_entities()
                        .contains(&device.identity.device_type.as_str())
            })
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorKind::UnsupportedEntityType,
                    format!("no backend supports {}", device.identity.device_type),
                )
                .at(context)
            })?;
        if device.identity != *target {
            return Err(BackendError::new(
                BackendErrorKind::StaleState,
                "target identity differs from the discovered interface",
            )
            .at(context));
        }
        if !backend.supports_operation(&context.kind) {
            return Err(BackendError::new(
                BackendErrorKind::UnsupportedOperation,
                format!("backend does not support {}", context.kind.kind_name()),
            )
            .at(context));
        }
        Ok(Arc::clone(backend))
    }

    async fn query_in_namespace(
        &self,
        selector: &Match,
        namespace: Option<NamespaceId>,
    ) -> Result<Observation, BackendError> {
        let backends: Vec<_> = self
            .backends
            .iter()
            .filter(|backend| namespace.is_none_or(|namespace| namespace == backend.namespace()))
            .collect();
        let results = join_all(backends.iter().map(|backend| backend.query(selector))).await;
        let mut result = Observation::default();
        let mut seen = HashMap::new();
        for (backend, observation) in backends.into_iter().zip(results) {
            let observation = match observation {
                Ok(observation) => observation,
                Err(error) if error.kind == BackendErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            result.diagnostics.extend(observation.diagnostics);
            let entities = backend.supported_entities();
            for device in observation.devices {
                if device.identity.namespace != backend.namespace() {
                    return Err(BackendError::new(
                        BackendErrorKind::Internal,
                        "backend returned an interface from another namespace",
                    ));
                }
                let key = (device.identity.namespace, device.identity.index);
                let owned = entities.contains(&device.identity.device_type.as_str());
                match seen.get(&key).copied() {
                    Some((index, false)) if owned => {
                        result.devices[index] = device;
                        seen.insert(key, (index, true));
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(key, (result.devices.len(), owned));
                        result.devices.push(device);
                    }
                }
            }
        }
        if selector.name.is_some() && result.devices.is_empty() {
            return Err(BackendError::new(
                BackendErrorKind::NotFound,
                "named interface was not found in the registered namespaces",
            ));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use futures_util::FutureExt;
    use netfyr_state::plan::{
        DeviceObservation, Diagnostic, InterfaceId, NamespaceId, Observation, Operation,
        OperationKind, StateDiff,
    };
    use netfyr_state::{Match, Source, State};

    use super::BackendRegistry;
    use crate::{
        ApplyReport, BackendError, BackendErrorKind, BackendFuture, DryRunReport, NetworkBackend,
    };

    struct StubBackend {
        namespace: NamespaceId,
        entities: &'static [&'static str],
        devices: Vec<DeviceObservation>,
        failure: Option<BackendErrorKind>,
        diagnostics: Vec<Diagnostic>,
    }

    impl NetworkBackend for StubBackend {
        fn namespace(&self) -> NamespaceId {
            self.namespace
        }

        fn supported_entities(&self) -> &'static [&'static str] {
            self.entities
        }

        fn supports_operation(&self, kind: &OperationKind) -> bool {
            matches!(kind, OperationKind::SetMtu(_))
        }

        fn query<'a>(
            &'a self,
            selector: &'a Match,
        ) -> BackendFuture<'a, Result<Observation, BackendError>> {
            Box::pin(async move {
                if let Some(kind) = self.failure {
                    return Err(BackendError::new(kind, "stub failure"));
                }
                Ok(Observation {
                    devices: self
                        .devices
                        .iter()
                        .filter(|device| selector.matches(&device.state.match_spec))
                        .cloned()
                        .collect(),
                    diagnostics: self.diagnostics.clone(),
                })
            })
        }

        fn apply<'a>(&'a self, _: &'a StateDiff) -> BackendFuture<'a, ApplyReport> {
            Box::pin(async { panic!("resolution must not execute operations") })
        }

        fn dry_run<'a>(&'a self, _: &'a StateDiff) -> BackendFuture<'a, DryRunReport> {
            Box::pin(async { panic!("resolution must not preview a partial plan") })
        }
    }

    fn device(namespace_inode: u64, device_type: &str) -> DeviceObservation {
        let identity = InterfaceId {
            namespace: NamespaceId {
                device: 4,
                inode: namespace_inode,
            },
            index: 7,
            generation: 1,
            name: "eth0".into(),
            device_type: device_type.into(),
            mac: Some("02:00:00:00:00:01".into()),
        };
        let mut state = State::new(Source::Kernel);
        state.device_type = device_type.into();
        state.match_spec = Match {
            name: Some(identity.name.clone()),
            r#type: Some(device_type.into()),
            mac: identity.mac.clone(),
            ..Match::default()
        };
        DeviceObservation {
            identity,
            state,
            addresses: Vec::new(),
            addresses_complete: true,
            observed_at: Instant::now(),
        }
    }

    fn stub(device: DeviceObservation, entities: &'static [&'static str]) -> StubBackend {
        StubBackend {
            namespace: device.identity.namespace,
            entities,
            devices: vec![device],
            failure: None,
            diagnostics: Vec::new(),
        }
    }

    fn backend(device: DeviceObservation, entity: &'static str) -> Arc<dyn NetworkBackend> {
        let entities: &'static [&'static str] = match entity {
            "ethernet" => &["ethernet"],
            _ => &["test-only"],
        };
        Arc::new(stub(device, entities))
    }

    fn operation(device: &DeviceObservation) -> Operation {
        Operation {
            target: device.identity.clone(),
            kind: OperationKind::SetMtu(9000),
            depends_on: Vec::new(),
            address_group: None,
        }
    }

    #[test]
    fn overlapping_owners_are_rejected_only_within_one_namespace() {
        let mut registry = BackendRegistry::new();
        registry
            .register(backend(device(1, "ethernet"), "ethernet"))
            .unwrap();
        assert_eq!(
            registry
                .register(backend(device(1, "ethernet"), "ethernet"))
                .unwrap_err()
                .kind,
            BackendErrorKind::Conflict
        );
        assert_eq!(
            registry
                .register(Arc::new(stub(
                    device(1, "ethernet"),
                    &["bridge", "ethernet"]
                )))
                .unwrap_err()
                .kind,
            BackendErrorKind::Conflict
        );
        registry
            .register(Arc::new(stub(device(1, "ethernet"), &["bridge", "bond"])))
            .unwrap();
        registry
            .register(backend(device(2, "ethernet"), "ethernet"))
            .unwrap();
    }

    #[test]
    fn resolution_selects_namespace_and_retains_original_error_context() {
        let first = device(1, "ethernet");
        let second = device(2, "ethernet");
        let expected = backend(second.clone(), "ethernet");
        let mut registry = BackendRegistry::new();
        registry.register(backend(first, "ethernet")).unwrap();
        registry.register(expected.clone()).unwrap();
        let mut op = operation(&second);
        let actual = registry.resolve(12, &op).now_or_never().unwrap().unwrap();
        assert!(Arc::ptr_eq(&actual, &expected));

        op.kind = OperationKind::AddEntity;
        let error = registry
            .resolve(12, &op)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::UnsupportedOperation);
        let context = error.context.unwrap();
        assert_eq!(context.index, 12);
        assert_eq!(context.target, second.identity);
        assert_eq!(context.kind, OperationKind::AddEntity);
    }

    #[test]
    fn discovered_unsupported_kind_cannot_be_spoofed_as_ethernet() {
        let observed = device(1, "bridge");
        let mut op = operation(&observed);
        op.target.device_type = "ethernet".into();
        let mut registry = BackendRegistry::new();
        registry.register(backend(observed, "ethernet")).unwrap();
        let error = registry
            .resolve(0, &op)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::UnsupportedEntityType);
    }

    #[test]
    fn reused_interface_name_requires_a_fresh_plan() {
        let observed = device(1, "ethernet");
        let mut op = operation(&observed);
        op.target.generation = 0;
        let mut registry = BackendRegistry::new();
        registry.register(backend(observed, "ethernet")).unwrap();
        let error = registry
            .resolve(4, &op)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::StaleState);
        assert_eq!(error.context.unwrap().index, 4);
    }

    #[test]
    fn inventory_deduplicates_within_namespace_and_prefers_the_type_owner() {
        let observed = device(1, "ethernet");
        let mut owner_observation = observed.clone();
        owner_observation.identity.generation = 2;
        let mut registry = BackendRegistry::new();
        registry.register(backend(observed, "test-only")).unwrap();
        registry
            .register(backend(owner_observation, "ethernet"))
            .unwrap();
        registry
            .register(backend(device(2, "ethernet"), "ethernet"))
            .unwrap();
        let result = registry.query_all().now_or_never().unwrap().unwrap();
        assert_eq!(result.devices.len(), 2);
        assert_eq!(result.devices[0].identity.generation, 2);
        assert_ne!(
            result.devices[0].identity.namespace,
            result.devices[1].identity.namespace
        );
    }

    #[test]
    fn absent_namespace_and_absent_named_interface_are_not_found() {
        let mut registry = BackendRegistry::new();
        registry
            .register(backend(device(1, "ethernet"), "ethernet"))
            .unwrap();
        let op = operation(&device(2, "ethernet"));
        let error = registry
            .resolve(0, &op)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::NotFound);
        let selector = Match {
            name: Some("missing".into()),
            ..Match::default()
        };
        let error = registry
            .query(&selector)
            .now_or_never()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, BackendErrorKind::NotFound);
        let selector = Match {
            r#type: Some("missing".into()),
            ..Match::default()
        };
        assert!(
            registry
                .query(&selector)
                .now_or_never()
                .unwrap()
                .unwrap()
                .devices
                .is_empty()
        );
    }

    #[test]
    fn query_skips_not_found_backends_and_propagates_other_failures() {
        let mut missing = stub(device(1, "ethernet"), &["ethernet"]);
        missing.failure = Some(BackendErrorKind::NotFound);
        let mut registry = BackendRegistry::new();
        registry.register(Arc::new(missing)).unwrap();
        registry
            .register(backend(device(2, "ethernet"), "ethernet"))
            .unwrap();
        let result = registry.query_all().now_or_never().unwrap().unwrap();
        assert_eq!(result.devices.len(), 1);
        assert_eq!(result.devices[0].identity.namespace.inode, 2);

        let mut broken = stub(device(3, "ethernet"), &["ethernet"]);
        broken.failure = Some(BackendErrorKind::QueryFailed);
        registry.register(Arc::new(broken)).unwrap();
        let error = registry.query_all().now_or_never().unwrap().unwrap_err();
        assert_eq!(error.kind, BackendErrorKind::QueryFailed);
        let error = registry
            .resolve(5, &operation(&device(3, "ethernet")))
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::QueryFailed);
        assert_eq!(error.context.unwrap().index, 5);
    }

    #[test]
    fn a_device_from_another_namespace_is_an_internal_error() {
        let mut misplaced = stub(device(1, "ethernet"), &["ethernet"]);
        misplaced.devices[0].identity.namespace.inode = 2;
        let mut registry = BackendRegistry::new();
        registry.register(Arc::new(misplaced)).unwrap();
        let error = registry.query_all().now_or_never().unwrap().unwrap_err();
        assert_eq!(error.kind, BackendErrorKind::Internal);
    }

    #[test]
    fn diagnostics_from_every_backend_are_kept() {
        let issue = |message: &str| Diagnostic {
            target: None,
            field: None,
            message: message.into(),
        };
        let mut first = stub(device(1, "ethernet"), &["ethernet"]);
        first.diagnostics = vec![issue("first")];
        let mut second = stub(device(2, "ethernet"), &["ethernet"]);
        second.diagnostics = vec![issue("second")];
        let mut registry = BackendRegistry::new();
        registry.register(Arc::new(first)).unwrap();
        registry.register(Arc::new(second)).unwrap();
        let result = registry.query_all().now_or_never().unwrap().unwrap();
        assert_eq!(result.diagnostics, [issue("first"), issue("second")]);
    }

    #[test]
    fn a_plan_resolves_to_one_backend_or_is_refused() {
        let first = device(1, "ethernet");
        let second = device(2, "ethernet");
        let expected = backend(first.clone(), "ethernet");
        let mut registry = BackendRegistry::new();
        registry.register(expected.clone()).unwrap();
        registry
            .register(backend(second.clone(), "ethernet"))
            .unwrap();
        let mut diff = StateDiff {
            operations: vec![operation(&first), operation(&first)],
            ..StateDiff::default()
        };
        let resolved = registry
            .resolve_plan(&diff)
            .now_or_never()
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&resolved, &expected));

        diff.operations.push(operation(&second));
        let error = registry
            .resolve_plan(&diff)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::UnsupportedOperation);
        assert_eq!(error.context.unwrap().index, 2);

        let error = registry
            .resolve_plan(&StateDiff::default())
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.kind, BackendErrorKind::NotFound);
    }
}
