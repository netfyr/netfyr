//! Policy loading, validation, and static-state production.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use netfyr_state::{
    ApplyOptions, Error as StateError, Match, SchemaRegistry, Source, State, ValidationError,
    Value, Warning, prepare_for_apply_with_registry,
};

const DHCP4_SCHEMA: &str = r#"{
  "type": "object", "x-netfyr-writable": true, "additionalProperties": false,
  "properties": {
    "send-hostname": { "type": "boolean", "x-netfyr-writable": true },
    "route-metric": { "type": "integer", "x-netfyr-writable": true, "minimum": 0 }
  }
}"#;
const OPEN_PARAMS_SCHEMA: &str = r#"{
  "type": "object", "x-netfyr-writable": true, "additionalProperties": true
}"#;

/// The source family a provider uses when it later produces state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSource {
    /// Dynamic Configuration Protocol provider.
    Dhcp,
    /// IPv6 router advertisement provider.
    Ra,
}

impl ProviderSource {
    fn default_priority(self) -> i32 {
        match self {
            Self::Dhcp => Source::Dhcp {
                policy: String::new(),
            }
            .default_priority(),
            Self::Ra => Source::Ra {
                policy: String::new(),
            }
            .default_priority(),
        }
    }
}

/// One fixed provider registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRegistration {
    /// Stable provider kind.
    pub kind: &'static str,
    /// The enclosing state domain, such as `ipv4`.
    pub domain: &'static str,
    /// The nested provider trigger key.
    pub key: &'static str,
    /// Which source family later executes the provider.
    pub source: ProviderSource,
    /// The provider parameter schema in netfyr's schema dialect.
    pub params_schema: &'static str,
}

impl ProviderRegistration {
    /// The trigger's dotted location.
    pub fn location(self) -> String {
        format!("{}.{}", self.domain, self.key)
    }

    /// The source family's fixed default priority.
    pub fn default_priority(self) -> i32 {
        self.source.default_priority()
    }
}

const PROVIDERS: [ProviderRegistration; 3] = [
    ProviderRegistration {
        kind: "dhcp4",
        domain: "ipv4",
        key: "dhcp4",
        source: ProviderSource::Dhcp,
        params_schema: DHCP4_SCHEMA,
    },
    ProviderRegistration {
        kind: "dhcp6",
        domain: "ipv6",
        key: "dhcp6",
        source: ProviderSource::Dhcp,
        params_schema: OPEN_PARAMS_SCHEMA,
    },
    ProviderRegistration {
        kind: "ra",
        domain: "ipv6",
        key: "ra",
        source: ProviderSource::Ra,
        params_schema: OPEN_PARAMS_SCHEMA,
    },
];

/// The compile-time provider inventory and its schema composition helpers.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProviderRegistry;

impl ProviderRegistry {
    /// The registry's registrations, in deterministic registry order.
    pub fn registrations(self) -> &'static [ProviderRegistration] {
        &PROVIDERS
    }

    /// Build the state registry extended with all provider trigger schemas.
    pub fn schema_registry(self) -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        for registration in self.registrations() {
            let (fragment, path) = match registration.domain {
                "ipv4" => ("ipv4", registration.key.to_string()),
                domain => ("base", format!("{domain}.{}", registration.key)),
            };
            schemas
                .add_schema_node(fragment, &path, registration.params_schema)
                .expect("the compile-time provider registry has valid non-conflicting schemas");
        }
        schemas
    }

    fn registration(self, domain: &str, key: &str) -> Option<ProviderRegistration> {
        self.registrations()
            .iter()
            .copied()
            .find(|registration| registration.domain == domain && registration.key == key)
    }
}

/// A named unit of desired state and dynamic provider declarations.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Unique policy identifier.
    pub name: String,
    /// Target device selector.
    pub match_spec: Match,
    /// Desired device technology, if explicitly given or implied.
    pub device_type: String,
    /// Static fields and nested provider triggers, in input order.
    pub fields: IndexMap<String, Value>,
    /// Optional static contribution priority override.
    pub priority: Option<i32>,
    /// Uninterpreted external annotations retained only on the policy.
    pub metadata: Option<IndexMap<String, Value>>,
}

/// A validated declaration of a provider to be run by a later framework.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDecl {
    /// Provider kind from the registry.
    pub kind: String,
    /// Provider source family from the registry.
    pub source: ProviderSource,
    /// Declaring policy name.
    pub policy: String,
    /// Dotted trigger location.
    pub location: String,
    /// Fixed provider default priority.
    pub priority: i32,
    /// Schema-decoded parameters in input order.
    pub params: IndexMap<String, Value>,
    /// Copied device selector.
    pub match_spec: Match,
    /// Copied device technology.
    pub device_type: String,
}

/// The output of policy production.
#[derive(Clone, Debug, Default)]
pub struct Production {
    /// Static state contribution, absent for provider-only policies.
    pub state: Option<State>,
    /// Dynamic provider declarations in field order.
    pub providers: Vec<ProviderDecl>,
}

/// Aggregate output from [`PolicySet::produce_all`].
#[derive(Clone, Debug, Default)]
pub struct ProduceAllOutcome {
    /// Static contributions in policy insertion order.
    pub states: Vec<State>,
    /// Provider declarations in policy and field order.
    pub providers: Vec<ProviderDecl>,
}

/// An insertion-ordered policy collection.
#[derive(Clone, Debug, Default)]
pub struct PolicySet {
    policies: IndexMap<String, Policy>,
}

impl PolicySet {
    /// Insert a policy without replacing an existing name.
    pub fn insert(&mut self, policy: Policy) -> Result<(), PolicyError> {
        if policy.name.is_empty() {
            return Err(PolicyError::EmptyName);
        }
        if self.policies.contains_key(&policy.name) {
            return Err(PolicyError::DuplicateName(policy.name));
        }
        self.policies.insert(policy.name.clone(), policy);
        Ok(())
    }

    /// Produce every policy in insertion order, atomically.
    pub fn produce_all(&self) -> Result<ProduceAllOutcome, PolicyError> {
        let mut output = ProduceAllOutcome::default();
        for policy in self.policies.values() {
            let produced = policy.produce()?;
            if let Some(state) = produced.state {
                output.states.push(state);
            }
            output.providers.extend(produced.providers);
        }
        Ok(output)
    }

    /// Policies in insertion order.
    pub fn policies(&self) -> impl Iterator<Item = &Policy> {
        self.policies.values()
    }
}

impl Policy {
    /// Validate and split this policy into static state and provider declarations.
    pub fn produce(&self) -> Result<Production, PolicyError> {
        if self.name.is_empty() {
            return Err(PolicyError::EmptyName);
        }
        if self.match_spec.is_empty() {
            return Err(PolicyError::MissingMatch);
        }
        let registry = ProviderRegistry;
        let schemas = registry.schema_registry();
        let candidate = self.candidate_state();
        let errors = schemas.validate_writable(&candidate);
        if !errors.is_empty() {
            return Err(PolicyError::Validation(errors));
        }

        let mut fields = self.fields.clone();
        let mut providers = Vec::new();
        let domains: Vec<String> = fields.keys().cloned().collect();
        for domain in domains {
            let Some(Value::Map(values)) = fields.get_mut(&domain) else {
                continue;
            };
            let keys: Vec<String> = values.keys().cloned().collect();
            let mut removed_any = false;
            for key in keys {
                let Some(registration) = registry.registration(&domain, &key) else {
                    continue;
                };
                let Value::Map(params) = values
                    .shift_remove(&key)
                    .expect("validated trigger remains in its enclosing map")
                else {
                    unreachable!("validated trigger is a parameter map");
                };
                removed_any = true;
                providers.push(ProviderDecl {
                    kind: registration.kind.to_string(),
                    source: registration.source,
                    policy: self.name.clone(),
                    location: registration.location(),
                    priority: registration.default_priority(),
                    params,
                    match_spec: self.match_spec.clone(),
                    device_type: self.device_type.clone(),
                });
            }
            if removed_any && values.is_empty() {
                fields.shift_remove(&domain);
            }
        }
        if fields.is_empty() && self.device_type.is_empty() && providers.is_empty() {
            return Err(PolicyError::EmptyPolicy);
        }

        let state = (!fields.is_empty() || !self.device_type.is_empty()).then(|| {
            let source = Source::Static {
                policy: self.name.clone(),
            };
            State {
                device_type: self.device_type.clone(),
                match_spec: self.match_spec.clone(),
                fields,
                priority: self.priority.unwrap_or_else(|| source.default_priority()),
                source,
                metadata: netfyr_state::StateMetadata::new(),
            }
        });
        Ok(Production { state, providers })
    }

    fn candidate_state(&self) -> State {
        let mut state = State::new(Source::Static {
            policy: self.name.clone(),
        });
        state.match_spec = self.match_spec.clone();
        state.device_type = self.device_type.clone();
        state.fields = self.fields.clone();
        state
    }
}

/// Successful file or directory loading result.
#[derive(Clone, Debug, Default)]
pub struct LoadOutcome {
    /// Policies in file and document order.
    pub policies: Vec<Policy>,
    /// Query-to-apply warnings in encounter order.
    pub warnings: Vec<Warning>,
}

/// Errors from policy parsing, validation, or loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// An I/O error while reading a policy path.
    Io(String),
    /// YAML parser error.
    Yaml(String),
    /// The document contains multiple YAML documents.
    MultiDocument,
    /// A document or sequence member was not a mapping.
    InvalidTopLevel(String),
    /// `kind` was not `policy`, `state`, or absent.
    UnknownKind(String),
    /// Explicit policy name was absent, empty, or not a string.
    EmptyName,
    /// A duplicate name occurred in one loader result or a policy set.
    DuplicateName(String),
    /// An explicit policy omitted a non-empty match.
    MissingMatch,
    /// `priority` was not an i32 integer.
    InvalidPriority,
    /// `metadata` was not a mapping.
    MetadataNotMapping,
    /// Candidate fields failed the composed writable schema.
    Validation(Vec<ValidationError>),
    /// A state-level decode or apply-preparation error.
    State(StateError),
    /// A policy had neither static content nor providers.
    EmptyPolicy,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "policy I/O error: {error}"),
            Self::Yaml(error) => write!(f, "policy YAML error: {error}"),
            Self::MultiDocument => write!(f, "policy input contains more than one YAML document"),
            Self::InvalidTopLevel(shape) => write!(
                f,
                "policy input must be a mapping or a list of mappings, found {shape}"
            ),
            Self::UnknownKind(kind) => write!(f, "unknown policy kind '{kind}'"),
            Self::EmptyName => write!(f, "policy name must be a non-empty string"),
            Self::DuplicateName(name) => write!(f, "duplicate policy name '{name}'"),
            Self::MissingMatch => write!(f, "policy requires a non-empty match"),
            Self::InvalidPriority => write!(f, "policy priority must be an i32 integer"),
            Self::MetadataNotMapping => write!(f, "policy metadata must be a mapping"),
            Self::Validation(errors) => {
                write!(f, "policy schema validation failed")?;
                for error in errors {
                    write!(f, "; {error}")?;
                }
                Ok(())
            }
            Self::State(error) => error.fmt(f),
            Self::EmptyPolicy => write!(f, "policy has no static fields or provider declarations"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// Load one YAML policy file.
pub fn load_policy_file(
    path: impl AsRef<Path>,
    options: ApplyOptions,
) -> Result<LoadOutcome, PolicyError> {
    let path = path.as_ref();
    let input = fs::read_to_string(path).map_err(|error| PolicyError::Io(error.to_string()))?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or(PolicyError::EmptyName)?;
    load_document(&input, stem, &options)
}

/// Recursively load visible `.yaml` and `.yml` files in lexical relative-path order.
pub fn load_policy_dir(
    path: impl AsRef<Path>,
    options: ApplyOptions,
) -> Result<LoadOutcome, PolicyError> {
    let root = path.as_ref();
    let mut files = Vec::new();
    collect_policy_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut outcome = LoadOutcome::default();
    let mut names = IndexMap::<String, ()>::new();
    for (_, file) in files {
        let loaded = load_policy_file(&file, options.clone())?;
        for policy in loaded.policies {
            if names.contains_key(&policy.name) {
                return Err(PolicyError::DuplicateName(policy.name));
            }
            names.insert(policy.name.clone(), ());
            outcome.policies.push(policy);
        }
        outcome.warnings.extend(loaded.warnings);
    }
    Ok(outcome)
}

fn collect_policy_files(
    root: &Path,
    path: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), PolicyError> {
    for entry in fs::read_dir(path).map_err(|error| PolicyError::Io(error.to_string()))? {
        let entry = entry.map_err(|error| PolicyError::Io(error.to_string()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_policy_files(root, &entry_path, files)?;
        } else if file_type.is_file()
            && matches!(
                entry_path
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("yaml" | "yml")
            )
        {
            files.push((
                entry_path
                    .strip_prefix(root)
                    .expect("walked file stays below root")
                    .to_path_buf(),
                entry_path,
            ));
        }
    }
    Ok(())
}

fn load_document(
    input: &str,
    stem: &str,
    options: &ApplyOptions,
) -> Result<LoadOutcome, PolicyError> {
    let value = serde_yaml::from_str::<serde_yaml::Value>(input).map_err(|error| {
        let message = error.to_string();
        if message.contains("more than one document") {
            PolicyError::MultiDocument
        } else {
            PolicyError::Yaml(message)
        }
    })?;
    let mappings: Vec<&serde_yaml::Mapping> = match value {
        serde_yaml::Value::Mapping(ref mapping) => vec![mapping],
        serde_yaml::Value::Sequence(ref values) => values
            .iter()
            .map(|value| {
                value
                    .as_mapping()
                    .ok_or_else(|| PolicyError::InvalidTopLevel(yaml_shape(value).to_string()))
            })
            .collect::<Result<_, _>>()?,
        ref value => return Err(PolicyError::InvalidTopLevel(yaml_shape(value).to_string())),
    };
    let multiple = matches!(value, serde_yaml::Value::Sequence(_));
    let mut outcome = LoadOutcome::default();
    let mut names = IndexMap::<String, ()>::new();
    for (index, mapping) in mappings.into_iter().enumerate() {
        let fallback = if multiple {
            format!("{stem}-{}", index + 1)
        } else {
            stem.to_string()
        };
        let (policy, warnings) = policy_from_mapping(mapping, fallback, options)?;
        if names.contains_key(&policy.name) {
            return Err(PolicyError::DuplicateName(policy.name));
        }
        names.insert(policy.name.clone(), ());
        outcome.policies.push(policy);
        outcome.warnings.extend(warnings);
    }
    Ok(outcome)
}

fn policy_from_mapping(
    mapping: &serde_yaml::Mapping,
    fallback_name: String,
    options: &ApplyOptions,
) -> Result<(Policy, Vec<Warning>), PolicyError> {
    let kind = mapping_get(mapping, "kind");
    let explicit = match kind {
        None => false,
        Some(serde_yaml::Value::String(kind)) if kind == "state" => false,
        Some(serde_yaml::Value::String(kind)) if kind == "policy" => true,
        Some(serde_yaml::Value::String(kind)) => {
            return Err(PolicyError::UnknownKind(kind.clone()));
        }
        Some(value) => return Err(PolicyError::UnknownKind(yaml_shape(value).to_string())),
    };
    let name = if explicit {
        mapping_get(mapping, "name")
            .and_then(serde_yaml::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .ok_or(PolicyError::EmptyName)?
    } else {
        fallback_name
    };
    let priority = mapping_get(mapping, "priority")
        .map(parse_priority)
        .transpose()?;
    let metadata = mapping_get(mapping, "metadata")
        .map(yaml_map_to_model)
        .transpose()?
        .map(Value::into_map)
        .transpose()
        .map_err(|_| PolicyError::MetadataNotMapping)?;

    let mut candidate = serde_yaml::Mapping::new();
    for (key, value) in mapping {
        if key
            .as_str()
            .is_some_and(|key| matches!(key, "kind" | "metadata" | "priority"))
            || (explicit && key.as_str() == Some("name"))
        {
            continue;
        }
        candidate.insert(key.clone(), value.clone());
    }
    let schemas = ProviderRegistry.schema_registry();
    let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(candidate))
        .map_err(|error| PolicyError::Yaml(error.to_string()))?;
    let mut state = schemas
        .from_yaml(&text)
        .map_err(PolicyError::State)?
        .pop()
        .expect("mapping yields one state");
    let mut warnings = Vec::new();
    if explicit {
        if state.match_spec.is_empty() {
            return Err(PolicyError::MissingMatch);
        }
    } else if state.match_spec.is_empty() {
        let prepared = prepare_for_apply_with_registry(&[state], options, &schemas)
            .map_err(PolicyError::State)?;
        warnings = prepared.warnings;
        state = prepared
            .states
            .into_iter()
            .next()
            .expect("one input state yields one output state");
    } else {
        validate_policy_state(&state, &schemas)?;
    }
    if explicit {
        validate_policy_state(&state, &schemas)?;
    }
    Ok((
        Policy {
            name,
            match_spec: state.match_spec,
            device_type: state.device_type,
            fields: state.fields,
            priority,
            metadata,
        },
        warnings,
    ))
}

fn validate_policy_state(state: &State, schemas: &SchemaRegistry) -> Result<(), PolicyError> {
    let errors = schemas.validate_writable(state);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(PolicyError::Validation(errors))
    }
}

fn parse_priority(value: &serde_yaml::Value) -> Result<i32, PolicyError> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(PolicyError::InvalidPriority)
}

fn mapping_get<'a>(mapping: &'a serde_yaml::Mapping, name: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(name.to_string()))
}

fn yaml_map_to_model(value: &serde_yaml::Value) -> Result<Value, PolicyError> {
    match value {
        serde_yaml::Value::Null => Err(PolicyError::Yaml(
            "null metadata values are unsupported".to_string(),
        )),
        serde_yaml::Value::Bool(value) => Ok(Value::Bool(*value)),
        serde_yaml::Value::Number(value) => value
            .as_u64()
            .map(Value::U64)
            .or_else(|| value.as_i64().map(Value::I64))
            .ok_or_else(|| {
                PolicyError::Yaml("floating-point metadata values are unsupported".to_string())
            }),
        serde_yaml::Value::String(value) => Ok(Value::String(value.clone())),
        serde_yaml::Value::Sequence(values) => values
            .iter()
            .map(yaml_map_to_model)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        serde_yaml::Value::Mapping(values) => {
            let mut result = IndexMap::new();
            for (key, value) in values {
                let key = key.as_str().ok_or_else(|| {
                    PolicyError::Yaml("metadata keys must be strings".to_string())
                })?;
                result.insert(key.to_string(), yaml_map_to_model(value)?);
            }
            Ok(Value::Map(result))
        }
        serde_yaml::Value::Tagged(_) => Err(PolicyError::Yaml(
            "tagged metadata values are unsupported".to_string(),
        )),
    }
}

trait IntoMap {
    fn into_map(self) -> Result<IndexMap<String, Value>, ()>;
}

impl IntoMap for Value {
    fn into_map(self) -> Result<IndexMap<String, Value>, ()> {
        match self {
            Value::Map(values) => Ok(values),
            _ => Err(()),
        }
    }
}

fn yaml_shape(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a sequence",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn load(input: &str) -> LoadOutcome {
        load_document(input, "test", &ApplyOptions::default()).unwrap()
    }

    fn policy(name: &str, fields: &[(&str, Value)]) -> Policy {
        let mut values = IndexMap::new();
        for (key, value) in fields {
            values.insert((*key).to_string(), value.clone());
        }
        Policy {
            name: name.to_string(),
            match_spec: Match {
                name: Some("eth0".to_string()),
                ..Match::default()
            },
            device_type: String::new(),
            fields: values,
            priority: None,
            metadata: None,
        }
    }

    #[test]
    fn registry_inventory_is_fixed_and_ordered() {
        let registrations = ProviderRegistry.registrations();
        assert_eq!(registrations.len(), 3);
        assert_eq!(registrations[0].kind, "dhcp4");
        assert_eq!(registrations[0].location(), "ipv4.dhcp4");
        assert_eq!(registrations[0].source, ProviderSource::Dhcp);
        assert_eq!(registrations[0].default_priority(), 50);
        assert_eq!(registrations[1].kind, "dhcp6");
        assert_eq!(registrations[1].location(), "ipv6.dhcp6");
        assert_eq!(registrations[2].kind, "ra");
        assert_eq!(registrations[2].location(), "ipv6.ra");
        assert_eq!(registrations[2].source, ProviderSource::Ra);
        assert_eq!(registrations[2].default_priority(), 25);
    }

    #[test]
    fn provider_schemas_compose_without_conflict() {
        // `schema_registry()` panics if a provider's `params_schema` is
        // malformed or its dotted location collides with another schema
        // node, so building it here asserts every registered provider
        // schema is well-formed independent of any policy document.
        let schemas = ProviderRegistry.schema_registry();
        assert!(schemas.field_info("ipv4", "dhcp4.route-metric").is_some());
        assert!(schemas.field_info("base", "ipv6.dhcp6").is_some());
        assert!(schemas.field_info("base", "ipv6.ra").is_some());
    }

    #[test]
    fn explicit_policy_splits_static_fields_and_dhcp_parameters() {
        let outcome = load(
            "kind: policy\nname: office\nmatch:\n  name: eth0\npriority: 101\nmetadata:\n  managed-by: test\nmtu: 1500\nipv4:\n  addresses:\n    - ip: 192.0.2.10/24\n  dhcp4:\n    send-hostname: true\n    route-metric: 100\n",
        );
        let policy = &outcome.policies[0];
        assert_eq!(policy.name, "office");
        assert_eq!(
            policy.metadata.as_ref().unwrap().get("managed-by"),
            Some(&Value::String("test".to_string()))
        );
        let produced = policy.produce().unwrap();
        let state = produced.state.unwrap();
        assert_eq!(
            state.source,
            Source::Static {
                policy: "office".to_string()
            }
        );
        assert_eq!(state.priority, 101);
        assert_eq!(
            state.fields.keys().collect::<Vec<_>>(),
            vec![&"mtu".to_string(), &"ipv4".to_string()]
        );
        let Value::Map(ipv4) = state.fields.get("ipv4").unwrap() else {
            panic!("ipv4 must be a map")
        };
        assert!(ipv4.contains_key("addresses"));
        assert!(!ipv4.contains_key("dhcp4"));
        assert_eq!(produced.providers.len(), 1);
        assert_eq!(produced.providers[0].kind, "dhcp4");
        assert_eq!(
            produced.providers[0].params.get("route-metric"),
            Some(&Value::U64(100))
        );
        assert_eq!(produced.providers[0].match_spec, policy.match_spec);
    }

    #[test]
    fn provider_only_policy_has_no_empty_static_contribution() {
        let policy = &load("kind: policy\nname: dhcp\nmatch: { name: eth0 }\nipv4:\n  dhcp4: {}\n")
            .policies[0];
        let produced = policy.produce().unwrap();
        assert!(produced.state.is_none());
        assert_eq!(produced.providers.len(), 1);
    }

    #[test]
    fn production_preserves_explicitly_empty_maps() {
        for input in [
            "match: { name: eth0 }\nipv4: {}\n",
            "match: { name: eth0 }\nipv4: {}\nipv6: { ra: {} }\n",
        ] {
            let produced = load(input).policies[0].produce().unwrap();
            let state = produced.state.unwrap();
            assert_eq!(state.fields.len(), 1);
            assert_eq!(state.fields["ipv4"], Value::Map(IndexMap::new()));
        }
    }

    #[test]
    fn explicit_device_type_survives_production_and_writable_validation() {
        let outcome = load(
            "kind: policy\nname: office\nmatch: { name: eth0 }\ntype: ethernet\nmtu: 1500\nipv4: { dhcp4: {} }\n",
        );
        let produced = outcome.policies[0].produce().unwrap();
        let state = produced.state.unwrap();
        assert_eq!(state.device_type, "ethernet");
        assert_eq!(state.match_spec.name.as_deref(), Some("eth0"));
        assert_eq!(state.fields["mtu"], Value::U64(1500));
        assert!(SchemaRegistry::new().validate_writable(&state).is_empty());
        assert_eq!(produced.providers[0].device_type, "ethernet");
    }

    #[test]
    fn bare_query_form_normalizes_and_keeps_provider_trigger() {
        let outcome = load("name: eth0\ntype: ethernet\ncarrier: true\nipv4:\n  dhcp4: {}\n");
        let policy = &outcome.policies[0];
        assert_eq!(policy.name, "test");
        assert_eq!(policy.match_spec.name.as_deref(), Some("eth0"));
        assert_eq!(policy.match_spec.r#type.as_deref(), Some("ethernet"));
        assert!(policy.device_type.is_empty());
        assert_eq!(
            outcome.warnings,
            vec![Warning {
                key: "carrier".to_string(),
                known_read_only: true
            }]
        );
        assert_eq!(policy.produce().unwrap().providers.len(), 1);
    }

    #[test]
    fn invalid_provider_parameters_are_rejected_before_production() {
        let error = load_document(
            "kind: policy\nname: bad\nmatch: { name: eth0 }\nipv4:\n  dhcp4:\n    route-metric: -1\n    unknown: true\n",
            "bad",
            &ApplyOptions::default(),
        )
        .unwrap_err();
        let PolicyError::Validation(errors) = error else {
            panic!("expected validation error")
        };
        assert_eq!(errors.len(), 2);
        assert!(
            matches!(errors[0], ValidationError::OutOfRange { ref path, .. } if path == "ipv4.dhcp4.route-metric")
        );
        assert!(
            matches!(errors[1], ValidationError::UnknownField { ref path, .. } if path == "ipv4.dhcp4.unknown")
        );
    }

    #[test]
    fn policy_set_preserves_overlapping_contributions_and_rejects_duplicate_names() {
        let mut first = policy("first", &[("mtu", Value::U64(1500))]);
        first.match_spec.name = Some("eth0".to_string());
        let mut second = policy("second", &[("enabled", Value::Bool(true))]);
        second.match_spec.name = Some("eth0".to_string());
        let mut set = PolicySet::default();
        set.insert(first).unwrap();
        set.insert(second).unwrap();
        let all = set.produce_all().unwrap();
        assert_eq!(all.states.len(), 2);
        assert_eq!(all.states[0].fields.get("mtu"), Some(&Value::U64(1500)));
        assert_eq!(
            all.states[1].fields.get("enabled"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            set.insert(policy("first", &[])),
            Err(PolicyError::DuplicateName("first".to_string()))
        );
    }

    #[test]
    fn mixed_sequence_uses_suffixes_only_for_bare_entries() {
        let outcome = load_document(
            "- name: eth0\n  mtu: 1500\n- kind: policy\n  name: named\n  match: { name: eth1 }\n  enabled: true\n",
            "switches",
            &ApplyOptions::default(),
        )
        .unwrap();
        assert_eq!(
            outcome
                .policies
                .iter()
                .map(|policy| policy.name.as_str())
                .collect::<Vec<_>>(),
            vec!["switches-1", "named"]
        );
    }

    #[test]
    fn single_entry_sequence_still_gets_a_name_suffix() {
        assert_eq!(
            load("- name: eth0\n  mtu: 1500\n").policies[0].name,
            "test-1"
        );
        assert_eq!(load("name: eth0\nmtu: 1500\n").policies[0].name, "test");
    }

    #[test]
    fn directory_load_is_sorted_skips_hidden_paths_and_detects_duplicates() {
        let root = std::env::temp_dir().join(format!(
            "netfyr-policy-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("z.yaml"), "match: { name: eth2 }\nmtu: 1500\n").unwrap();
        fs::write(
            root.join("sub/a.yml"),
            "match: { name: eth1 }\nenabled: true\n",
        )
        .unwrap();
        fs::write(
            root.join(".hidden/ignored.yaml"),
            "match: { name: hidden }\nmtu: 1500\n",
        )
        .unwrap();
        let outcome = load_policy_dir(&root, ApplyOptions::default()).unwrap();
        assert_eq!(
            outcome
                .policies
                .iter()
                .map(|policy| policy.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
        fs::write(
            root.join("sub/z.yaml"),
            "match: { name: eth3 }\nmtu: 1500\n",
        )
        .unwrap();
        assert!(matches!(
            load_policy_dir(&root, ApplyOptions::default()),
            Err(PolicyError::DuplicateName(name)) if name == "z"
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
