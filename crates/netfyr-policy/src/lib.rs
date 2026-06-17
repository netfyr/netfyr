//! netfyr-policy crate — policy data model and factory implementations.
//!
//! A policy is a named factory that produces a `StateSet`. The static factory
//! (`StaticFactory`) is the simplest: it copies inline state definitions from
//! the policy document into a `StateSet`. Dynamic factories (e.g., DHCPv4)
//! run inside the daemon.
//!
//! [`load_policy_file`] is the unified entry point for reading a single policy
//! file. It handles three document kinds:
//!
//! - No `kind:` field or `kind: state` → bare state, wrapped into a static
//!   `Policy` with priority 100 and a name derived from the filename.
//! - `kind: policy` → parsed as an explicit `Policy` (via
//!   `parse_policy_from_value`).
//! - Any other `kind` value → error.
//!
//! [`load_policy_dir`] walks a directory tree and calls [`load_policy_file`] on
//! every `.yaml`/`.yml` file, collecting results into a [`PolicySet`].

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use netfyr_state::{parse_state_value, union, ConflictError, Provenance, Selector, State, StateSet, YamlError};
use serde::de::Deserialize;
use serde::{Deserialize as DeserializeDerive, Serialize};
use walkdir::WalkDir;

// ── FactoryType ───────────────────────────────────────────────────────────────

/// The type of factory that produces state for a policy.
///
/// Serializes to/from lowercase strings in YAML (`"static"`, `"dhcpv4"`).
#[derive(Clone, Debug, PartialEq, Serialize, DeserializeDerive)]
#[serde(rename_all = "lowercase")]
pub enum FactoryType {
    /// Produces state from inline YAML definitions inside the policy document.
    Static,
    /// Produces state by acquiring a DHCPv4 lease at runtime (daemon-side).
    Dhcpv4,
}

// ── Policy ────────────────────────────────────────────────────────────────────

/// A named factory that produces a desired `StateSet`.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Unique policy name (e.g., `"eth0"`, `"eth0-dhcp"`).
    pub name: String,
    /// Which factory type produces the state.
    pub factory_type: FactoryType,
    /// Numeric priority propagated to all generated fields (default: 100).
    pub priority: u32,
    /// Inline state for single-entity static policies.
    pub state: Option<State>,
    /// Inline states for multi-entity static policies.
    pub states: Option<Vec<State>>,
    /// Target selector (e.g., which interface to run DHCP on).
    pub selector: Option<Selector>,
}

// ── PolicySet ─────────────────────────────────────────────────────────────────

/// A collection of `Policy` values keyed by name, preserving insertion order.
#[derive(Clone, Debug, Default)]
pub struct PolicySet {
    inner: IndexMap<String, Policy>,
}

impl PolicySet {
    /// Returns a new, empty `PolicySet`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a policy by name.
    pub fn insert(&mut self, policy: Policy) {
        self.inner.insert(policy.name.clone(), policy);
    }

    /// Returns a reference to the policy with the given name.
    pub fn get(&self, name: &str) -> Option<&Policy> {
        self.inner.get(name)
    }

    /// Removes and returns the policy with the given name.
    pub fn remove(&mut self, name: &str) -> Option<Policy> {
        self.inner.shift_remove(name)
    }

    /// Iterates over all policies in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Policy> {
        self.inner.values()
    }

    /// Returns the number of policies in the set.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the set contains no policies.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Runs all static factories and unions the results into a single `StateSet`.
    ///
    /// Non-static policies (e.g., `Dhcpv4`) are silently skipped — the daemon
    /// handles those at runtime. Returns `Err(FactoryError::ConflictError)` if
    /// two static policies produce the same entity field at the same priority
    /// with different values.
    pub fn produce_all_static(&self) -> Result<StateSet, FactoryError> {
        let factory = StaticFactory;
        let mut combined = StateSet::new();
        for policy in self.iter().filter(|p| p.factory_type == FactoryType::Static) {
            let state_set = factory.produce(policy)?;
            combined = union(&combined, &state_set).map_err(FactoryError::ConflictError)?;
        }
        Ok(combined)
    }
}

// ── StateFactory trait ────────────────────────────────────────────────────────

/// The interface all policy factories implement.
pub trait StateFactory {
    fn produce(&self, policy: &Policy) -> Result<StateSet, FactoryError>;
}

// ── FactoryError ──────────────────────────────────────────────────────────────

/// Errors that can occur during factory execution.
#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    /// Static factory but no `state` or `states` field defined in the policy.
    #[error(
        "static factory for policy '{policy_name}' has neither 'state' nor 'states' defined"
    )]
    MissingState { policy_name: String },

    /// Factory misconfiguration (wrong fields for the factory type, etc.).
    #[error(
        "invalid factory configuration for policy '{policy_name}' (type: {factory_type}): {reason}"
    )]
    InvalidFactory {
        policy_name: String,
        factory_type: String,
        reason: String,
    },

    /// Wraps a `StateSet` union conflict (same entity, same field, same priority, different values).
    #[error(transparent)]
    ConflictError(#[from] ConflictError),

    /// Catch-all for factory errors that don't fit a specific variant.
    #[error("{message}")]
    Other { message: String },
}

// ── StaticFactory ─────────────────────────────────────────────────────────────

/// The simplest factory type: copies inline state definitions from the policy
/// into a `StateSet`, stamping each entity with the policy's priority and name.
pub struct StaticFactory;

impl StateFactory for StaticFactory {
    fn produce(&self, policy: &Policy) -> Result<StateSet, FactoryError> {
        // Reject policies with no state defined (or an empty states list).
        let states_empty = policy.states.as_ref().is_none_or(|v| v.is_empty());
        if policy.state.is_none() && states_empty {
            return Err(FactoryError::MissingState {
                policy_name: policy.name.clone(),
            });
        }

        let mut set = StateSet::new();

        if let Some(state) = &policy.state {
            tracing::debug!(
                policy = %policy.name,
                entity_type = %state.entity_type,
                "static factory inserting single state"
            );
            set.insert(apply_policy_to_state(state, policy));
        }

        if let Some(states) = &policy.states {
            for state in states {
                tracing::debug!(
                    policy = %policy.name,
                    entity_type = %state.entity_type,
                    "static factory inserting state from states list"
                );
                set.insert(apply_policy_to_state(state, policy));
            }
        }

        Ok(set)
    }
}

/// Clones a state and stamps it with the policy's priority, policy_ref, and
/// `UserConfigured` provenance on every field. If the policy has a top-level
/// selector, it overrides the state's own selector (new spec format where state
/// contains only config fields and the selector lives at the policy level).
fn apply_policy_to_state(state: &State, policy: &Policy) -> State {
    let mut s = state.clone();
    s.priority = policy.priority;
    s.policy_ref = Some(policy.name.clone());
    if let Some(selector) = &policy.selector {
        s.selector = selector.clone();
    }
    for field in s.fields.values_mut() {
        field.provenance = Provenance::UserConfigured {
            policy_ref: policy.name.clone(),
        };
    }
    s
}

// ── PolicyError ───────────────────────────────────────────────────────────────

/// Errors that can occur while parsing policy YAML.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// YAML syntax error or flat-format state parse error.
    #[error("YAML error: {0}")]
    Yaml(#[from] YamlError),

    /// A required field is absent from the policy document.
    #[error("missing required field '{field}' in policy document")]
    MissingField { field: String },

    /// The `kind` field is present but not `"policy"`.
    #[error("unknown 'kind' value: '{kind}'; expected 'policy'")]
    InvalidKind { kind: String },

    /// The `kind` field is `"state"` or absent — handled by SPEC-008.
    #[error(
        "unsupported 'kind' value: '{kind}'; bare state documents are not yet supported here"
    )]
    UnsupportedKind { kind: String },

    /// A field has the wrong YAML type.
    #[error("field '{field}' has wrong type; expected {expected}")]
    InvalidFieldType { field: String, expected: String },

    /// The `factory` string does not match any known `FactoryType`.
    #[error("unknown factory type: '{factory}'")]
    UnknownFactory { factory: String },

    /// Serde-level deserialization error (e.g., while decoding `Selector`).
    #[error("serde error: {0}")]
    Serde(#[from] serde_yaml::Error),
}

// ── parse_policy_from_value ───────────────────────────────────────────────────

/// Parses a single non-null `serde_yaml::Value` into a `Policy`.
///
/// The value must have `kind: policy`. Returns `PolicyError::UnsupportedKind`
/// for absent or `"state"` kind, and `PolicyError::InvalidKind` for any other
/// unrecognised kind value. Used by both `parse_policy_yaml` and the file
/// loader to avoid duplicating parsing logic.
pub(crate) fn parse_policy_from_value(raw: serde_yaml::Value) -> Result<Policy, PolicyError> {
    let map = match &raw {
        serde_yaml::Value::Mapping(m) => m,
        _ => {
            return Err(PolicyError::MissingField {
                field: "kind".to_string(),
            })
        }
    };

    // ── kind ──────────────────────────────────────────────────────────────────

    let kind_key = serde_yaml::Value::String("kind".to_string());
    match map.get(&kind_key) {
        Some(serde_yaml::Value::String(k)) if k == "policy" => {}
        Some(serde_yaml::Value::String(k)) if k == "state" => {
            return Err(PolicyError::UnsupportedKind { kind: k.clone() });
        }
        Some(serde_yaml::Value::String(k)) => {
            return Err(PolicyError::InvalidKind { kind: k.clone() });
        }
        Some(_) => {
            return Err(PolicyError::InvalidKind {
                kind: "<non-string>".to_string(),
            });
        }
        None => {
            return Err(PolicyError::UnsupportedKind {
                kind: "<absent>".to_string(),
            });
        }
    }

    // ── name (required string) ────────────────────────────────────────────────

    let name_key = serde_yaml::Value::String("name".to_string());
    let name = match map.get(&name_key) {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(PolicyError::InvalidFieldType {
                field: "name".to_string(),
                expected: "string".to_string(),
            })
        }
        None => {
            return Err(PolicyError::MissingField {
                field: "name".to_string(),
            })
        }
    };

    // ── factory (required string → FactoryType) ───────────────────────────────

    let factory_key = serde_yaml::Value::String("factory".to_string());
    let factory_type = match map.get(&factory_key) {
        Some(serde_yaml::Value::String(factory_str)) => {
            serde_yaml::from_value::<FactoryType>(serde_yaml::Value::String(
                factory_str.clone(),
            ))
            .map_err(|_| PolicyError::UnknownFactory {
                factory: factory_str.clone(),
            })?
        }
        Some(_) => {
            return Err(PolicyError::InvalidFieldType {
                field: "factory".to_string(),
                expected: "string".to_string(),
            })
        }
        None => {
            return Err(PolicyError::MissingField {
                field: "factory".to_string(),
            })
        }
    };

    // ── priority (optional non-negative integer, default 100) ─────────────────

    let priority_key = serde_yaml::Value::String("priority".to_string());
    let priority = match map.get(&priority_key) {
        Some(serde_yaml::Value::Number(n)) => {
            let p = n.as_u64().ok_or_else(|| PolicyError::InvalidFieldType {
                field: "priority".to_string(),
                expected: "non-negative integer".to_string(),
            })?;
            u32::try_from(p).map_err(|_| PolicyError::InvalidFieldType {
                field: "priority".to_string(),
                expected: "integer within u32 range (0..=4294967295)".to_string(),
            })?
        }
        Some(_) => {
            return Err(PolicyError::InvalidFieldType {
                field: "priority".to_string(),
                expected: "integer".to_string(),
            })
        }
        None => 100,
    };

    // ── selector (optional mapping → Selector) ────────────────────────────────

    let selector_key_yaml = serde_yaml::Value::String("selector".to_string());
    let selector = match map.get(&selector_key_yaml) {
        Some(v) => {
            let sel =
                serde_yaml::from_value::<Selector>(v.clone()).map_err(PolicyError::Serde)?;
            Some(sel)
        }
        None => None,
    };

    // ── state (optional flat mapping → State) ─────────────────────────────────

    let state_key = serde_yaml::Value::String("state".to_string());
    let state = match map.get(&state_key) {
        Some(v) => {
            let s = parse_state_value(v.clone()).map_err(PolicyError::Yaml)?;
            Some(s)
        }
        None => None,
    };

    // ── states (optional sequence of flat mappings → Vec<State>) ──────────────

    let states_key = serde_yaml::Value::String("states".to_string());
    let states = match map.get(&states_key) {
        Some(serde_yaml::Value::Sequence(seq)) => {
            let mut result = Vec::new();
            for item in seq {
                let s = parse_state_value(item.clone()).map_err(PolicyError::Yaml)?;
                result.push(s);
            }
            Some(result)
        }
        Some(_) => {
            return Err(PolicyError::InvalidFieldType {
                field: "states".to_string(),
                expected: "sequence".to_string(),
            })
        }
        None => None,
    };

    Ok(Policy {
        name,
        factory_type,
        priority,
        state,
        states,
        selector,
    })
}

// ── parse_policy_yaml ─────────────────────────────────────────────────────────

/// Parses a (possibly multi-document) YAML string into a list of `Policy` values.
///
/// Each document must have `kind: policy`. Documents with `kind: state` or no
/// `kind` field return `Err(PolicyError::UnsupportedKind)` — auto-wrapping of
/// bare state documents into policies is handled by the file loader
/// (`load_policy_file`). Trailing `---` null documents are silently
/// skipped.
pub fn parse_policy_yaml(input: &str) -> Result<Vec<Policy>, PolicyError> {
    let mut policies = Vec::new();

    for document in serde_yaml::Deserializer::from_str(input) {
        let raw: serde_yaml::Value =
            Deserialize::deserialize(document).map_err(PolicyError::Serde)?;

        // Silently skip null documents (e.g., a trailing `---`).
        if matches!(raw, serde_yaml::Value::Null) {
            continue;
        }

        policies.push(parse_policy_from_value(raw)?);
    }

    Ok(policies)
}

// ── LoaderError ───────────────────────────────────────────────────────────────

/// Errors that can occur while loading policy files from disk.
#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    /// Failed to read a file from the filesystem.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// YAML syntax error encountered while deserializing a document.
    #[error("YAML syntax error in {path}: {source}")]
    Yaml {
        path: PathBuf,
        source: serde_yaml::Error,
    },

    /// Error parsing a bare state document.
    #[error("state parse error in {path}: {source}")]
    State { path: PathBuf, source: YamlError },

    /// Error parsing a `kind: policy` document.
    #[error("policy parse error in {path}: {source}")]
    Policy {
        path: PathBuf,
        source: PolicyError,
    },

    /// The `kind` field has an unrecognised value.
    #[error("unknown kind '{kind}' in {path}; expected 'policy' or 'state'")]
    UnknownKind { kind: String, path: PathBuf },

    /// A bare state document is missing the required `selector:` sub-mapping.
    #[error(
        "bare state document in {path} is missing a required 'selector:' sub-mapping"
    )]
    MissingSelector { path: PathBuf },

    /// Two files in a directory produced a policy with the same name.
    #[error("duplicate policy name '{name}' (from {path})")]
    DuplicatePolicyName { name: String, path: PathBuf },

    /// Error traversing a directory tree.
    #[error("directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derives a policy name from a file path by stripping the extension.
///
/// `eth0.yaml` → `"eth0"`, `bond0-vlan100.yml` → `"bond0-vlan100"`.
/// Falls back to `"unnamed"` for paths with no file stem or non-UTF-8 names.
fn policy_name_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string()
}

// ── load_policy_file ──────────────────────────────────────────────────────────

/// Reads `path` and returns all policies defined in it.
///
/// YAML documents without a `kind:` field, or with `kind: state`, are treated
/// as bare states and auto-wrapped into a static `Policy` with priority 100.
/// The policy name is the file stem for single-document files, or
/// `"{stem}-{N}"` (1-based) for multi-document files.
///
/// Documents with `kind: policy` are parsed as explicit policies and keep their
/// declared `name` and `priority`.
///
/// Null/empty documents (trailing `---`) are silently skipped and do not affect
/// document numbering.
pub fn load_policy_file(path: &Path) -> Result<Vec<Policy>, LoaderError> {
    let content = std::fs::read_to_string(path).map_err(|e| LoaderError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let base_name = policy_name_from_path(path);
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    // ── First pass: collect non-null documents ────────────────────────────────

    let mut docs: Vec<serde_yaml::Value> = Vec::new();
    for document in serde_yaml::Deserializer::from_str(&content) {
        let raw: serde_yaml::Value =
            Deserialize::deserialize(document).map_err(|e| LoaderError::Yaml {
                path: path.to_path_buf(),
                source: e,
            })?;
        if !matches!(raw, serde_yaml::Value::Null) {
            docs.push(raw);
        }
    }

    let is_multi = docs.len() > 1;

    // ── Second pass: dispatch by kind ─────────────────────────────────────────

    let mut policies = Vec::new();

    for (index, raw) in docs.into_iter().enumerate() {
        let doc_num = index + 1; // 1-based index among non-null documents

        // Extract the `kind` field as an owned string. If the field exists but
        // is not a YAML string, treat it as an unknown kind rather than
        // silently falling through to the bare-state path.
        let kind: Option<String> = match raw.get("kind") {
            None => None,
            Some(v) => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    return Err(LoaderError::UnknownKind {
                        kind: "<non-string>".to_string(),
                        path: path.to_path_buf(),
                    });
                }
            },
        };

        match kind.as_deref() {
            // ── Bare state: auto-wrap into a static policy ────────────────────
            None | Some("state") => {
                // Require a "selector:" sub-mapping in bare state documents.
                let has_selector = raw
                    .as_mapping()
                    .map(|m| {
                        m.contains_key(serde_yaml::Value::String("selector".to_string()))
                    })
                    .unwrap_or(false);
                if !has_selector {
                    return Err(LoaderError::MissingSelector {
                        path: path.to_path_buf(),
                    });
                }

                let state =
                    parse_state_value(raw).map_err(|e| LoaderError::State {
                        path: path.to_path_buf(),
                        source: e,
                    })?;

                let policy_selector = Some(state.selector.clone());

                let name = if is_multi {
                    format!("{base_name}-{doc_num}")
                } else {
                    base_name.clone()
                };

                tracing::info!(
                    "Wrapping bare state from {} as static policy \"{}\" with priority 100",
                    filename,
                    name
                );

                policies.push(Policy {
                    name,
                    factory_type: FactoryType::Static,
                    priority: 100,
                    state: Some(state),
                    states: None,
                    selector: policy_selector,
                });
            }

            // ── Explicit policy: delegate to shared parser ────────────────────
            Some("policy") => {
                let policy =
                    parse_policy_from_value(raw).map_err(|e| LoaderError::Policy {
                        path: path.to_path_buf(),
                        source: e,
                    })?;
                policies.push(policy);
            }

            // ── Unknown kind: error ───────────────────────────────────────────
            Some(other) => {
                return Err(LoaderError::UnknownKind {
                    kind: other.to_string(),
                    path: path.to_path_buf(),
                });
            }
        }
    }

    Ok(policies)
}

// ── load_policy_dir ───────────────────────────────────────────────────────────

/// Recursively loads all `.yaml` and `.yml` files from `path` into a
/// [`PolicySet`].
///
/// Files whose name begins with `.` are skipped. Returns an error if two files
/// produce a policy with the same name.
pub fn load_policy_dir(path: &Path) -> Result<PolicySet, LoaderError> {
    let mut policy_set = PolicySet::new();

    for entry in WalkDir::new(path).into_iter() {
        let entry = entry?; // propagate walkdir::Error

        // Skip directories and non-regular files.
        if !entry.file_type().is_file() {
            continue;
        }

        // Skip hidden files (names starting with `.`).
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }

        // Only process YAML files.
        let ext = entry.path().extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("yaml" | "yml")) {
            continue;
        }

        let policies = load_policy_file(entry.path())?;

        for policy in policies {
            if policy_set.get(&policy.name).is_some() {
                return Err(LoaderError::DuplicatePolicyName {
                    name: policy.name,
                    path: entry.path().to_path_buf(),
                });
            }
            policy_set.insert(policy);
        }
    }

    Ok(policy_set)
}

// ── Policy model tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_state::{FieldValue, StateMetadata, Value};

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build a `State` with a named selector and the given configuration fields.
    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>, priority: u32) -> State {
        let mut field_map: IndexMap<String, FieldValue> = IndexMap::new();
        for (k, v) in fields {
            field_map.insert(
                k.to_string(),
                FieldValue {
                    value: v,
                    provenance: Provenance::KernelDefault,
                },
            );
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority,
        }
    }

    /// Build a static `Policy` with a single embedded state.
    fn static_policy(name: &str, priority: u32, state: State) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Static,
            priority,
            state: Some(state),
            states: None,
            selector: None,
        }
    }

    /// Build a DHCPv4 `Policy` with a named selector (no inline state).
    fn dhcp_policy(name: &str, interface: &str) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Dhcpv4,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name(interface)),
        }
    }

    // ── StaticFactory helper builders ─────────────────────────────────────────

    fn make_state_no_priority(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        make_state(entity_type, name, fields, 100)
    }

    fn static_policy_single(name: &str, priority: u32, state: State) -> Policy {
        static_policy(name, priority, state)
    }

    fn static_policy_multi(name: &str, priority: u32, states: Vec<State>) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Static,
            priority,
            state: None,
            states: Some(states),
            selector: None,
        }
    }

    fn empty_static_policy(name: &str) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Static,
            priority: 100,
            state: None,
            states: None,
            selector: None,
        }
    }

    // ── Fixture YAML strings ──────────────────────────────────────────────────

    const STATIC_POLICY_YAML: &str = "\
kind: policy
name: eth0-static
factory: static
priority: 150
state:
  type: ethernet
  name: eth0
  mtu: 1500
";

    const MULTI_ENTITY_POLICY_YAML: &str = "\
kind: policy
name: server-network
factory: static
priority: 100
states:
  - type: ethernet
    name: eth0
    mtu: 1500
  - type: dns
    scope: global
    servers:
      - 10.0.1.2
";

    const DHCPV4_POLICY_YAML: &str = "\
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth0
";

    const NO_PRIORITY_YAML: &str = "\
kind: policy
name: test-policy
factory: static
state:
  type: ethernet
  name: eth0
";

    const MULTI_DOC_YAML: &str = "\
kind: policy
name: eth0-static
factory: static
priority: 100
state:
  type: ethernet
  name: eth0
  mtu: 1500
---
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 50
selector:
  name: eth0
";

    // ── Feature: Policy type definitions — FactoryType serialization ──────────

    #[test]
    fn test_factory_type_dhcpv4_serializes_to_dhcpv4_string() {
        let yaml = serde_yaml::to_string(&FactoryType::Dhcpv4).unwrap();
        assert_eq!(yaml.trim(), "dhcpv4");
    }

    #[test]
    fn test_factory_type_static_serializes_to_static_string() {
        let yaml = serde_yaml::to_string(&FactoryType::Static).unwrap();
        assert_eq!(yaml.trim(), "static");
    }

    #[test]
    fn test_factory_type_dhcpv4_deserializes_from_string() {
        let ft: FactoryType = serde_yaml::from_str("dhcpv4").unwrap();
        assert_eq!(ft, FactoryType::Dhcpv4);
    }

    #[test]
    fn test_factory_type_static_deserializes_from_string() {
        let ft: FactoryType = serde_yaml::from_str("static").unwrap();
        assert_eq!(ft, FactoryType::Static);
    }

    // ── Feature: PolicySet collection ─────────────────────────────────────────

    #[test]
    fn test_policy_set_insert_and_get_returns_inserted_policy() {
        let mut set = PolicySet::new();
        let policy = static_policy(
            "eth0",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        );
        set.insert(policy);
        assert!(set.get("eth0").is_some());
    }

    #[test]
    fn test_policy_set_len_returns_one_after_single_insert() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "eth0",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_policy_set_is_empty_before_insert() {
        assert!(PolicySet::new().is_empty());
    }

    #[test]
    fn test_policy_set_get_unknown_name_returns_none() {
        let set = PolicySet::new();
        assert!(set.get("nonexistent").is_none());
    }

    #[test]
    fn test_policy_set_remove_returns_policy_and_decrements_len() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "eth0",
            100,
            make_state("ethernet", "eth0", vec![], 100),
        ));
        let removed = set.remove("eth0");
        assert!(removed.is_some());
        assert_eq!(set.len(), 0);
    }

    // ── Feature: produce_all_static ───────────────────────────────────────────

    #[test]
    fn test_produce_all_static_unions_two_static_policies() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "eth0",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(static_policy(
            "dns",
            100,
            make_state(
                "dns",
                "main",
                vec![("servers", Value::List(vec![Value::String("10.0.1.2".to_string())]))],
                100,
            ),
        ));
        let result = set.produce_all_static().unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.get("ethernet", "eth0").is_some());
        assert!(result.get("dns", "main").is_some());
    }

    #[test]
    fn test_produce_all_static_skips_dhcpv4_policies() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "eth0",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(dhcp_policy("eth1-dhcp", "eth1"));
        let result = set.produce_all_static().unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.get("ethernet", "eth0").is_some());
    }

    #[test]
    fn test_produce_all_static_equal_priority_conflict_returns_conflict_error() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "a",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(static_policy(
            "b",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100),
        ));
        let result = set.produce_all_static();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FactoryError::ConflictError(_)));
    }

    #[test]
    fn test_produce_all_static_conflict_error_identifies_mtu_field_on_ethernet_eth0() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "a",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(static_policy(
            "b",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100),
        ));
        match set.produce_all_static().unwrap_err() {
            FactoryError::ConflictError(ce) => {
                assert!(ce.conflicts.iter().any(|c| {
                    c.field == "mtu" && c.entity_type == "ethernet" && c.selector_key == "eth0"
                }));
            }
            other => panic!("expected ConflictError, got {:?}", other),
        }
    }

    #[test]
    fn test_produce_all_static_higher_priority_mtu_wins() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "base",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(static_policy(
            "override",
            200,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 200),
        ));
        let result = set.produce_all_static().unwrap();
        let state = result.get("ethernet", "eth0").expect("ethernet/eth0 must be in result");
        assert_eq!(state.fields["mtu"].value, Value::U64(9000));
    }

    #[test]
    fn test_produce_all_static_priority_winner_provenance_references_override_policy() {
        let mut set = PolicySet::new();
        set.insert(static_policy(
            "base",
            100,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100),
        ));
        set.insert(static_policy(
            "override",
            200,
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 200),
        ));
        let result = set.produce_all_static().unwrap();
        let state = result.get("ethernet", "eth0").unwrap();
        match &state.fields["mtu"].provenance {
            Provenance::UserConfigured { policy_ref } => {
                assert_eq!(policy_ref, "override");
            }
            other => panic!("expected UserConfigured provenance, got {:?}", other),
        }
    }

    #[test]
    fn test_produce_all_static_empty_set_returns_empty_stateset() {
        let set = PolicySet::new();
        let result = set.produce_all_static().unwrap();
        assert!(result.is_empty());
    }

    // ── Feature: Static factory produces StateSet ─────────────────────────────

    #[test]
    fn test_static_factory_single_state_produces_one_entity() {
        let policy = static_policy_single(
            "eth0",
            200,
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let result = StaticFactory.produce(&policy).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_static_factory_single_state_sets_entity_priority() {
        let policy = static_policy_single(
            "eth0",
            200,
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let result = StaticFactory.produce(&policy).unwrap();
        let state = result.get("ethernet", "eth0").expect("ethernet/eth0 must be in result");
        assert_eq!(state.priority, 200);
    }

    #[test]
    fn test_static_factory_single_state_sets_policy_ref() {
        let policy = static_policy_single(
            "eth0",
            200,
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let result = StaticFactory.produce(&policy).unwrap();
        let state = result.get("ethernet", "eth0").expect("ethernet/eth0 must be in result");
        assert_eq!(state.policy_ref, Some("eth0".to_string()));
    }

    #[test]
    fn test_static_factory_single_state_field_has_user_configured_provenance() {
        let policy = static_policy_single(
            "eth0",
            200,
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let result = StaticFactory.produce(&policy).unwrap();
        let state = result.get("ethernet", "eth0").unwrap();
        match &state.fields["mtu"].provenance {
            Provenance::UserConfigured { policy_ref } => {
                assert_eq!(policy_ref, "eth0");
            }
            other => panic!("expected UserConfigured provenance, got {:?}", other),
        }
    }

    #[test]
    fn test_static_factory_multiple_states_returns_two_entities() {
        let state_eth0 =
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let state_dns = make_state_no_priority("dns", "main", vec![("servers", Value::List(vec![]))]);
        let policy = static_policy_multi("server", 100, vec![state_eth0, state_dns]);
        let result = StaticFactory.produce(&policy).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_static_factory_multiple_states_ethernet_has_correct_priority_and_policy_ref() {
        let state_eth0 =
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let state_dns = make_state_no_priority("dns", "main", vec![]);
        let policy = static_policy_multi("server", 100, vec![state_eth0, state_dns]);
        let result = StaticFactory.produce(&policy).unwrap();
        let eth0 = result.get("ethernet", "eth0").unwrap();
        assert_eq!(eth0.priority, 100);
        assert_eq!(eth0.policy_ref, Some("server".to_string()));
    }

    #[test]
    fn test_static_factory_multiple_states_dns_has_correct_priority_and_policy_ref() {
        let state_eth0 =
            make_state_no_priority("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let state_dns = make_state_no_priority("dns", "main", vec![]);
        let policy = static_policy_multi("server", 100, vec![state_eth0, state_dns]);
        let result = StaticFactory.produce(&policy).unwrap();
        let dns = result.get("dns", "main").unwrap();
        assert_eq!(dns.priority, 100);
        assert_eq!(dns.policy_ref, Some("server".to_string()));
    }

    #[test]
    fn test_static_factory_no_state_returns_missing_state_error() {
        let policy = empty_static_policy("empty");
        let result = StaticFactory.produce(&policy);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FactoryError::MissingState { .. }));
    }

    #[test]
    fn test_static_factory_missing_state_error_contains_policy_name() {
        let policy = empty_static_policy("empty");
        match StaticFactory.produce(&policy).unwrap_err() {
            FactoryError::MissingState { policy_name } => {
                assert_eq!(policy_name, "empty");
            }
            other => panic!("expected MissingState error, got {:?}", other),
        }
    }

    #[test]
    fn test_static_factory_preserves_all_field_values() {
        let mut route_map: IndexMap<String, Value> = IndexMap::new();
        route_map.insert("destination".to_string(), Value::String("0.0.0.0/0".to_string()));
        route_map.insert("gateway".to_string(), Value::String("10.0.1.1".to_string()));

        let state = make_state_no_priority(
            "ethernet",
            "eth0",
            vec![
                ("mtu", Value::U64(9000)),
                (
                    "addresses",
                    Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                ),
                ("routes", Value::List(vec![Value::Map(route_map.clone())])),
            ],
        );
        let policy = static_policy_single("test", 100, state);
        let result = StaticFactory.produce(&policy).unwrap();
        let out = result.get("ethernet", "eth0").expect("ethernet/eth0 must be present");

        assert_eq!(out.fields["mtu"].value, Value::U64(9000));

        let addrs = out.fields["addresses"].value.as_list().unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0], Value::String("10.0.1.50/24".to_string()));

        let routes = out.fields["routes"].value.as_list().unwrap();
        assert_eq!(routes.len(), 1);
        let route = routes[0].as_map().unwrap();
        assert_eq!(
            route.get("destination"),
            Some(&Value::String("0.0.0.0/0".to_string()))
        );
        assert_eq!(
            route.get("gateway"),
            Some(&Value::String("10.0.1.1".to_string()))
        );
    }

    #[test]
    fn test_static_factory_empty_states_list_returns_missing_state_error() {
        let policy = Policy {
            name: "empty-list".to_string(),
            factory_type: FactoryType::Static,
            priority: 100,
            state: None,
            states: Some(vec![]),
            selector: None,
        };
        let result = StaticFactory.produce(&policy);
        assert!(matches!(result, Err(FactoryError::MissingState { .. })));
    }

    // ── Feature: Multi-document policy YAML parsing ───────────────────────────

    #[test]
    fn test_parse_static_policy_returns_one_policy() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_parse_static_policy_name_is_eth0_static() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        assert_eq!(policies[0].name, "eth0-static");
    }

    #[test]
    fn test_parse_static_policy_factory_type_is_static() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_parse_static_policy_priority_is_150() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        assert_eq!(policies[0].priority, 150);
    }

    #[test]
    fn test_parse_static_policy_state_is_some_with_entity_type_ethernet() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        // entity_type is determined by the backend, not the inline state YAML
        assert!(state.entity_type.is_empty());
    }

    #[test]
    fn test_parse_multi_entity_policy_returns_one_policy_with_two_states() {
        let policies = parse_policy_yaml(MULTI_ENTITY_POLICY_YAML).unwrap();
        assert_eq!(policies.len(), 1);
        let states = policies[0].states.as_ref().expect("states should be Some");
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn test_parse_dhcpv4_policy_returns_one_policy() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_parse_dhcpv4_policy_factory_type_is_dhcpv4() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Dhcpv4);
    }

    #[test]
    fn test_parse_dhcpv4_policy_selector_name_is_eth0() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        let selector = policies[0].selector.as_ref().expect("selector should be Some");
        assert_eq!(selector.name, Some("eth0".to_string()));
    }

    #[test]
    fn test_parse_dhcpv4_policy_state_is_none() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        assert!(policies[0].state.is_none());
    }

    #[test]
    fn test_parse_dhcpv4_policy_states_is_none() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        assert!(policies[0].states.is_none());
    }

    #[test]
    fn test_factory_type_dhcpv4_deserializes_via_policy_yaml() {
        let policies = parse_policy_yaml(DHCPV4_POLICY_YAML).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Dhcpv4);
    }

    #[test]
    fn test_factory_type_static_deserializes_via_policy_yaml() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_parse_default_priority_is_100_when_field_absent() {
        let policies = parse_policy_yaml(NO_PRIORITY_YAML).unwrap();
        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_parse_multidoc_yaml_returns_two_policies() {
        let policies = parse_policy_yaml(MULTI_DOC_YAML).unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_parse_multidoc_yaml_first_policy_name_and_factory() {
        let policies = parse_policy_yaml(MULTI_DOC_YAML).unwrap();
        assert_eq!(policies[0].name, "eth0-static");
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_parse_multidoc_yaml_second_policy_name_and_factory() {
        let policies = parse_policy_yaml(MULTI_DOC_YAML).unwrap();
        assert_eq!(policies[1].name, "eth0-dhcp");
        assert_eq!(policies[1].factory_type, FactoryType::Dhcpv4);
    }

    #[test]
    fn test_parse_multidoc_yaml_first_policy_has_state_second_does_not() {
        let policies = parse_policy_yaml(MULTI_DOC_YAML).unwrap();
        assert!(policies[0].state.is_some());
        assert!(policies[1].state.is_none());
    }

    #[test]
    fn test_parse_trailing_separator_skipped() {
        let yaml = "\
kind: policy
name: eth0-static
factory: static
state:
  type: ethernet
  name: eth0
---
";
        let policies = parse_policy_yaml(yaml).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_parse_unknown_factory_type_returns_error() {
        let yaml = "\
kind: policy
name: test
factory: magic
state:
  type: ethernet
  name: eth0
";
        let result = parse_policy_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PolicyError::UnknownFactory { .. }));
    }

    #[test]
    fn test_parse_missing_name_returns_error() {
        let yaml = "\
kind: policy
factory: static
state:
  type: ethernet
  name: eth0
";
        let result = parse_policy_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PolicyError::MissingField { .. }));
    }

    #[test]
    fn test_parse_kind_state_returns_unsupported_kind_error() {
        let yaml = "\
kind: state
type: ethernet
name: eth0
";
        let result = parse_policy_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PolicyError::UnsupportedKind { .. }));
    }

    #[test]
    fn test_parse_no_kind_returns_unsupported_kind_error() {
        let yaml = "\
type: ethernet
name: eth0
mtu: 1500
";
        let result = parse_policy_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PolicyError::UnsupportedKind { .. }));
    }

    // ── Feature: Policy-level selector field ──────────────────────────────────

    const STATIC_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML: &str = "\
kind: policy
name: eth0-static
factory: static
priority: 150
selector:
  name: eth0
state:
  type: ethernet
  mtu: 1500
";

    const MULTI_ENTITY_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML: &str = "\
kind: policy
name: server-network
factory: static
priority: 100
selector:
  name: eth0
states:
  - type: ethernet
    mtu: 1500
  - type: dns
    scope: global
    servers:
      - 10.0.1.2
";

    // ── Criterion: "selector has name 'eth0'" (static policy) ────────────────

    #[test]
    fn test_parse_static_policy_selector_has_name_eth0() {
        let policies = parse_policy_yaml(STATIC_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML).unwrap();
        let selector = policies[0]
            .selector
            .as_ref()
            .expect("policy selector should be Some");
        assert_eq!(selector.name, Some("eth0".to_string()));
    }

    // ── Criterion: "state is Some with field mtu=1500" ────────────────────────

    #[test]
    fn test_parse_static_policy_state_field_mtu_is_1500() {
        let policies = parse_policy_yaml(STATIC_POLICY_YAML).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    #[test]
    fn test_parse_static_policy_with_top_level_selector_state_field_mtu_is_1500() {
        let policies = parse_policy_yaml(STATIC_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    // ── Criterion: "the selector has name 'eth0'" (multi-entity policy) ──────

    #[test]
    fn test_parse_multi_entity_policy_with_top_level_selector_name_is_eth0() {
        let policies =
            parse_policy_yaml(MULTI_ENTITY_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML).unwrap();
        let selector = policies[0]
            .selector
            .as_ref()
            .expect("policy selector should be Some");
        assert_eq!(selector.name, Some("eth0".to_string()));
    }

    #[test]
    fn test_parse_multi_entity_policy_with_top_level_selector_two_states() {
        let policies =
            parse_policy_yaml(MULTI_ENTITY_POLICY_WITH_TOP_LEVEL_SELECTOR_YAML).unwrap();
        let states = policies[0].states.as_ref().expect("states should be Some");
        assert_eq!(states.len(), 2);
    }

    // ── Spec YAML format: state without 'type:' ───────────────────────────────

    #[test]
    fn test_parse_static_policy_spec_yaml_format_typeless_state() {
        let yaml = "\
kind: policy
name: eth0-static
factory: static
priority: 150
selector:
  name: eth0
state:
  mtu: 1500
";
        let result = parse_policy_yaml(yaml);
        assert!(
            result.is_ok(),
            "parsing spec YAML format (state without 'type:') should succeed, got: {:?}",
            result.err()
        );
        let policies = result.unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "eth0-static");
        assert_eq!(policies[0].factory_type, FactoryType::Static);
        assert_eq!(policies[0].priority, 150);
        let selector = policies[0]
            .selector
            .as_ref()
            .expect("selector should be Some");
        assert_eq!(selector.name, Some("eth0".to_string()));
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    #[test]
    fn test_parse_multi_entity_policy_spec_yaml_format_typeless_states() {
        let yaml = "\
kind: policy
name: server-network
factory: static
priority: 100
selector:
  name: eth0
states:
  - mtu: 1500
    addresses:
      - 10.0.1.50/24
  - dns_servers:
      - 10.0.1.2
";
        let result = parse_policy_yaml(yaml);
        assert!(
            result.is_ok(),
            "parsing spec YAML format (states without 'type:') should succeed, got: {:?}",
            result.err()
        );
        let policies = result.unwrap();
        assert_eq!(policies.len(), 1);
        let selector = policies[0]
            .selector
            .as_ref()
            .expect("selector should be Some");
        assert_eq!(selector.name, Some("eth0".to_string()));
        let states = policies[0].states.as_ref().expect("states should be Some");
        assert_eq!(states.len(), 2);
    }
}

// ── Loader tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod loader_tests {
    use super::*;
    use netfyr_state::Value;
    use std::fs;
    use std::path::PathBuf;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Write `content` to `filename` inside `dir` and return the full path.
    fn write_file(dir: &tempfile::TempDir, filename: &str, content: &str) -> PathBuf {
        let path = dir.path().join(filename);
        fs::write(&path, content).unwrap();
        path
    }

    // ── Scenario: Single bare state file is wrapped into a policy ─────────────

    #[test]
    fn test_bare_state_single_doc_returns_one_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_bare_state_single_doc_policy_name_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    #[test]
    fn test_bare_state_single_doc_factory_type_is_static() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_bare_state_single_doc_priority_is_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_bare_state_single_doc_state_entity_type_is_ethernet() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        // entity_type is determined by the backend, not the inline state YAML
        assert!(state.entity_type.is_empty());
    }

    #[test]
    fn test_bare_state_single_doc_state_has_mtu_1500() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    /// Single-doc file must NOT get a numeric suffix ("eth0", not "eth0-1").
    #[test]
    fn test_bare_state_single_doc_name_has_no_numeric_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
        assert!(
            !policies[0].name.ends_with("-1"),
            "single-document files must not produce a '-1' suffix"
        );
    }

    // ── Scenario: Explicit kind: state is treated same as bare state ────────

    #[test]
    fn test_explicit_kind_state_returns_one_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_explicit_kind_state_policy_name_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    #[test]
    fn test_explicit_kind_state_factory_type_is_static() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_explicit_kind_state_priority_is_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_explicit_kind_state_state_entity_type_is_ethernet() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        // entity_type is determined by the backend, not the inline state YAML
        assert!(state.entity_type.is_empty());
    }

    #[test]
    fn test_explicit_kind_state_state_has_mtu_1500() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "kind: state\nselector:\n  name: eth0\nmtu: 1500\n",
        );
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    // ── Scenario: Multi-document bare state file produces numbered policies ──

    #[test]
    fn test_multi_doc_bare_state_returns_two_policies() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "interfaces.yaml",
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_multi_doc_bare_state_first_policy_name_has_suffix_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "interfaces.yaml",
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "interfaces-1");
    }

    #[test]
    fn test_multi_doc_bare_state_second_policy_name_has_suffix_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "interfaces.yaml",
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[1].name, "interfaces-2");
    }

    #[test]
    fn test_multi_doc_bare_state_both_policies_have_static_factory_and_priority_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "interfaces.yaml",
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        );
        let policies = load_policy_file(&path).unwrap();
        for p in &policies {
            assert_eq!(p.factory_type, FactoryType::Static);
            assert_eq!(p.priority, 100);
        }
    }

    // ── Scenario: kind: policy documents are not wrapped ───────────────────

    #[test]
    fn test_explicit_kind_policy_returns_one_policy_with_declared_name() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
        // Name comes from the document, not the filename.
        assert_eq!(policies[0].name, "custom-policy");
    }

    #[test]
    fn test_explicit_kind_policy_priority_is_declared_value_not_default() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        // Priority 200, not the bare-state default of 100.
        assert_eq!(policies[0].priority, 200);
    }

    #[test]
    fn test_explicit_kind_policy_factory_type_is_static() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    // ── Scenario: Mixed file with bare state and explicit policy ────────────

    #[test]
    fn test_mixed_file_returns_two_policies() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: eth0\nmtu: 1500\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_mixed_file_first_is_wrapped_bare_state_named_with_suffix_1_and_priority_100() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: eth0\nmtu: 1500\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "mixed-1");
        assert_eq!(policies[0].priority, 100);
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_mixed_file_second_is_explicit_policy_with_declared_name_and_priority_200() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: eth0\nmtu: 1500\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[1].name, "eth0-override");
        assert_eq!(policies[1].priority, 200);
    }

    // ── Scenario: Info log is emitted for wrapped bare states ───────────────

    #[test]
    fn test_bare_state_load_succeeds_implying_info_log_path_was_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let result = load_policy_file(&path);
        assert!(result.is_ok(), "loading a bare state file should succeed");
    }

    // ── Scenario: Policy name derived from filename without extension ───────

    #[test]
    fn test_policy_name_derived_from_yaml_extension_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    #[test]
    fn test_policy_name_derived_from_yml_extension_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "bond0-vlan100.yml", "selector:\n  name: bond0.100\nmtu: 9000\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "bond0-vlan100");
    }

    // ── Scenario: Load all policies from a directory ────────────────────────

    #[test]
    fn test_load_policy_dir_three_files_returns_policy_set_with_three_entries() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: main\nservers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 3);
    }

    #[test]
    fn test_load_policy_dir_contains_eth0_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: main\nservers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("eth0").is_some(), "policy set should contain 'eth0'");
    }

    #[test]
    fn test_load_policy_dir_contains_dns_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: main\nservers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("dns").is_some(), "policy set should contain 'dns'");
    }

    #[test]
    fn test_load_policy_dir_contains_custom_explicit_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: main\nservers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("custom").is_some(), "policy set should contain 'custom'");
    }

    // ── Scenario: Duplicate policy names across files are rejected ──────────

    #[test]
    fn test_load_policy_dir_duplicate_name_returns_duplicate_error() {
        let dir = tempfile::tempdir().unwrap();
        // eth0.yaml derives name "eth0" from filename.
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        // also-eth0.yaml uses an explicit kind: policy with name "eth0".
        write_file(
            &dir,
            "also-eth0.yaml",
            "kind: policy\nname: eth0\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let result = load_policy_dir(dir.path());
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::DuplicatePolicyName { .. }),
            "expected DuplicatePolicyName error"
        );
    }

    #[test]
    fn test_load_policy_dir_duplicate_name_error_identifies_conflicting_policy_name() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(
            &dir,
            "also-eth0.yaml",
            "kind: policy\nname: eth0\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        match load_policy_dir(dir.path()).unwrap_err() {
            LoaderError::DuplicatePolicyName { name, .. } => {
                assert_eq!(name, "eth0");
            }
            other => panic!("expected DuplicatePolicyName, got {:?}", other),
        }
    }

    // ── Scenario: Hidden files are skipped during directory loading ─────────

    #[test]
    fn test_load_policy_dir_skips_hidden_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        // .backup.yaml is hidden; same entity as eth0 — would conflict if loaded.
        write_file(&dir, ".backup.yaml", "selector:\n  name: eth0\nmtu: 9000\n");
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 1);
        assert!(policy_set.get("eth0").is_some());
    }

    #[test]
    fn test_load_policy_dir_hidden_file_does_not_cause_duplicate_error() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, ".backup.yaml", "selector:\n  name: eth0\nmtu: 9000\n");
        // If the hidden file were loaded it would trigger a DuplicatePolicyName error.
        let result = load_policy_dir(dir.path());
        assert!(result.is_ok(), "hidden files must be skipped, not cause errors");
    }

    // ── Scenario: Unknown kind value produces an error ──────────────────────

    #[test]
    fn test_unknown_kind_value_returns_unknown_kind_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "bad.yaml", "kind: unknown\nname: something\n");
        let result = load_policy_file(&path);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::UnknownKind { .. }),
            "expected UnknownKind error for 'kind: unknown'"
        );
    }

    #[test]
    fn test_unknown_kind_value_error_identifies_the_kind_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "bad.yaml", "kind: unknown\nname: something\n");
        match load_policy_file(&path).unwrap_err() {
            LoaderError::UnknownKind { kind, .. } => {
                assert_eq!(kind, "unknown");
            }
            other => panic!("expected UnknownKind, got {:?}", other),
        }
    }

    // ── Additional edge cases ───────────────────────────────────────────────

    /// Non-YAML files in a directory are silently skipped.
    #[test]
    fn test_load_policy_dir_skips_non_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "README.txt", "this is not a YAML policy file");
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 1);
    }

    /// Trailing `---` separators in a file produce null documents that are
    /// silently skipped and do not affect document numbering.
    #[test]
    fn test_trailing_separator_skipped_single_doc_still_has_no_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            &dir,
            "eth0.yaml",
            "selector:\n  name: eth0\nmtu: 1500\n---\n",
        );
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "eth0");
    }

    /// The `.yml` extension (not just `.yaml`) is processed by load_policy_dir.
    #[test]
    fn test_load_policy_dir_processes_yml_extension() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 1);
        assert!(policy_set.get("eth0").is_some());
    }

    /// An empty directory produces an empty PolicySet without error.
    #[test]
    fn test_load_policy_dir_empty_directory_returns_empty_policy_set() {
        let dir = tempfile::tempdir().unwrap();
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.is_empty());
    }

    // ── SPEC-008: Bare state shorthand — selector sub-mapping format ───────────
    //
    // These tests exercise the SPEC-008 acceptance criteria using the NEW bare
    // state format where `selector:` is a sub-mapping at the top level, and all
    // other top-level keys (except `kind`) are state fields.
    //
    // The existing tests above this section use the OLD flat format (top-level
    // `type:` and `name:` keys) which the current implementation rejects with
    // LoaderError::MissingSelector. Those tests are left in place for the verify
    // phase to reconcile.

    // ── Scenario: Single bare state file is wrapped into a policy ─────────────

    #[test]
    fn test_bare_state_selector_submapping_returns_one_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_bare_state_selector_submapping_policy_name_is_eth0() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    #[test]
    fn test_bare_state_selector_submapping_factory_type_is_static() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_bare_state_selector_submapping_priority_is_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_bare_state_selector_submapping_policy_selector_has_name_eth0() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let sel = policies[0].selector.as_ref().expect("policy.selector should be Some");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    #[test]
    fn test_bare_state_selector_submapping_state_has_mtu_1500() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("policy.state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    #[test]
    fn test_bare_state_selector_submapping_state_does_not_contain_selector_as_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("policy.state should be Some");
        assert!(
            !state.fields.contains_key("selector"),
            "selector sub-mapping must not appear as a state field"
        );
    }

    #[test]
    fn test_bare_state_selector_submapping_single_doc_no_numeric_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
        assert!(
            !policies[0].name.ends_with("-1"),
            "single-document file must not produce a '-1' suffix"
        );
    }

    // ── Scenario: Explicit kind: state is treated same as bare state ───────────

    #[test]
    fn test_kind_state_selector_submapping_returns_one_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "kind: state\nselector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
    }

    #[test]
    fn test_kind_state_selector_submapping_policy_name_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "kind: state\nselector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    #[test]
    fn test_kind_state_selector_submapping_factory_is_static_priority_100() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "kind: state\nselector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].factory_type, FactoryType::Static);
        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_kind_state_selector_submapping_policy_selector_name_is_eth0() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "kind: state\nselector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let sel = policies[0].selector.as_ref().expect("policy.selector should be Some");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    #[test]
    fn test_kind_state_selector_submapping_state_has_mtu_1500() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "kind: state\nselector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("policy.state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    // ── Scenario: Multi-document bare state file produces numbered policies ────

    #[test]
    fn test_multi_doc_selector_submapping_returns_two_policies() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_multi_doc_selector_submapping_first_policy_named_interfaces_1() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "interfaces-1");
    }

    #[test]
    fn test_multi_doc_selector_submapping_second_policy_named_interfaces_2() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[1].name, "interfaces-2");
    }

    #[test]
    fn test_multi_doc_selector_submapping_first_selector_name_is_eth0() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let sel = policies[0].selector.as_ref().expect("selector should be Some");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    #[test]
    fn test_multi_doc_selector_submapping_second_selector_name_is_eth1() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let sel = policies[1].selector.as_ref().expect("selector should be Some");
        assert_eq!(sel.name, Some("eth1".to_string()));
    }

    #[test]
    fn test_multi_doc_selector_submapping_both_have_static_factory_and_priority_100() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        for p in &policies {
            assert_eq!(p.factory_type, FactoryType::Static);
            assert_eq!(p.priority, 100);
        }
    }

    #[test]
    fn test_multi_doc_selector_submapping_first_state_has_mtu_1500() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(1500));
    }

    #[test]
    fn test_multi_doc_selector_submapping_second_state_has_mtu_9000() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n";
        let path = write_file(&dir, "interfaces.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let state = policies[1].state.as_ref().expect("state should be Some");
        assert_eq!(state.fields["mtu"].value, Value::U64(9000));
    }

    // ── Scenario: kind: policy documents are not wrapped ──────────────────────

    #[test]
    fn test_kind_policy_uses_declared_name_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "custom-policy");
    }

    #[test]
    fn test_kind_policy_priority_is_declared_value_200_not_default_100() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].priority, 200);
    }

    #[test]
    fn test_kind_policy_selector_name_is_eth0() {
        let dir = tempfile::tempdir().unwrap();
        let content = "kind: policy\nname: custom-policy\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "custom.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let sel = policies[0].selector.as_ref().expect("selector should be Some");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    // ── Scenario: Mixed file with bare state and explicit policy ──────────────

    #[test]
    fn test_mixed_file_selector_submapping_returns_two_policies() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 2);
    }

    #[test]
    fn test_mixed_file_first_is_wrapped_bare_state_named_mixed_1_priority_100() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "mixed-1");
        assert_eq!(policies[0].priority, 100);
        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_mixed_file_second_is_explicit_policy_named_eth0_override_priority_200() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n---\n\
                       kind: policy\nname: eth0-override\nfactory: static\npriority: 200\n\
                       selector:\n  name: eth0\nstate:\n  mtu: 9000\n";
        let path = write_file(&dir, "mixed.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[1].name, "eth0-override");
        assert_eq!(policies[1].priority, 200);
    }

    // ── Scenario: Info log is emitted for wrapped bare states ─────────────────
    // Direct log assertion is impractical in unit tests; we verify the function
    // succeeds, which requires the tracing::info! code path to be reached.

    #[test]
    fn test_bare_state_selector_submapping_load_succeeds_implying_log_path_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let result = load_policy_file(&path);
        assert!(result.is_ok(), "loading a bare state with selector sub-mapping must succeed");
    }

    // ── Scenario: Policy name derived from filename without extension ──────────

    #[test]
    fn test_policy_name_derived_from_yml_filename_bond0_vlan100() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "bond0-vlan100.yml", "selector:\n  name: bond0.100\nmtu: 9000\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "bond0-vlan100");
    }

    #[test]
    fn test_policy_name_derived_from_yaml_filename_selector_submapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies[0].name, "eth0");
    }

    // ── Scenario: Load all policies from a directory ──────────────────────────

    #[test]
    fn test_load_policy_dir_selector_submapping_three_files_returns_three_policies() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 3);
    }

    #[test]
    fn test_load_policy_dir_selector_submapping_contains_eth0() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("eth0").is_some(), "policy set should contain 'eth0'");
    }

    #[test]
    fn test_load_policy_dir_selector_submapping_contains_dns() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("dns").is_some(), "policy set should contain 'dns'");
    }

    #[test]
    fn test_load_policy_dir_selector_submapping_contains_custom() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, "dns.yaml", "selector:\n  name: dns-main\ndns_servers:\n  - 10.0.1.2\n");
        write_file(
            &dir,
            "custom.yaml",
            "kind: policy\nname: custom\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert!(policy_set.get("custom").is_some(), "policy set should contain 'custom'");
    }

    // ── Scenario: Duplicate policy names across files are rejected ─────────────

    #[test]
    fn test_load_policy_dir_duplicate_name_selector_submapping_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // eth0.yaml derives name "eth0" from filename
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        // also-eth0.yaml explicitly declares name "eth0"
        write_file(
            &dir,
            "also-eth0.yaml",
            "kind: policy\nname: eth0\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        let result = load_policy_dir(dir.path());
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::DuplicatePolicyName { .. }),
            "expected DuplicatePolicyName error"
        );
    }

    #[test]
    fn test_load_policy_dir_duplicate_name_selector_submapping_error_identifies_eth0() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(
            &dir,
            "also-eth0.yaml",
            "kind: policy\nname: eth0\nfactory: static\n\
             selector:\n  name: eth1\nstate:\n  mtu: 9000\n",
        );
        match load_policy_dir(dir.path()).unwrap_err() {
            LoaderError::DuplicatePolicyName { name, .. } => {
                assert_eq!(name, "eth0");
            }
            other => panic!("expected DuplicatePolicyName, got {:?}", other),
        }
    }

    // ── Scenario: Hidden files are skipped during directory loading ────────────

    #[test]
    fn test_load_policy_dir_skips_hidden_selector_submapping_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        // .backup.yaml is hidden; would conflict if loaded (same derived name "eth0")
        write_file(&dir, ".backup.yaml", "selector:\n  name: eth0\nmtu: 9000\n");
        let policy_set = load_policy_dir(dir.path()).unwrap();
        assert_eq!(policy_set.len(), 1);
        assert!(policy_set.get("eth0").is_some());
    }

    #[test]
    fn test_load_policy_dir_hidden_selector_file_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n");
        write_file(&dir, ".backup.yaml", "selector:\n  name: eth0\nmtu: 9000\n");
        // If .backup.yaml were loaded it would trigger DuplicatePolicyName
        let result = load_policy_dir(dir.path());
        assert!(result.is_ok(), "hidden files must be skipped, not cause errors");
    }

    // ── Scenario: Unknown kind value produces an error ─────────────────────────

    #[test]
    fn test_unknown_kind_with_selector_submapping_returns_unknown_kind_error() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "bad.yaml", "kind: unknown\nselector:\n  name: eth0\nmtu: 1500\n");
        let result = load_policy_file(&path);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::UnknownKind { .. }),
            "expected UnknownKind error"
        );
    }

    #[test]
    fn test_unknown_kind_error_message_contains_unknown_kind_string() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "bad.yaml", "kind: unknown\nselector:\n  name: eth0\n");
        match load_policy_file(&path).unwrap_err() {
            LoaderError::UnknownKind { kind, .. } => {
                assert_eq!(kind, "unknown");
            }
            other => panic!("expected UnknownKind, got {:?}", other),
        }
    }

    // ── Additional criterion: selector sub-mapping is required ─────────────────

    #[test]
    fn test_bare_state_without_selector_submapping_returns_missing_selector_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "mtu: 1500\n");
        let result = load_policy_file(&path);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::MissingSelector { .. }),
            "bare state without selector: sub-mapping must return MissingSelector error"
        );
    }

    #[test]
    fn test_kind_state_without_selector_submapping_returns_missing_selector_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "eth0.yaml", "kind: state\nmtu: 1500\n");
        let result = load_policy_file(&path);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), LoaderError::MissingSelector { .. }),
            "kind: state without selector: sub-mapping must return MissingSelector error"
        );
    }

    // ── Trailing separator skipped (selector sub-mapping format) ──────────────

    #[test]
    fn test_trailing_separator_selector_submapping_single_doc_no_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "eth0.yaml", "selector:\n  name: eth0\nmtu: 1500\n---\n");
        let policies = load_policy_file(&path).unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "eth0");
    }

    // ── Addresses list parsed correctly in selector sub-mapping format ─────────

    #[test]
    fn test_bare_state_selector_submapping_addresses_list_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let content = "selector:\n  name: eth0\nmtu: 1500\naddresses:\n  - 10.0.1.50/24\n";
        let path = write_file(&dir, "eth0.yaml", content);
        let policies = load_policy_file(&path).unwrap();
        let state = policies[0].state.as_ref().expect("state should be Some");
        let addrs = state.fields["addresses"].value.as_list().expect("addresses should be a list");
        assert_eq!(addrs.len(), 1);
    }
}
