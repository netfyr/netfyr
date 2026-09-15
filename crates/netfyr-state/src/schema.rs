//! Schema validation for [`crate::State`] values.
//!
//! The embedded JSON-Schema fragments in `schemas/` are the authoritative
//! validation definition of the data model: every field a fragment defines,
//! its type, its legal range, and whether a policy may set it (the
//! `x-netfyr-writable` extension). Validating a state before any
//! system change turns typos (`mtt` for `mtu`) and read-only writes into
//! clear, collected errors instead of confusing backend failures or
//! silent no-ops.
//!
//! Fragments are independent and apply when their trigger key is present
//! in `fields`: `base` always, `ipv4` when an `ipv4` sub-object is
//! present, `ethernet` when an `ethernet` sub-object is present.
//! [`SchemaRegistry::validate`] accepts read-only fields (query results
//! carry them); [`SchemaRegistry::validate_writable`] rejects them
//! (policies must not set them). Both modes reject fields the fragments do
//! not define. Every violation is collected in a deterministic order:
//! fragments in registration order, fields in `fields` insertion order,
//! missing-required errors last. The returned vector is empty exactly when
//! the state is valid.
//!
//! # Dialect
//!
//! The fragments use a documented subset of JSON Schema (draft 2020-12):
//! `type`, `format`, `enum`,
//! `minimum`/`maximum`, `properties`, `required`, `items`, and
//! `additionalProperties` (absent means closed), plus the
//! `x-netfyr-writable` extension (absent means read-only). Unknown
//! keywords are ignored except `format`, whose values must be supported by
//! this module; the pinned field-set and behavior tests backstop typos in
//! the fragments themselves.

use std::fmt;
use std::net::IpAddr;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::Deserialize;

use crate::state::State;
use crate::value::Value;

/// The schema validation mode used by direct YAML decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeMode {
    /// Query output accepts schema-defined read-only properties and derives
    /// its match metadata from the properties returned by the kernel.
    Query,
    /// Policy input requires an explicit match and permits writable fields
    /// only.
    Policy,
}

/// The `base` fragment: fields common to every network device, always
/// validated.
const BASE_JSON: &str = include_str!("../schemas/base.json");
/// The `ipv4` fragment: validated when `fields` carries an `ipv4`
/// sub-object.
const IPV4_JSON: &str = include_str!("../schemas/ipv4.json");
/// The `ethernet` fragment: validated when `fields` carries an `ethernet`
/// sub-object.
const ETHERNET_JSON: &str = include_str!("../schemas/ethernet.json");

/// The type vocabulary of the schema dialect, as the walker sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    /// A string scalar.
    String,
    /// A boolean.
    Boolean,
    /// An integer (positive or negative).
    Integer,
    /// An object (a nested field mapping).
    Object,
    /// An array (an ordered list).
    Array,
    /// A bare IP address.
    IpAddress,
    /// An IP network: an address with a prefix length (CIDR).
    IpNetwork,
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FieldType::String => "string",
            FieldType::Boolean => "boolean",
            FieldType::Integer => "integer",
            FieldType::Object => "object",
            FieldType::Array => "array",
            FieldType::IpAddress => "ip",
            FieldType::IpNetwork => "ip-network",
        })
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "string" => Ok(FieldType::String),
            "boolean" => Ok(FieldType::Boolean),
            "integer" => Ok(FieldType::Integer),
            "object" => Ok(FieldType::Object),
            "array" => Ok(FieldType::Array),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["string", "boolean", "integer", "object", "array"],
            )),
        }
    }
}

/// Formats recognized by the embedded schema dialect.
#[derive(Clone, Copy, Debug, Deserialize)]
enum SchemaFormat {
    #[serde(rename = "ipv4-cidr")]
    Ipv4Cidr,
}

/// Parse an `address/prefix` CIDR while retaining the address host bits.
/// This helper is intentionally private to schema format handling: bare YAML
/// strings must never be interpreted as networks without this context.
fn parse_ip_network(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let addr = addr.parse::<IpAddr>().ok()?;
    let prefix = prefix.parse::<u8>().ok()?;
    let max_prefix = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (prefix <= max_prefix).then_some((addr, prefix))
}

/// Metadata for one field of a schema fragment: whether a policy may set
/// it and its type in the dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    /// Whether the field may be set in a policy
    /// (`x-netfyr-writable`; read-only otherwise).
    pub writable: bool,
    /// The field's type in the schema dialect.
    pub r#type: FieldType,
}

/// One violation found while validating a state against the fragments.
///
/// `path` is from the state root, dot-separated, with zero-based `[n]`
/// array indices (`mtu`, `ipv4.addresses[0].ip`); `fragment` names the
/// fragment that reported the error. Validation collects every violation
/// it finds, so a state is valid exactly when the returned vector is
/// empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    /// The field is not defined in its fragment (a typo, or a field the
    /// model does not cover yet).
    UnknownField {
        /// The fragment that rejected the field.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
    },
    /// The field is read-only and was seen in policy input
    /// (`validate_writable` mode only).
    ReadOnlyField {
        /// The fragment that defines the field read-only.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
    },
    /// An integer value lies outside the fragment's `minimum`/`maximum`.
    /// The value itself is not stored: the message names the field and
    /// the legal range.
    OutOfRange {
        /// The fragment that defines the range.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
        /// The fragment's `minimum`, if any.
        min: Option<i64>,
        /// The fragment's `maximum`, if any.
        max: Option<i64>,
    },
    /// The value's type does not match the fragment's `type`.
    WrongType {
        /// The fragment that defines the field.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
        /// The type the fragment requires.
        expected: FieldType,
        /// The type the value actually has.
        found: FieldType,
    },
    /// A field marked `required` in the fragment is absent.
    MissingField {
        /// The fragment that requires the field.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
    },
    /// A string value is not one of the fragment's `enum` values.
    EnumViolation {
        /// The fragment that lists the allowed values.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
        /// The values the fragment allows.
        allowed: Vec<String>,
    },
    /// A value does not satisfy a field's semantic format.
    InvalidFormat {
        /// The fragment that defines the field.
        fragment: &'static str,
        /// The field's path from the state root.
        path: String,
        /// The required format.
        expected: &'static str,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::UnknownField { fragment, path } => {
                write!(f, "unknown field '{path}' in the '{fragment}' fragment")
            }
            ValidationError::ReadOnlyField { fragment, path } => write!(
                f,
                "field '{path}' is read-only and cannot be set ('{fragment}' \
                fragment)"
            ),
            ValidationError::OutOfRange {
                fragment,
                path,
                min,
                max,
            } => {
                let range = match (min, max) {
                    (Some(low), Some(high)) => format!("between {low} and {high}"),
                    (Some(low), None) => format!("at least {low}"),
                    (None, Some(high)) => format!("at most {high}"),
                    (None, None) => "the allowed range".to_string(),
                };
                write!(
                    f,
                    "value of '{path}' is out of range: expected {range} \
                     ('{fragment}' fragment)"
                )
            }
            ValidationError::WrongType {
                fragment,
                path,
                expected,
                found,
            } => write!(
                f,
                "field '{path}' has the wrong type: expected {expected}, found {found} \
                 ('{fragment}' fragment)"
            ),
            ValidationError::MissingField { fragment, path } => {
                write!(f, "field '{path}' is required ('{fragment}' fragment)")
            }
            ValidationError::EnumViolation {
                fragment,
                path,
                allowed,
            } => write!(
                f,
                "field '{path}' must be one of: {} ('{fragment}' fragment)",
                allowed.join(", ")
            ),
            ValidationError::InvalidFormat {
                fragment,
                path,
                expected,
            } => write!(
                f,
                "field '{path}' must use {expected} format ('{fragment}' fragment)"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// One node of the parsed schema dialect. Recursive: object nodes carry
/// `properties`, array nodes carry `items`.
#[derive(Deserialize)]
struct SchemaNode {
    /// The dialect `type` keyword.
    r#type: Option<FieldType>,
    /// The optional JSON Schema semantic format annotation.
    format: Option<SchemaFormat>,
    /// The `x-netfyr-writable` extension: whether a policy may set the
    /// field. Absent means read-only.
    #[serde(rename = "x-netfyr-writable", default)]
    writable: bool,
    /// The `minimum` bound for integers, if any.
    minimum: Option<i64>,
    /// The `maximum` bound for integers, if any.
    maximum: Option<i64>,
    /// The allowed string values for `enum`, if any.
    r#enum: Option<Vec<String>>,
    /// The child fields of an object node.
    properties: Option<IndexMap<String, SchemaNode>>,
    /// The fields that must be present in an object node.
    required: Option<Vec<String>>,
    /// The element schema of an array node.
    items: Option<Box<SchemaNode>>,
    /// `additionalProperties`; absent means closed (unknown keys are
    /// rejected).
    #[serde(rename = "additionalProperties")]
    additional_properties: Option<bool>,
    /// A technology fragment can declare the device type it implies when
    /// query YAML does not carry an explicit top-level `type`.
    #[serde(rename = "x-netfyr-device-type")]
    implied_device_type: Option<String>,
}

/// An opaque schema node used by the YAML codec while it recursively decodes
/// a value. This keeps the parsed JSON schema private to this module.
#[derive(Clone, Copy)]
pub(crate) struct SchemaCursor<'a> {
    node: &'a SchemaNode,
}

impl<'a> SchemaCursor<'a> {
    pub(crate) fn property(self, key: &str) -> Option<Self> {
        self.node
            .properties
            .as_ref()?
            .get(key)
            .map(|node| Self { node })
    }

    pub(crate) fn items(self) -> Option<Self> {
        self.node.items.as_deref().map(|node| Self { node })
    }

    pub(crate) fn decode_string(self, text: &str) -> Value {
        match semantic_type(self.node).expect("schema nodes are validated at registration") {
            FieldType::IpNetwork => parse_ip_network(text)
                .map(Value::IpNetwork)
                // Preserve malformed text for the validator's InvalidFormat
                // diagnostic at this exact schema path.
                .unwrap_or_else(|| Value::String(text.to_string())),
            _ => Value::String(text.to_string()),
        }
    }
}

/// One schema fragment: its name, the key that triggers it (absent for
/// the always-applied `base` fragment), the parsed root node, and the
/// flat field map derived from the root once at construction.
struct Fragment {
    /// The fragment name as used in [`ValidationError`] and in the
    /// `field_info`/`fragment_fields` lookups.
    name: &'static str,
    /// The top-level `fields` key that applies this fragment; `None`
    /// applies it to every state.
    trigger_key: Option<&'static str>,
    /// The parsed fragment root.
    root: SchemaNode,
    /// Every field at every nesting level, keyed by dot path within the
    /// fragment (e.g. `addresses.ip`), in lexicographic order.
    fields: IndexMap<String, FieldInfo>,
}

/// Validates [`crate::State`] values against the embedded schema
/// fragments.
///
/// Construct via [`SchemaRegistry::new`] or [`Default`]; the embedded
/// fragments are compile-time constants, so construction cannot fail.
pub struct SchemaRegistry {
    fragments: Vec<Fragment>,
}

impl SchemaRegistry {
    /// Build the registry from the embedded fragments, in the order
    /// `base`, `ipv4`, `ethernet`.
    ///
    /// # Panics
    ///
    /// Panics if an embedded fragment is malformed.
    pub fn new() -> Self {
        let fragments = vec![
            Self::parse_fragment("base", BASE_JSON, None),
            Self::parse_fragment("ipv4", IPV4_JSON, Some("ipv4")),
            Self::parse_fragment("ethernet", ETHERNET_JSON, Some("ethernet")),
        ];
        Self { fragments }
    }

    /// Add a schema node below an existing fragment root.
    ///
    /// Missing intermediate properties are created as writable closed objects.
    /// This composes extension-owned schema into the same decoder and validator
    /// used by the built-in state fragments.
    ///
    /// Every node, including nested properties and array items, must declare a
    /// type compatible with its format. Invalid schemas, invalid paths, and
    /// conflicts return an error without modifying the registry. Paths below a
    /// fragment trigger must be added to that fragment, not to `base`.
    pub fn add_schema_node(
        &mut self,
        fragment: &str,
        path: &str,
        json: &str,
    ) -> Result<(), String> {
        let node: SchemaNode = serde_json::from_str(json)
            .map_err(|err| format!("schema node at '{path}' is malformed: {err}"))?;
        validate_schema_node(&node)
            .map_err(|err| format!("schema node at '{path}' is malformed: {err}"))?;
        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
            return Err(
                "schema node path must not be empty or contain empty components".to_string(),
            );
        }
        // Even a nested base path would create a top-level property that
        // shadows an existing fragment's decoder.
        let target_is_base = self
            .fragments
            .iter()
            .find(|f| f.name == fragment)
            .is_some_and(|f| f.trigger_key.is_none());
        if target_is_base {
            let collides = self
                .fragments
                .iter()
                .any(|f| f.trigger_key == Some(parts[0]));
            if collides {
                return Err(format!(
                    "base property '{}' collides with a fragment trigger key",
                    parts[0],
                ));
            }
        }

        let fragment = self
            .fragments
            .iter_mut()
            .find(|candidate| candidate.name == fragment)
            .ok_or_else(|| format!("unknown schema fragment '{fragment}'"))?;
        let mut parent = &mut fragment.root;
        for part in &parts[..parts.len() - 1] {
            parent = parent
                .properties
                .get_or_insert_with(IndexMap::new)
                .entry((*part).to_string())
                .or_insert_with(writable_object);
            if parent.r#type != Some(FieldType::Object) {
                return Err(format!("schema path component '{part}' is not an object"));
            }
        }
        let properties = parent.properties.get_or_insert_with(IndexMap::new);
        let name = parts
            .last()
            .expect("non-empty path was checked")
            .to_string();
        if properties.contains_key(&name) {
            return Err(format!("schema node '{path}' already exists"));
        }
        properties.insert(name, node);

        fragment.fields.clear();
        derive_fields(&fragment.root, "", &mut fragment.fields);
        fragment.fields.sort_keys();
        Ok(())
    }

    /// Decode YAML directly into model values using this registry's schema
    /// paths. The YAML codec is implemented separately to keep the YAML
    /// dependency isolated from the schema representation.
    pub fn from_yaml(&self, input: &str) -> Result<Vec<State>, crate::Error> {
        crate::yaml::from_yaml_with_registry(input, self)
    }

    /// Decode and validate YAML in the requested decode mode. This is the
    /// high-level codec entry point; [`Self::from_yaml`] remains available to
    /// callers that need a candidate state for separately collected errors.
    pub fn decode_yaml(&self, input: &str, mode: DecodeMode) -> Result<Vec<State>, crate::Error> {
        let mut states = self.from_yaml(input)?;
        let mut errors = Vec::new();

        for (state_index, state) in states.iter().enumerate() {
            match mode {
                DecodeMode::Query if !state.match_spec.is_empty() => {
                    return Err(crate::Error::UnexpectedMatch { state_index });
                }
                DecodeMode::Policy if state.match_spec.is_empty() => {
                    return Err(crate::Error::MissingMatch { state_index });
                }
                _ => {}
            }
            let validation_errors = match mode {
                DecodeMode::Query => self.validate(state),
                DecodeMode::Policy => self.validate_writable(state),
            };
            errors.extend(
                validation_errors
                    .into_iter()
                    .map(|error| crate::IndexedValidationError { state_index, error }),
            );
        }
        if !errors.is_empty() {
            return Err(crate::Error::Validation(errors));
        }

        if mode == DecodeMode::Query {
            for state in &mut states {
                state.match_spec.name = field_string(&state.fields, "name");
                state.match_spec.r#type =
                    (!state.device_type.is_empty()).then(|| state.device_type.clone());
                state.match_spec.driver = field_string(&state.fields, "driver");
                state.match_spec.mac = field_string(&state.fields, "mac");
                state.source = crate::Source::Kernel;
                state.priority = state.source.default_priority();
            }
        }
        Ok(states)
    }

    /// Encode query-form YAML using this registry's field routing.
    pub fn to_yaml_query(&self, states: &[State]) -> Result<String, crate::Error> {
        crate::yaml::to_yaml_query_with_registry(states, self)
    }

    /// Encode policy-form YAML using this registry's field routing.
    pub fn to_yaml_policy(&self, states: &[State]) -> Result<String, crate::Error> {
        crate::yaml::to_yaml_policy_with_registry(states, self)
    }

    /// Parse one embedded fragment and derive its flat field map.
    ///
    /// # Panics
    ///
    /// Panics on malformed JSON or an invalid schema node.
    fn parse_fragment(
        name: &'static str,
        json: &'static str,
        trigger_key: Option<&'static str>,
    ) -> Fragment {
        let root: SchemaNode = serde_json::from_str(json)
            .unwrap_or_else(|err| panic!("embedded schema fragment '{name}' is malformed: {err}"));
        validate_schema_node(&root)
            .unwrap_or_else(|err| panic!("embedded schema fragment '{name}' is malformed: {err}"));
        let mut fields = IndexMap::new();
        derive_fields(&root, "", &mut fields);
        // Serde fills `properties` in JSON document order; the field map
        // contract is lexicographic, so sort it once here.
        fields.sort_keys();
        Fragment {
            name,
            trigger_key,
            root,
            fields,
        }
    }

    /// Validate a state, accepting read-only fields (for query results).
    ///
    /// Returns every violation found; an empty vector means the state is
    /// valid.
    pub fn validate(&self, state: &State) -> Vec<ValidationError> {
        self.validate_inner(state, false)
    }

    /// Validate a state as policy input: read-only fields are rejected in
    /// addition to everything [`Self::validate`] checks.
    ///
    /// Returns every violation found; an empty vector means the state is
    /// valid.
    pub fn validate_writable(&self, state: &State) -> Vec<ValidationError> {
        self.validate_inner(state, true)
    }

    /// Look up one field of a fragment by its dot path within the
    /// fragment (e.g. `"addresses"` or `"addresses.ip"`).
    ///
    /// Returns `None` for an unknown fragment or field.
    pub fn field_info(&self, fragment: &str, field: &str) -> Option<FieldInfo> {
        self.fragments
            .iter()
            .find(|f| f.name == fragment)?
            .fields
            .get(field)
            .copied()
    }

    /// Look up the schema governing a top-level YAML state property. Base
    /// properties and registered fragment trigger keys use this one routing
    /// table, avoiding duplicated knowledge in the YAML codec.
    pub(crate) fn top_level_schema(&self, key: &str) -> Option<SchemaCursor<'_>> {
        let base = self
            .fragments
            .iter()
            .find(|fragment| fragment.trigger_key.is_none())
            .expect("registry has a base fragment");
        base.root
            .properties
            .as_ref()
            .and_then(|properties| properties.get(key))
            .or_else(|| {
                self.fragments
                    .iter()
                    .find(|fragment| fragment.trigger_key == Some(key))
                    .map(|fragment| &fragment.root)
            })
            .map(|node| SchemaCursor { node })
    }

    /// Return the device type implied by a registered technology fragment in
    /// the state. An explicit top-level `type` remains authoritative.
    pub(crate) fn implied_device_type(&self, fields: &IndexMap<String, Value>) -> Option<&str> {
        self.fragments
            .iter()
            .filter(|fragment| {
                fragment
                    .trigger_key
                    .is_some_and(|trigger| fields.contains_key(trigger))
            })
            .find_map(|fragment| fragment.root.implied_device_type.as_deref())
    }

    /// Every field of a fragment at every nesting level, keyed by dot
    /// path in lexicographic order.
    ///
    /// Returns `None` for an unknown fragment.
    pub fn fragment_fields(&self, fragment: &str) -> Option<Vec<(String, FieldInfo)>> {
        self.fragments.iter().find(|f| f.name == fragment).map(|f| {
            f.fields
                .iter()
                .map(|(path, info)| (path.clone(), *info))
                .collect()
        })
    }

    /// Remove schema-defined read-only fields from a query-derived state.
    /// Unknown fields are intentionally retained; callers must validate the
    /// state first, so they are reported instead of silently discarded.
    pub(crate) fn remove_read_only_fields(&self, state: &mut State) -> Vec<String> {
        let base = self
            .fragments
            .iter()
            .find(|fragment| fragment.trigger_key.is_none())
            .expect("registry has a base fragment");
        let mut removed = Vec::new();
        let keys: Vec<String> = state.fields.keys().cloned().collect();

        for key in keys {
            if let Some(fragment) = self
                .fragments
                .iter()
                .find(|fragment| fragment.trigger_key == Some(key.as_str()))
            {
                let mut empty = false;
                if let Some(Value::Map(value)) = state.fields.get_mut(&key) {
                    remove_read_only_from_map(&fragment.root, value, &key, &mut removed);
                    empty = value.is_empty();
                }
                if empty {
                    state.fields.shift_remove(&key);
                }
            } else if let Some(node) = base
                .root
                .properties
                .as_ref()
                .and_then(|props| props.get(&key))
            {
                remove_read_only_property(node, &mut state.fields, &key, "", &mut removed);
            }
        }
        removed
    }

    /// Shared validate body: apply each fragment whose trigger key is
    /// present and collect every violation.
    fn validate_inner(&self, state: &State, writable_only: bool) -> Vec<ValidationError> {
        // The base fragment must not flag other fragments' trigger keys
        // as unknown: an `ipv4:` / `ethernet:` sub-object is valid state.
        let trigger_keys: Vec<&str> = self
            .fragments
            .iter()
            .filter_map(|f| f.trigger_key)
            .collect();

        let mut errors = Vec::new();
        for fragment in &self.fragments {
            match fragment.trigger_key {
                None => {
                    walk_map(
                        &fragment.root,
                        &state.fields,
                        "",
                        &trigger_keys,
                        fragment.name,
                        writable_only,
                        &mut errors,
                    );
                    // `device_type` is structural metadata carried by the
                    // state model, not a field the policy writes to the
                    // device.  Validate its value (must be a known string)
                    // but never reject it as read-only: pass `false` for
                    // the writable check regardless of the caller's mode.
                    if !state.device_type.is_empty() {
                        let type_node = fragment
                            .root
                            .properties
                            .as_ref()
                            .and_then(|props| props.get("type"))
                            .expect("base schema has a type property");
                        walk_property(
                            type_node,
                            &Value::String(state.device_type.clone()),
                            "type",
                            fragment.name,
                            false,
                            &mut errors,
                        );
                    }
                }
                Some(key) => match state.fields.get(key) {
                    Some(Value::Map(inner)) => {
                        walk_map(
                            &fragment.root,
                            inner,
                            key,
                            &[],
                            fragment.name,
                            writable_only,
                            &mut errors,
                        );
                    }
                    Some(other) => errors.push(ValidationError::WrongType {
                        fragment: fragment.name,
                        path: key.to_string(),
                        expected: FieldType::Object,
                        found: value_type(other),
                    }),
                    None => {}
                },
            }
        }
        errors
    }
}

fn writable_object() -> SchemaNode {
    SchemaNode {
        r#type: Some(FieldType::Object),
        format: None,
        writable: true,
        minimum: None,
        maximum: None,
        r#enum: None,
        properties: Some(IndexMap::new()),
        required: None,
        items: None,
        additional_properties: Some(false),
        implied_device_type: None,
    }
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The registry used by the convenience YAML functions. Applications that
/// register another schema set should call the methods on their own registry.
pub(crate) static EMBEDDED_REGISTRY: LazyLock<SchemaRegistry> = LazyLock::new(SchemaRegistry::new);

/// Flatten a fragment's field hierarchy into `out`, keyed by dot path
/// within the fragment. Fields inside array elements continue from the
/// array's path prefix (`addresses.ip`, not `addresses[0].ip`): the flat
/// map describes the shape, and `[n]` belongs to validation-time paths.
fn derive_fields(node: &SchemaNode, prefix: &str, out: &mut IndexMap<String, FieldInfo>) {
    if let Some(properties) = &node.properties {
        for (name, prop) in properties {
            let path = join_path(prefix, name);
            out.insert(
                path.clone(),
                FieldInfo {
                    writable: prop.writable,
                    r#type: semantic_type(prop)
                        .expect("schema nodes are validated at registration"),
                },
            );
            derive_fields(prop, &path, out);
        }
    }
    if let Some(items) = &node.items {
        derive_fields(items, prefix, out);
    }
}

/// Get a node's type as exposed by the state model, including semantic
/// formats whose serialized JSON Schema type remains `string`.
fn semantic_type(node: &SchemaNode) -> Result<FieldType, &'static str> {
    match (node.r#type, node.format) {
        (Some(FieldType::String), Some(SchemaFormat::Ipv4Cidr)) => Ok(FieldType::IpNetwork),
        (Some(_), Some(SchemaFormat::Ipv4Cidr)) => {
            Err("ipv4-cidr schema format requires type string")
        }
        (Some(field_type), None) => Ok(field_type),
        (None, _) => Err("schema node without a 'type' keyword"),
    }
}

/// Check the complete schema subtree before it becomes part of a registry.
fn validate_schema_node(node: &SchemaNode) -> Result<(), String> {
    semantic_type(node)?;
    if let Some(properties) = &node.properties {
        for (name, property) in properties {
            validate_schema_node(property).map_err(|err| format!("property '{name}': {err}"))?;
        }
    }
    if let Some(items) = &node.items {
        validate_schema_node(items).map_err(|err| format!("array items: {err}"))?;
    }
    Ok(())
}

/// Validate one object: check each present key against the node's
/// `properties`, then report each absent `required` key.
///
/// `prefix` is the map's path from the state root (empty at the state
/// root); `ignore_keys` are top-level keys owned by another fragment
/// (empty below the state root).
fn walk_map(
    node: &SchemaNode,
    map: &IndexMap<String, Value>,
    prefix: &str,
    ignore_keys: &[&str],
    fragment: &'static str,
    writable_only: bool,
    errors: &mut Vec<ValidationError>,
) {
    // JSON Schema: `additionalProperties: false` (or absent) closes the
    // object so that unknown keys are rejected; `true` leaves it open.
    let closed = !node.additional_properties.unwrap_or(false);
    for (key, value) in map.iter() {
        // A trigger key belongs to another fragment; it is validated
        // there, not here.
        if ignore_keys.contains(&key.as_str()) {
            continue;
        }
        match node.properties.as_ref().and_then(|props| props.get(key)) {
            Some(prop) => {
                let path = join_path(prefix, key);
                walk_property(prop, value, &path, fragment, writable_only, errors);
            }
            None => {
                if closed {
                    errors.push(ValidationError::UnknownField {
                        fragment,
                        path: join_path(prefix, key),
                    });
                }
            }
        }
    }
    // Missing-required errors come after present-field errors, in the
    // fragment's `required` order.
    for required in node.required.iter().flatten() {
        if !map.contains_key(required) && !ignore_keys.contains(&required.as_str()) {
            errors.push(ValidationError::MissingField {
                fragment,
                path: join_path(prefix, required),
            });
        }
    }
}

/// Validate a property regardless of whether it came from a map or a model
/// projection such as `State::device_type`.
fn walk_property(
    node: &SchemaNode,
    value: &Value,
    path: &str,
    fragment: &'static str,
    writable_only: bool,
    errors: &mut Vec<ValidationError>,
) {
    if writable_only && !node.writable {
        errors.push(ValidationError::ReadOnlyField {
            fragment,
            path: path.to_string(),
        });
    }
    walk_node(node, value, path, fragment, writable_only, errors);
}

/// Validate one value against its schema node: the type test first, then
/// the type-specific checks (range, enum), then recursion into
/// containers.
fn walk_node(
    node: &SchemaNode,
    value: &Value,
    path: &str,
    fragment: &'static str,
    writable_only: bool,
    errors: &mut Vec<ValidationError>,
) {
    let expected = semantic_type(node).expect("schema nodes are validated at registration");
    let found = value_type(value);
    if !type_accepts(node, expected, found) {
        errors.push(ValidationError::WrongType {
            fragment,
            path: path.to_string(),
            expected,
            found,
        });
        // A mistyped value cannot be recursed into.
        return;
    }
    match expected {
        FieldType::Integer => {
            let actual = integer_value(value);
            if let Some(min) = node.minimum {
                if actual < i128::from(min) {
                    errors.push(ValidationError::OutOfRange {
                        fragment,
                        path: path.to_string(),
                        min: Some(min),
                        max: node.maximum,
                    });
                }
            }
            if let Some(max) = node.maximum {
                if actual > i128::from(max) {
                    errors.push(ValidationError::OutOfRange {
                        fragment,
                        path: path.to_string(),
                        min: node.minimum,
                        max: Some(max),
                    });
                }
            }
        }
        FieldType::String => {
            if let (Value::String(text), Some(allowed)) = (value, node.r#enum.as_ref()) {
                if !allowed.contains(text) {
                    errors.push(ValidationError::EnumViolation {
                        fragment,
                        path: path.to_string(),
                        allowed: allowed.clone(),
                    });
                }
            }
        }
        FieldType::IpNetwork => {
            let valid = match value {
                Value::IpNetwork((addr, prefix)) => addr.is_ipv4() && *prefix <= 32,
                Value::String(text) => {
                    matches!(parse_ip_network(text), Some((std::net::IpAddr::V4(_), prefix)) if prefix <= 32)
                }
                _ => unreachable!("the type test above accepted only CIDR values"),
            };
            if !valid {
                errors.push(ValidationError::InvalidFormat {
                    fragment,
                    path: path.to_string(),
                    expected: "ipv4-cidr",
                });
            }
        }
        FieldType::Object => {
            if let Value::Map(inner) = value {
                walk_map(node, inner, path, &[], fragment, writable_only, errors);
            }
        }
        FieldType::Array => {
            if let (Value::List(items), Some(item_schema)) = (value, node.items.as_deref()) {
                for (index, item) in items.iter().enumerate() {
                    walk_node(
                        item_schema,
                        item,
                        &format!("{path}[{index}]"),
                        fragment,
                        writable_only,
                        errors,
                    );
                }
            }
        }
        FieldType::Boolean | FieldType::IpAddress => {
            // Fully checked by the type test above.
        }
    }
}

/// The dialect type a value has.
fn value_type(value: &Value) -> FieldType {
    match value {
        Value::String(_) => FieldType::String,
        Value::U64(_) | Value::I64(_) => FieldType::Integer,
        Value::Bool(_) => FieldType::Boolean,
        Value::IpAddr(_) => FieldType::IpAddress,
        Value::IpNetwork(_) => FieldType::IpNetwork,
        Value::List(_) => FieldType::Array,
        Value::Map(_) => FieldType::Object,
    }
}

/// Whether a value of type `found` satisfies a field typed `expected`.
///
/// A formatted IPv4 CIDR field additionally accepts text so programmatic
/// callers need not construct the model's specialized IP-network variant.
fn type_accepts(node: &SchemaNode, expected: FieldType, found: FieldType) -> bool {
    expected == found
        || matches!(
            (node.format, expected, found),
            (
                Some(SchemaFormat::Ipv4Cidr),
                FieldType::IpNetwork,
                FieldType::String
            )
        )
}

/// A query selector comes only from a schema-validated string field. Keeping
/// this conversion local avoids arbitrary `Value::to_string()` coercions.
fn field_string(fields: &IndexMap<String, Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn remove_read_only_property(
    node: &SchemaNode,
    map: &mut IndexMap<String, Value>,
    key: &str,
    prefix: &str,
    removed: &mut Vec<String>,
) {
    let path = join_path(prefix, key);
    if !node.writable {
        map.shift_remove(key);
        removed.push(path);
    } else if let Some(Value::Map(value)) = map.get_mut(key) {
        remove_read_only_from_map(node, value, &path, removed);
    } else if let Some(Value::List(values)) = map.get_mut(key) {
        if let Some(items) = &node.items {
            for (index, value) in values.iter_mut().enumerate() {
                if let Value::Map(value) = value {
                    remove_read_only_from_map(items, value, &format!("{path}[{index}]"), removed);
                }
            }
        }
    }
}

fn remove_read_only_from_map(
    node: &SchemaNode,
    map: &mut IndexMap<String, Value>,
    prefix: &str,
    removed: &mut Vec<String>,
) {
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        if let Some(property) = node.properties.as_ref().and_then(|props| props.get(&key)) {
            remove_read_only_property(property, map, &key, prefix, removed);
        }
    }
}

/// The value of an accepted integer in `i128`, so a `U64` above
/// `i64::MAX` compares as out of range instead of wrapping. Only called
/// once the type test has accepted the value as an integer.
fn integer_value(value: &Value) -> i128 {
    match value {
        Value::U64(n) => i128::from(*n),
        Value::I64(n) => i128::from(*n),
        _ => unreachable!("the type test above accepted the value as an integer"),
    }
}

/// Join a path prefix and a key with a dot; an empty prefix yields just
/// the key.
fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTEGER_SCHEMA: &str = r#"{"type":"integer","x-netfyr-writable":true}"#;

    #[test]
    fn nested_base_extensions_cannot_shadow_fragment_triggers() {
        for path in [
            "ipv4.extension",
            "ipv4.extension.nested",
            "ethernet.extension",
        ] {
            let mut registry = SchemaRegistry::new();
            let base_fields = registry.fragment_fields("base");
            let error = registry
                .add_schema_node("base", path, INTEGER_SCHEMA)
                .unwrap_err();
            assert!(
                error.contains("collides with a fragment trigger key"),
                "{error}"
            );
            assert_eq!(registry.fragment_fields("base"), base_fields);

            // A rejected extension must not change the schema used by the
            // decoder, even though validation also accepts CIDRs as strings.
            let states = registry
                .from_yaml("ipv4: { addresses: [{ ip: 192.0.2.1/24 }] }")
                .unwrap();
            let Value::Map(ipv4) = &states[0].fields["ipv4"] else {
                panic!("ipv4 must be a map");
            };
            let Value::List(addresses) = &ipv4["addresses"] else {
                panic!("addresses must be a list");
            };
            let Value::Map(address) = &addresses[0] else {
                panic!("address must be a map");
            };
            assert_eq!(
                address["ip"],
                Value::IpNetwork(("192.0.2.1".parse().unwrap(), 24))
            );
            assert!(registry.validate_writable(&states[0]).is_empty());
        }
    }

    fn assert_schema_rejected_without_mutation(json: &str, reason: &str) {
        let mut registry = SchemaRegistry::new();
        let base_fields = registry.fragment_fields("base");
        let error = registry
            .add_schema_node("base", "custom.nested", json)
            .unwrap_err();
        assert!(error.contains(reason), "{json}: {error}");
        assert_eq!(registry.fragment_fields("base"), base_fields);
        assert!(registry.top_level_schema("custom").is_none());

        // Even the synthesized parents must be absent after rejection.
        registry
            .add_schema_node("base", "custom", INTEGER_SCHEMA)
            .unwrap();
    }

    #[test]
    fn schema_extensions_require_types_at_every_node() {
        for json in [
            "{}",
            r#"{"type":"object","properties":{"child":{}}}"#,
            r#"{"type":"array","items":{}}"#,
            r#"{"type":"array","items":{"type":"object","properties":{"child":{}}}}"#,
        ] {
            assert_schema_rejected_without_mutation(json, "type");
        }
    }

    #[test]
    fn schema_extensions_reject_incompatible_formats_at_every_node() {
        for json in [
            r#"{"type":"integer","format":"ipv4-cidr"}"#,
            r#"{"type":"object","properties":{"child":{"type":"boolean","format":"ipv4-cidr"}}}"#,
            r#"{"type":"array","items":{"type":"integer","format":"ipv4-cidr"}}"#,
        ] {
            assert_schema_rejected_without_mutation(json, "ipv4-cidr");
        }
    }

    #[test]
    fn schema_extensions_decode_valid_formats_in_their_own_fragment() {
        for (fragment, path, domain) in [
            ("ipv4", "extension", "ipv4"),
            ("base", "custom.extension", "custom"),
        ] {
            let mut registry = SchemaRegistry::new();
            registry
                .add_schema_node(
                    fragment,
                    path,
                    r#"{"type":"string","format":"ipv4-cidr","x-netfyr-writable":true}"#,
                )
                .unwrap();
            let states = registry
                .from_yaml(&format!("{domain}: {{ extension: 192.0.2.1/24 }}"))
                .unwrap();
            let Value::Map(values) = &states[0].fields[domain] else {
                panic!("extension's domain must be a map");
            };
            assert_eq!(
                values["extension"],
                Value::IpNetwork(("192.0.2.1".parse().unwrap(), 24))
            );
            assert!(registry.validate_writable(&states[0]).is_empty());
        }
    }
}
