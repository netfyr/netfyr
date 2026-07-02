//! Entity schema validation for netfyr state.
//!
//! Provides [`SchemaRegistry`], which loads JSON Schema definitions embedded at
//! compile time and validates [`State`] instances against them. The registry
//! distinguishes between full validation (structural correctness) and writable
//! validation (additionally rejects read-only fields that users cannot set).

use crate::{entity_types::{ETHERNET, WIFI}, FieldValue, State, Value};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::fmt;

// Embedded JSON Schema files — paths are relative to this source file.
// The compiler will error here if the file does not exist.
const ETHERNET_SCHEMA: &str = include_str!("schemas/ethernet.json");
const WIFI_SCHEMA: &str = include_str!("schemas/wifi.json");
const IPV4_SCHEMA: &str = include_str!("schemas/ipv4.json");
const IPV6_SCHEMA: &str = include_str!("schemas/ipv6.json");
const LINK_SCHEMA: &str = include_str!("schemas/link.json");

// ── FieldType ─────────────────────────────────────────────────────────────────

/// The expected type of a field in an entity schema.
///
/// Uses a single `Integer` variant for all integer widths because JSON Schema
/// uses `"type": "integer"` uniformly; range is expressed via `minimum`/`maximum`
/// constraints in [`FieldConstraints`], not the type width.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    String,
    Integer,
    Bool,
    Array,
    Object,
    IpAddress,
    IpNetwork,
    MacAddress,
}

// ── FieldConstraints ──────────────────────────────────────────────────────────

/// Optional constraints on a field's value beyond its basic type.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldConstraints {
    /// Inclusive minimum for integer fields.
    pub min: Option<i64>,
    /// Inclusive maximum for integer fields.
    pub max: Option<i64>,
    /// Regex pattern for string fields.
    pub pattern: Option<String>,
}

// ── FieldSchemaInfo ───────────────────────────────────────────────────────────

/// Metadata about a single field as declared in an entity schema.
#[derive(Debug, Clone)]
pub struct FieldSchemaInfo {
    pub field_type: FieldType,
    pub required: bool,
    /// `true` = can be set in policies; `false` = read-only (query output only).
    pub writable: bool,
    /// `true` = when this field is absent from the desired state, keep the
    /// current kernel value instead of unsetting it. Used for fields like
    /// `mtu` that have a kernel default and are not always managed by policies.
    pub keep_when_absent: bool,
    pub constraints: Option<FieldConstraints>,
    pub description: Option<String>,
    /// When this field is a list of maps, two items are considered equal if they
    /// agree on these keys, regardless of other keys. Empty means use PartialEq.
    pub comparison_keys: Vec<String>,
}

// ── ValidationErrorKind ───────────────────────────────────────────────────────

/// Category of a validation error, for programmatic handling.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationErrorKind {
    /// The field value has the wrong JSON type.
    InvalidType,
    /// The field value is outside the allowed numeric range.
    OutOfRange,
    /// The field name is not defined in the schema (`additionalProperties: false`).
    UnknownField,
    /// A required field is absent.
    MissingRequired,
    /// The field is read-only and cannot be set in a policy.
    ReadOnlyField,
    /// The field value does not match the required string pattern.
    InvalidFormat,
    /// A constraint violation not covered by the above kinds.
    ConstraintViolation,
    /// The entity type is not registered in the schema registry.
    UnknownEntityType,
}

// ── ValidationError ───────────────────────────────────────────────────────────

/// A single validation error for one field (or the entity as a whole).
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// Field path in dot-bracket notation: `"mtu"`, `"routes[0].gateway"`.
    /// Empty string for entity-level errors (e.g., unknown entity type).
    pub field: String,
    /// Human-readable error description.
    pub message: String,
    /// Category of the error.
    pub kind: ValidationErrorKind,
}

// ── ValidationErrors ──────────────────────────────────────────────────────────

/// A collection of validation errors from a single validation run.
///
/// All errors are collected rather than stopping at the first failure, so users
/// receive a complete picture of what needs to be fixed.
#[derive(Debug, Clone)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// Returns all collected errors.
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    /// Returns the number of errors.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are no errors.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, err) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            if err.field.is_empty() {
                write!(f, "  - {}", err.message)?;
            } else {
                write!(f, "  - field \"{}\": {}", err.field, err.message)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

// ── EntitySchema ──────────────────────────────────────────────────────────────

/// The compiled schema for a single entity type.
pub struct EntitySchema {
    /// Pre-compiled JSON Schema validator for efficient repeated validation.
    validator: jsonschema::Validator,
    /// Parsed field metadata indexed by field name (top-level properties only).
    fields: HashMap<String, FieldSchemaInfo>,
    /// Parsed sub-property metadata for Object-type top-level fields.
    ///
    /// Keyed by parent field name → child field name → metadata. Populated for
    /// any top-level field with `"type": "object"` that has `"properties"`.
    /// Used by `SchemaRegistry::field_info()` and `comparison_keys()` to
    /// provide backward-compatible lookup of fields that moved into sub-objects
    /// (e.g., `addresses` and `routes` moved into `ipv4`/`ipv6`).
    sub_fields: HashMap<String, HashMap<String, FieldSchemaInfo>>,
    /// Raw JSON Schema value kept alive so the validator's borrows remain valid.
    #[allow(dead_code)]
    raw: serde_json::Value,
}

impl EntitySchema {
    /// Returns metadata for a specific top-level field, or `None` if not in this schema.
    pub fn field_info(&self, field: &str) -> Option<&FieldSchemaInfo> {
        self.fields.get(field)
    }

    /// Returns metadata for a sub-property of an Object-type top-level field.
    pub fn sub_field_info(&self, parent: &str, child: &str) -> Option<&FieldSchemaInfo> {
        self.sub_fields.get(parent)?.get(child)
    }

    /// Returns all top-level field names declared in this schema.
    pub fn field_names(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }
}

// ── SchemaRegistry ────────────────────────────────────────────────────────────

/// Registry of entity type schemas, loaded from embedded JSON Schema files.
///
/// Created once at startup via [`SchemaRegistry::new()`]. All methods take
/// `&self`, allowing the registry to be shared.
pub struct SchemaRegistry {
    schemas: HashMap<String, EntitySchema>,
}

impl SchemaRegistry {
    /// Creates a registry pre-loaded with all embedded entity schemas.
    ///
    /// # Panics
    ///
    /// Panics if any embedded schema is malformed JSON or an invalid JSON
    /// Schema. Since schemas are compile-time constants this indicates a
    /// build-time bug, not a runtime condition.
    pub fn new() -> Self {
        let fragments = load_fragments();
        let mut schemas = HashMap::new();
        schemas.insert(ETHERNET.to_string(), load_entity_schema(ETHERNET, ETHERNET_SCHEMA, &fragments));
        schemas.insert(WIFI.to_string(), load_entity_schema(WIFI, WIFI_SCHEMA, &fragments));
        SchemaRegistry { schemas }
    }

    /// Validates a state against its entity type schema.
    ///
    /// Returns `Ok(())` if valid. Returns all collected errors (not just the
    /// first) so the caller can report a complete picture of what is wrong.
    pub fn validate(&self, state: &State) -> Result<(), ValidationErrors> {
        let entity_schema = match self.schemas.get(&state.entity_type) {
            Some(s) => s,
            None => {
                return Err(ValidationErrors(vec![ValidationError {
                    field: String::new(),
                    message: format!(
                        "unknown entity type: {}; known types: {}",
                        state.entity_type,
                        {
                            let mut types = self.entity_types();
                            types.sort_unstable();
                            types.join(", ")
                        }
                    ),
                    kind: ValidationErrorKind::UnknownEntityType,
                }]));
            }
        };

        let instance = fields_to_json(&state.fields);
        let mut errors: Vec<ValidationError> = entity_schema
            .validator
            .iter_errors(&instance)
            .flat_map(|err| {
                use jsonschema::error::ValidationErrorKind as JsKind;
                let base_field = json_pointer_to_field_path(&err.instance_path.to_string());
                match &err.kind {
                    // AdditionalProperties carries the unknown field names directly.
                    // Emit one error per unknown field so the field path is correct.
                    JsKind::AdditionalProperties { unexpected } => unexpected
                        .iter()
                        .map(|name| {
                            let field = if base_field.is_empty() {
                                name.clone()
                            } else {
                                format!("{base_field}.{name}")
                            };
                            ValidationError {
                                field,
                                message: err.to_string(),
                                kind: ValidationErrorKind::UnknownField,
                            }
                        })
                        .collect::<Vec<_>>(),
                    // For Required errors the instance_path points to the parent object;
                    // append the missing property name to form the full field path.
                    JsKind::Required { property } => {
                        let mut field = base_field;
                        if let Some(prop_name) = property.as_str() {
                            if !field.is_empty() {
                                field.push('.');
                            }
                            field.push_str(prop_name);
                        }
                        vec![ValidationError {
                            field,
                            message: err.to_string(),
                            kind: ValidationErrorKind::MissingRequired,
                        }]
                    }
                    _ => {
                        let kind = classify_error_kind(&err);
                        let message = err.to_string();
                        vec![ValidationError { field: base_field, message, kind }]
                    }
                }
            })
            .collect();

        errors.extend(run_custom_checks(state));

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Like [`validate`], but additionally rejects read-only fields.
    ///
    /// Used when validating user-provided policy state: users must not set
    /// fields that are populated only by queries (e.g., hardware properties).
    /// Both structural errors and read-only field errors are collected
    /// independently and combined — this method never short-circuits.
    pub fn validate_writable(&self, state: &State) -> Result<(), ValidationErrors> {
        // Collect structural errors from JSON Schema validation.
        let mut errors = match self.validate(state) {
            Ok(()) => Vec::new(),
            Err(ValidationErrors(errs)) => errs,
        };

        // If the entity type is unknown there is no schema to check writability against.
        let entity_schema = match self.schemas.get(&state.entity_type) {
            Some(s) => s,
            None => return Err(ValidationErrors(errors)),
        };

        // Check each field against writable metadata.
        for (field_name, _) in &state.fields {
            if let Some(info) = entity_schema.fields.get(field_name.as_str()) {
                if !info.writable {
                    errors.push(ValidationError {
                        field: field_name.clone(),
                        message: format!(
                            "field \"{field_name}\" is read-only and cannot be set in a policy"
                        ),
                        kind: ValidationErrorKind::ReadOnlyField,
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Returns the compiled schema for an entity type, or `None` if unknown.
    pub fn get_schema(&self, entity_type: &str) -> Option<&EntitySchema> {
        self.schemas.get(entity_type)
    }

    /// Returns all registered entity type names.
    pub fn entity_types(&self) -> Vec<&str> {
        self.schemas.keys().map(String::as_str).collect()
    }

    /// Returns metadata for a specific field in an entity type.
    ///
    /// Checks top-level fields first. If not found, falls back to searching
    /// sub-properties of Object-type fields — this backward-compatible fallback
    /// allows callers like the diff engine to look up `"addresses"` or `"routes"`
    /// by name even after they moved into `ipv4`/`ipv6` sub-objects.
    ///
    /// Returns `None` if the entity type or field is not found anywhere.
    pub fn field_info(&self, entity_type: &str, field: &str) -> Option<FieldSchemaInfo> {
        let schema = self.schemas.get(entity_type)?;
        // Top-level lookup (fast path).
        if let Some(info) = schema.fields.get(field) {
            return Some(info.clone());
        }
        // Fallback: search sub-fields across all Object-type top-level properties.
        for sub_map in schema.sub_fields.values() {
            if let Some(info) = sub_map.get(field) {
                return Some(info.clone());
            }
        }
        None
    }

    /// Returns the comparison keys for a list field in an entity type.
    ///
    /// Returns `Some(keys)` when the field declares `x-netfyr-comparison-keys`
    /// in its schema; returns `None` if the entity type or field is not found,
    /// or if the field has no comparison keys. Callers that receive `None`
    /// should fall back to full-value equality.
    ///
    /// Applies the same backward-compatible sub-field fallback as
    /// [`field_info`](Self::field_info).
    pub fn comparison_keys(&self, entity_type: &str, field_name: &str) -> Option<Vec<String>> {
        let schema = self.schemas.get(entity_type)?;
        // Top-level lookup.
        if let Some(info) = schema.fields.get(field_name) {
            return if info.comparison_keys.is_empty() { None } else { Some(info.comparison_keys.clone()) };
        }
        // Fallback: search sub-fields.
        for sub_map in schema.sub_fields.values() {
            if let Some(info) = sub_map.get(field_name) {
                return if info.comparison_keys.is_empty() { None } else { Some(info.comparison_keys.clone()) };
            }
        }
        None
    }
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn load_fragments() -> HashMap<String, serde_json::Value> {
    let mut fragments = HashMap::new();
    for (name, src) in [("ipv4", IPV4_SCHEMA), ("ipv6", IPV6_SCHEMA), ("link", LINK_SCHEMA)] {
        let val: serde_json::Value = serde_json::from_str(src)
            .unwrap_or_else(|e| panic!("embedded {name} fragment is malformed JSON: {e}"));
        fragments.insert(name.to_string(), val);
    }
    fragments
}

fn resolve_schema(
    raw: &serde_json::Value,
    fragments: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    let mut resolved = raw.clone();
    let inherit = match raw.get("x-netfyr-inherit").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => return resolved,
    };

    let props = resolved
        .as_object_mut().unwrap()
        .entry("properties")
        .or_insert_with(|| serde_json::Value::Object(Default::default()))
        .as_object_mut().unwrap();

    for name_val in &inherit {
        let name = name_val.as_str()
            .unwrap_or_else(|| panic!("x-netfyr-inherit entries must be strings"));
        let fragment = fragments.get(name)
            .unwrap_or_else(|| panic!("unknown fragment: {name}"));
        let frag_props = fragment.get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("fragment {name} has no properties"));
        for (k, v) in frag_props {
            if props.contains_key(k) {
                panic!(
                    "fragment \"{name}\" property \"{k}\" collides with an existing property"
                );
            }
            props.insert(k.clone(), v.clone());
        }
    }

    resolved.as_object_mut().unwrap().remove("x-netfyr-inherit");
    resolved
}

fn load_entity_schema(
    name: &str,
    schema_str: &str,
    fragments: &HashMap<String, serde_json::Value>,
) -> EntitySchema {
    let raw: serde_json::Value = serde_json::from_str(schema_str)
        .unwrap_or_else(|e| panic!("embedded {name} schema is malformed JSON: {e}"));
    let resolved = resolve_schema(&raw, fragments);
    let validator = jsonschema::validator_for(&resolved)
        .unwrap_or_else(|e| panic!("embedded {name} schema is invalid JSON Schema: {e}"));
    let fields = parse_field_metadata(&resolved);
    let sub_fields = parse_sub_field_metadata(&resolved);
    EntitySchema { validator, fields, sub_fields, raw: resolved }
}

/// Converts `State.fields` to a JSON object suitable for schema validation.
///
/// Only the `value` component of each [`FieldValue`] is included; `provenance`
/// is omitted because it is not part of the entity schema definition.
fn fields_to_json(fields: &IndexMap<String, FieldValue>) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        fields.iter().map(|(k, fv)| (k.clone(), value_to_json(&fv.value))).collect();
    serde_json::Value::Object(map)
}

/// Converts a [`Value`] to a [`serde_json::Value`] for JSON Schema validation.
///
/// IP address and network types are converted to their string representations
/// because the JSON Schema defines them as `"type": "string"` with a pattern.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::U64(n) => serde_json::Value::from(*n),
        Value::I64(n) => serde_json::Value::from(*n),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::IpAddr(ip) => serde_json::Value::String(ip.to_string()),
        Value::IpNetwork(net) => serde_json::Value::String(net.to_string()),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        Value::Map(map) => serde_json::Value::Object(
            map.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect(),
        ),
    }
}

/// Converts a JSON Pointer (e.g., `/routes/0/destination`) to a user-friendly
/// dot-bracket path (e.g., `routes[0].destination`).
///
/// Rules:
/// - Numeric segments are wrapped in `[N]` and attached without a preceding `.`.
/// - String segments are joined with `.` (omitted before the first segment).
fn json_pointer_to_field_path(pointer: &str) -> String {
    if pointer.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    for segment in pointer.trim_start_matches('/').split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment.chars().all(|c| c.is_ascii_digit()) {
            // Array index: append as [N] with no leading dot.
            result.push('[');
            result.push_str(segment);
            result.push(']');
        } else {
            // Field name: add '.' separator when there is already content.
            if !result.is_empty() {
                result.push('.');
            }
            result.push_str(segment);
        }
    }
    result
}

/// Maps a `jsonschema::ValidationError` to our [`ValidationErrorKind`].
fn classify_error_kind(err: &jsonschema::ValidationError<'_>) -> ValidationErrorKind {
    use jsonschema::error::ValidationErrorKind as JsKind;
    match &err.kind {
        JsKind::AdditionalProperties { .. } => ValidationErrorKind::UnknownField,
        JsKind::Required { .. } => ValidationErrorKind::MissingRequired,
        JsKind::Type { .. } => ValidationErrorKind::InvalidType,
        JsKind::Minimum { .. }
        | JsKind::Maximum { .. }
        | JsKind::ExclusiveMinimum { .. }
        | JsKind::ExclusiveMaximum { .. } => ValidationErrorKind::OutOfRange,
        JsKind::Pattern { .. } => ValidationErrorKind::InvalidFormat,
        _ => ValidationErrorKind::ConstraintViolation,
    }
}

/// Parses field metadata from the `properties` section of a JSON Schema object.
fn parse_field_metadata(schema: &serde_json::Value) -> HashMap<String, FieldSchemaInfo> {
    let mut fields = HashMap::new();

    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return fields,
    };

    // Collect the names of required fields from the top-level `required` array.
    let required_set: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    for (field_name, field_schema) in properties {
        let field_type = parse_field_type(field_schema);
        let required = required_set.contains(&field_name.as_str());
        // `x-netfyr-writable` defaults to `false` (read-only) when absent.
        let writable =
            field_schema.get("x-netfyr-writable").and_then(|v| v.as_bool()).unwrap_or(false);
        let keep_when_absent =
            field_schema.get("x-netfyr-keep-when-absent").and_then(|v| v.as_bool()).unwrap_or(false);
        let description =
            field_schema.get("description").and_then(|v| v.as_str()).map(String::from);
        let constraints = parse_constraints(field_schema);
        let comparison_keys = field_schema
            .get("x-netfyr-comparison-keys")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        fields.insert(
            field_name.clone(),
            FieldSchemaInfo { field_type, required, writable, keep_when_absent, constraints, description, comparison_keys },
        );
    }

    fields
}

/// Parses sub-property metadata for all Object-type top-level properties.
///
/// For each top-level property with `"type": "object"` that has nested
/// `"properties"`, calls [`parse_field_metadata`] on the sub-schema and stores
/// the result keyed by `parent_name → child_name → FieldSchemaInfo`.
fn parse_sub_field_metadata(schema: &serde_json::Value) -> HashMap<String, HashMap<String, FieldSchemaInfo>> {
    let mut sub_fields: HashMap<String, HashMap<String, FieldSchemaInfo>> = HashMap::new();

    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return sub_fields,
    };

    for (field_name, field_schema) in properties {
        let is_object = field_schema.get("type").and_then(|t| t.as_str()) == Some("object");
        let has_props = field_schema.get("properties").is_some();

        if is_object && has_props {
            let sub = parse_field_metadata(field_schema);
            if !sub.is_empty() {
                sub_fields.insert(field_name.clone(), sub);
            }
        }
    }

    sub_fields
}

/// Determines the [`FieldType`] from a JSON Schema property definition.
fn parse_field_type(field_schema: &serde_json::Value) -> FieldType {
    match field_schema.get("type").and_then(|t| t.as_str()) {
        Some("integer") => FieldType::Integer,
        Some("boolean") => FieldType::Bool,
        Some("array") => FieldType::Array,
        Some("object") => FieldType::Object,
        _ => FieldType::String,
    }
}

/// Extracts numeric and pattern constraints from a JSON Schema property.
///
/// Returns `None` if the property has no constraints (no min, max, or pattern).
fn parse_constraints(field_schema: &serde_json::Value) -> Option<FieldConstraints> {
    let min = field_schema.get("minimum").and_then(|v| v.as_i64());
    let max = field_schema.get("maximum").and_then(|v| v.as_i64());
    let pattern = field_schema.get("pattern").and_then(|v| v.as_str()).map(String::from);

    if min.is_some() || max.is_some() || pattern.is_some() {
        Some(FieldConstraints { min, max, pattern })
    } else {
        None
    }
}

/// Dispatches entity-type-specific custom validation checks that go beyond what
/// JSON Schema alone can express (e.g., duplicate detection with named values).
fn run_custom_checks(state: &State) -> Vec<ValidationError> {
    match state.entity_type.as_str() {
        ETHERNET | WIFI => check_addresses(state),
        _ => Vec::new(),
    }
}

/// Custom validation for address fields:
/// - Rejects duplicate CIDR strings with a `ConstraintViolation` error.
///
/// Checks addresses in nested `ipv4.addresses` and `ipv6.addresses` sub-objects,
/// and also the flat top-level `addresses` field for backward compatibility with
/// states produced by the current backend (which has not yet been restructured).
fn check_addresses(state: &State) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Nested ipv4.addresses (new format).
    if let Some(fv) = state.fields.get("ipv4") {
        if let Value::Map(ipv4_map) = &fv.value {
            if let Some(Value::List(addresses)) = ipv4_map.get("addresses") {
                errors.extend(check_address_list(addresses, "ipv4.addresses"));
            }
        }
    }

    // Nested ipv6.addresses (new format).
    if let Some(fv) = state.fields.get("ipv6") {
        if let Value::Map(ipv6_map) = &fv.value {
            if let Some(Value::List(addresses)) = ipv6_map.get("addresses") {
                errors.extend(check_address_list(addresses, "ipv6.addresses"));
            }
        }
    }

    // Flat top-level addresses (backward compat with backend-produced states).
    if let Some(fv) = state.fields.get("addresses") {
        if let Value::List(addresses) = &fv.value {
            errors.extend(check_address_list(addresses, "addresses"));
        }
    }

    errors
}

/// Checks a slice of address items for duplicates and returns any errors found.
fn check_address_list(addresses: &[Value], field_prefix: &str) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut reported_dup: HashSet<String> = HashSet::new();

    for item in addresses {
        let addr = match item {
            Value::IpNetwork(_) | Value::String(_) => item.to_string(),
            Value::Map(m) => match m.get("address") {
                Some(v) => v.to_string(),
                None => continue,
            },
            _ => continue,
        };

        if !seen.insert(addr.clone()) && reported_dup.insert(addr.clone()) {
            errors.push(ValidationError {
                field: field_prefix.to_string(),
                message: format!("duplicate address \"{addr}\""),
                kind: ValidationErrorKind::ConstraintViolation,
            });
        }
    }

    errors
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldValue, Provenance, Selector, State, StateMetadata, Value};
    use indexmap::IndexMap;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Build a minimal State with the given entity_type and field name/value pairs.
    fn make_state(entity_type: &str, fields: Vec<(&str, Value)>) -> State {
        let mut field_map = IndexMap::new();
        for (name, value) in fields {
            field_map.insert(
                name.to_string(),
                FieldValue { value, provenance: Provenance::KernelDefault },
            );
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name("eth0"),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        }
    }

    /// Build a route Value::Map with optional destination, gateway, and metric.
    fn make_route(destination: Option<&str>, gateway: Option<&str>, metric: Option<u64>) -> Value {
        let mut map = IndexMap::new();
        if let Some(dst) = destination {
            map.insert("destination".to_string(), Value::String(dst.to_string()));
        }
        if let Some(gw) = gateway {
            map.insert("gateway".to_string(), Value::String(gw.to_string()));
        }
        if let Some(m) = metric {
            map.insert("metric".to_string(), Value::U64(m));
        }
        Value::Map(map)
    }

    /// Build a `Value::Map` from name/value pairs (used to construct sub-objects in tests).
    fn make_map(pairs: &[(&str, Value)]) -> Value {
        let mut map = IndexMap::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.clone());
        }
        Value::Map(map)
    }

    // ── Feature: Schema registry initialization ───────────────────────────────

    /// Scenario: Registry loads with embedded schemas — entity_types() includes "ethernet"
    #[test]
    fn test_registry_loads_ethernet_schema_on_new() {
        let registry = SchemaRegistry::new();
        let types = registry.entity_types();
        assert!(
            types.contains(&"ethernet"),
            "entity_types() should include 'ethernet', got: {:?}",
            types
        );
    }

    /// Scenario: Registry loads with embedded schemas — get_schema("ethernet") returns Some
    #[test]
    fn test_registry_get_schema_ethernet_returns_some() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.get_schema("ethernet").is_some(),
            "get_schema(\"ethernet\") should return Some"
        );
    }

    /// Scenario: Unknown entity type returns None
    #[test]
    fn test_registry_unknown_entity_type_returns_none() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.get_schema("nonexistent").is_none(),
            "get_schema(\"nonexistent\") should return None"
        );
    }

    /// SchemaRegistry::default() should be equivalent to SchemaRegistry::new()
    #[test]
    fn test_schema_registry_default_loads_ethernet() {
        let registry = SchemaRegistry::default();
        assert!(registry.get_schema("ethernet").is_some());
    }

    // ── Feature: Ethernet schema validation ───────────────────────────────────

    /// Scenario: Valid ethernet state passes validation (mtu + addresses)
    #[test]
    fn test_valid_ethernet_state_passes_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("ipv4", make_map(&[("addresses", Value::List(vec![Value::String("10.0.1.50/24".to_string())]))])),
                ("ipv6", make_map(&[("addresses", Value::List(vec![Value::String("2001:db8::50/64".to_string())]))])),
            ],
        );
        assert!(registry.validate(&state).is_ok(), "valid ethernet state should pass validation");
    }

    /// Scenario: Empty ethernet state (no fields) also passes validation — all fields optional
    #[test]
    fn test_empty_ethernet_state_passes_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![]);
        assert!(
            registry.validate(&state).is_ok(),
            "ethernet state with no fields should pass (all fields are optional)"
        );
    }

    /// Scenario: MTU below minimum is rejected — returns ValidationErrors
    #[test]
    fn test_mtu_below_minimum_is_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(10))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "mtu=10 should fail validation (minimum is 68)");
    }

    /// Scenario: MTU below minimum — errors include OutOfRange for "mtu"
    #[test]
    fn test_mtu_below_minimum_error_kind_is_out_of_range() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(10))]);
        let errs = registry.validate(&state).unwrap_err();
        let has_out_of_range = errs
            .errors()
            .iter()
            .any(|e| e.field == "mtu" && e.kind == ValidationErrorKind::OutOfRange);
        assert!(
            has_out_of_range,
            "expected OutOfRange error for field 'mtu', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: MTU below minimum — message mentions the minimum value 68
    #[test]
    fn test_mtu_below_minimum_message_mentions_68() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(10))]);
        let errs = registry.validate(&state).unwrap_err();
        let mtu_err = errs
            .errors()
            .iter()
            .find(|e| e.field == "mtu")
            .expect("should have an error for field 'mtu'");
        assert!(
            mtu_err.message.contains("68"),
            "error message should mention minimum '68', got: {}",
            mtu_err.message
        );
    }

    /// Scenario: MTU above maximum is rejected — returns ValidationErrors
    #[test]
    fn test_mtu_above_maximum_is_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(99999))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "mtu=99999 should fail validation (maximum is 65535)");
    }

    /// Scenario: MTU above maximum — errors include OutOfRange for "mtu"
    #[test]
    fn test_mtu_above_maximum_error_kind_is_out_of_range() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(99999))]);
        let errs = registry.validate(&state).unwrap_err();
        let has_out_of_range = errs
            .errors()
            .iter()
            .any(|e| e.field == "mtu" && e.kind == ValidationErrorKind::OutOfRange);
        assert!(
            has_out_of_range,
            "expected OutOfRange error for field 'mtu', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: MTU above maximum — message mentions the maximum value 65535
    #[test]
    fn test_mtu_above_maximum_message_mentions_65535() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(99999))]);
        let errs = registry.validate(&state).unwrap_err();
        let mtu_err = errs
            .errors()
            .iter()
            .find(|e| e.field == "mtu")
            .expect("should have an error for field 'mtu'");
        assert!(
            mtu_err.message.contains("65535"),
            "error message should mention maximum '65535', got: {}",
            mtu_err.message
        );
    }

    /// MTU at exact minimum boundary (68) is accepted
    #[test]
    fn test_mtu_at_minimum_boundary_passes() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(68))]);
        assert!(registry.validate(&state).is_ok(), "mtu=68 (minimum boundary) should pass");
    }

    /// MTU at exact maximum boundary (65535) is accepted
    #[test]
    fn test_mtu_at_maximum_boundary_passes() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtu", Value::U64(65535))]);
        assert!(registry.validate(&state).is_ok(), "mtu=65535 (maximum boundary) should pass");
    }

    /// Scenario: Unknown field is rejected — "mtt" (typo for mtu) produces UnknownField
    #[test]
    fn test_unknown_field_is_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtt", Value::U64(1500))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "unknown field 'mtt' should be rejected");
    }

    /// Scenario: Unknown field — error kind is UnknownField
    #[test]
    fn test_unknown_field_error_kind_is_unknown_field() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtt", Value::U64(1500))]);
        let errs = registry.validate(&state).unwrap_err();
        let has_unknown = errs
            .errors()
            .iter()
            .any(|e| e.kind == ValidationErrorKind::UnknownField);
        assert!(
            has_unknown,
            "expected an UnknownField error, got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: Read-only field in writable validation — mac is rejected by validate_writable
    #[test]
    fn test_read_only_mac_rejected_in_writable_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("mac", Value::String("aa:bb:cc:dd:ee:ff".to_string())),
            ],
        );
        let result = registry.validate_writable(&state);
        assert!(result.is_err(), "validate_writable should reject read-only field 'mac'");
    }

    /// Scenario: Read-only field in writable validation — error kind is ReadOnlyField for "mac"
    #[test]
    fn test_read_only_mac_error_kind_is_readonly_field() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("mac", Value::String("aa:bb:cc:dd:ee:ff".to_string())),
            ],
        );
        let errs = registry.validate_writable(&state).unwrap_err();
        let has_readonly = errs
            .errors()
            .iter()
            .any(|e| e.field == "mac" && e.kind == ValidationErrorKind::ReadOnlyField);
        assert!(
            has_readonly,
            "expected ReadOnlyField error for field 'mac', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: Read-only field in regular validation passes — validate() does not check writability
    #[test]
    fn test_read_only_field_passes_regular_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("mac", Value::String("aa:bb:cc:dd:ee:ff".to_string())),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "validate() should accept read-only fields like 'mac' (they appear in query results)"
        );
    }

    /// Scenario: Read-only carrier and speed fields pass regular validation
    ///
    /// carrier is a top-level read-only field; speed lives inside the ethernet
    /// sub-object, which is itself a read-only top-level field.
    #[test]
    fn test_read_only_carrier_and_speed_pass_regular_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("carrier", Value::Bool(true)),
                ("ethernet", make_map(&[("speed", Value::U64(1000))])),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "carrier and ethernet sub-object (with speed) should pass regular validation"
        );
    }

    /// Scenario: Route object validation — valid route with all optional fields passes
    #[test]
    fn test_route_object_validation_passes() {
        let registry = SchemaRegistry::new();
        let route = make_route(Some("0.0.0.0/0"), Some("10.0.1.1"), Some(100));
        let state = make_state("ethernet", vec![("ipv4", make_map(&[("routes", Value::List(vec![route]))]))]);
        assert!(
            registry.validate(&state).is_ok(),
            "valid route object should pass validation"
        );
    }

    /// Scenario: Route with only required destination passes
    #[test]
    fn test_route_with_only_destination_passes() {
        let registry = SchemaRegistry::new();
        let route = make_route(Some("192.168.1.0/24"), None, None);
        let state = make_state("ethernet", vec![("ipv4", make_map(&[("routes", Value::List(vec![route]))]))]);
        assert!(
            registry.validate(&state).is_ok(),
            "route with only destination should pass (gateway and metric are optional)"
        );
    }

    /// Scenario: Route without required destination is rejected
    #[test]
    fn test_route_without_destination_is_rejected() {
        let registry = SchemaRegistry::new();
        let route = make_route(None, Some("10.0.1.1"), None);
        let state = make_state("ethernet", vec![("ipv4", make_map(&[("routes", Value::List(vec![route]))]))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "route without 'destination' should fail validation");
    }

    /// Scenario: Route without required destination — error kind is MissingRequired
    #[test]
    fn test_route_without_destination_error_kind_is_missing_required() {
        let registry = SchemaRegistry::new();
        let route = make_route(None, Some("10.0.1.1"), None);
        let state = make_state("ethernet", vec![("ipv4", make_map(&[("routes", Value::List(vec![route]))]))]);
        let errs = registry.validate(&state).unwrap_err();
        let has_missing = errs
            .errors()
            .iter()
            .any(|e| e.kind == ValidationErrorKind::MissingRequired);
        assert!(
            has_missing,
            "expected MissingRequired error for missing 'destination', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: Route without required destination — error references ipv4.routes[0].destination
    ///
    /// The jsonschema crate reports `required` errors at the parent object path
    /// (`/ipv4/routes/0` → `ipv4.routes[0]`). Our Required error handler appends the
    /// missing property name so the final path is "ipv4.routes[0].destination".
    #[test]
    fn test_route_without_destination_error_references_destination() {
        let registry = SchemaRegistry::new();
        let route = make_route(None, Some("10.0.1.1"), None);
        let state = make_state("ethernet", vec![("ipv4", make_map(&[("routes", Value::List(vec![route]))]))]);
        let errs = registry.validate(&state).unwrap_err();
        let missing_err = errs
            .errors()
            .iter()
            .find(|e| e.kind == ValidationErrorKind::MissingRequired)
            .expect("should have a MissingRequired error");
        assert_eq!(
            missing_err.field, "ipv4.routes[0].destination",
            "MissingRequired error field should be 'ipv4.routes[0].destination'"
        );
    }

    /// Scenario: Multiple validation errors are collected — at least 2 errors for mtu=99999 + unknown "foo"
    #[test]
    fn test_multiple_validation_errors_are_collected() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(99999)),
                ("foo", Value::String("bar".to_string())),
            ],
        );
        let result = registry.validate(&state);
        assert!(result.is_err(), "state with multiple errors should fail validation");
        let errs = result.unwrap_err();
        assert!(
            errs.len() >= 2,
            "should collect at least 2 errors, got {} error(s): {:?}",
            errs.len(),
            errs.errors()
        );
    }

    /// Scenario: Multiple validation errors — one error for "mtu", one for the unknown field "foo"
    #[test]
    fn test_multiple_errors_include_mtu_and_unknown_field() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(99999)),
                ("foo", Value::String("bar".to_string())),
            ],
        );
        let errs = registry.validate(&state).unwrap_err();
        let has_mtu_error = errs.errors().iter().any(|e| e.field == "mtu");
        let has_foo_unknown = errs
            .errors()
            .iter()
            .any(|e| e.kind == ValidationErrorKind::UnknownField);
        assert!(has_mtu_error, "should have an error for field 'mtu'");
        assert!(has_foo_unknown, "should have an UnknownField error for 'foo'");
    }

    // ── Feature: Field info queries ───────────────────────────────────────────

    /// Scenario: Query field metadata — mtu has type Integer
    #[test]
    fn test_field_info_mtu_type_is_integer() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        assert_eq!(info.field_type, FieldType::Integer, "mtu should have FieldType::Integer");
    }

    /// Scenario: Query field metadata — mtu is writable
    #[test]
    fn test_field_info_mtu_is_writable() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        assert!(info.writable, "mtu should be writable (x-netfyr-writable: true)");
    }

    /// Scenario: Query field metadata — mtu is not required
    #[test]
    fn test_field_info_mtu_is_not_required() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        assert!(!info.required, "mtu should not be required");
    }

    /// Scenario: Query field metadata — mtu constraints include min=68
    #[test]
    fn test_field_info_mtu_constraint_min_is_68() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        let constraints = info.constraints.expect("mtu should have constraints");
        assert_eq!(constraints.min, Some(68), "mtu minimum constraint should be 68");
    }

    /// Scenario: Query field metadata — mtu constraints include max=65535
    #[test]
    fn test_field_info_mtu_constraint_max_is_65535() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        let constraints = info.constraints.expect("mtu should have constraints");
        assert_eq!(constraints.max, Some(65535), "mtu maximum constraint should be 65535");
    }

    /// Scenario: Query read-only field metadata — carrier has type Bool
    #[test]
    fn test_field_info_carrier_type_is_bool() {
        let registry = SchemaRegistry::new();
        let info =
            registry.field_info("ethernet", "carrier").expect("carrier should have field info");
        assert_eq!(info.field_type, FieldType::Bool, "carrier should have FieldType::Bool");
    }

    /// Scenario: Query read-only field metadata — carrier is not writable
    #[test]
    fn test_field_info_carrier_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info =
            registry.field_info("ethernet", "carrier").expect("carrier should have field info");
        assert!(!info.writable, "carrier should be read-only (writable: false)");
    }

    /// Scenario: Query unknown field returns None
    #[test]
    fn test_field_info_unknown_field_returns_none() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.field_info("ethernet", "nonexistent").is_none(),
            "field_info for unknown field should return None"
        );
    }

    /// field_info for unknown entity type returns None
    #[test]
    fn test_field_info_unknown_entity_type_returns_none() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.field_info("nonexistent", "mtu").is_none(),
            "field_info for unknown entity type should return None"
        );
    }

    /// mac field is read-only (x-netfyr-writable: false)
    #[test]
    fn test_field_info_mac_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mac").expect("mac should have field info");
        assert!(!info.writable, "mac should be read-only (x-netfyr-writable: false)");
    }

    /// ethernet sub-object is read-only (speed lives inside it, not at top level)
    #[test]
    fn test_field_info_speed_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "ethernet")
            .expect("ethernet sub-object should have field info");
        assert!(!info.writable, "ethernet sub-object should be read-only (x-netfyr-writable: false)");
    }

    /// routes field is writable (via sub-field fallback to ipv4.routes)
    #[test]
    fn test_field_info_routes_is_writable() {
        let registry = SchemaRegistry::new();
        let info =
            registry.field_info("ethernet", "routes").expect("routes should have field info");
        assert!(info.writable, "routes should be writable (x-netfyr-writable: true)");
    }

    /// addresses field has comparison_keys = ["address"] (via sub-field fallback)
    #[test]
    fn test_field_info_addresses_has_comparison_keys() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "addresses").expect("addresses should have field info");
        assert_eq!(info.comparison_keys, vec!["address"], "addresses must have comparison_keys=[\"address\"]");
    }

    /// mtu field has empty comparison_keys (default)
    #[test]
    fn test_field_info_mtu_has_empty_comparison_keys() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "mtu").expect("mtu should have field info");
        assert!(info.comparison_keys.is_empty(), "mtu should have empty comparison_keys");
    }

    // ── Feature: Unknown entity type handling ─────────────────────────────────

    /// Scenario: Validate state with unknown entity type — returns an error
    #[test]
    fn test_validate_unknown_entity_type_returns_error() {
        let registry = SchemaRegistry::new();
        let state = make_state("nonexistent", vec![]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "validating unknown entity type should return an error");
    }

    /// Scenario: Validate state with unknown entity type — error kind is UnknownEntityType
    #[test]
    fn test_validate_unknown_entity_type_error_kind() {
        let registry = SchemaRegistry::new();
        let state = make_state("nonexistent", vec![]);
        let errs = registry.validate(&state).unwrap_err();
        let has_unknown_type = errs
            .errors()
            .iter()
            .any(|e| e.kind == ValidationErrorKind::UnknownEntityType);
        assert!(
            has_unknown_type,
            "expected UnknownEntityType error kind, got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: validate_writable also returns error for unknown entity type
    #[test]
    fn test_validate_writable_unknown_entity_type_returns_error() {
        let registry = SchemaRegistry::new();
        let state = make_state("nonexistent", vec![]);
        let result = registry.validate_writable(&state);
        assert!(
            result.is_err(),
            "validate_writable for unknown entity type should return an error"
        );
    }

    // ── ValidationErrors API tests ────────────────────────────────────────────

    /// ValidationErrors::is_empty() returns true when there are no errors
    #[test]
    fn test_validation_errors_is_empty_with_zero_errors() {
        let errors = ValidationErrors(vec![]);
        assert!(errors.is_empty());
        assert_eq!(errors.len(), 0);
    }

    /// ValidationErrors::is_empty() returns false when there are errors
    #[test]
    fn test_validation_errors_is_not_empty_with_errors() {
        let errors = ValidationErrors(vec![ValidationError {
            field: "mtu".to_string(),
            message: "value out of range".to_string(),
            kind: ValidationErrorKind::OutOfRange,
        }]);
        assert!(!errors.is_empty());
        assert_eq!(errors.len(), 1);
    }

    /// ValidationErrors::errors() returns all contained errors
    #[test]
    fn test_validation_errors_returns_all_errors() {
        let e1 = ValidationError {
            field: "mtu".to_string(),
            message: "out of range".to_string(),
            kind: ValidationErrorKind::OutOfRange,
        };
        let e2 = ValidationError {
            field: "foo".to_string(),
            message: "unknown field".to_string(),
            kind: ValidationErrorKind::UnknownField,
        };
        let errors = ValidationErrors(vec![e1, e2]);
        assert_eq!(errors.errors().len(), 2);
    }

    /// ValidationErrors::Display includes the field name and message
    #[test]
    fn test_validation_errors_display_includes_field_and_message() {
        let errors = ValidationErrors(vec![ValidationError {
            field: "mtu".to_string(),
            message: "value too large".to_string(),
            kind: ValidationErrorKind::OutOfRange,
        }]);
        let display = errors.to_string();
        assert!(display.contains("mtu"), "display should include field name 'mtu'");
        assert!(display.contains("value too large"), "display should include message");
    }

    /// ValidationErrors with an empty field (entity-level error) displays without field prefix
    #[test]
    fn test_validation_errors_display_entity_level_error() {
        let errors = ValidationErrors(vec![ValidationError {
            field: String::new(),
            message: "unknown entity type: nonexistent".to_string(),
            kind: ValidationErrorKind::UnknownEntityType,
        }]);
        let display = errors.to_string();
        assert!(
            display.contains("unknown entity type"),
            "entity-level error should appear in display, got: {}",
            display
        );
    }

    // ── Feature: Duplicate address and IPv6 rejection ─────────────────────────

    /// Scenario: Duplicate addresses are rejected
    #[test]
    fn test_duplicate_addresses_are_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![(
                "ipv4",
                make_map(&[("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                    Value::String("10.0.1.50/24".to_string()),
                ]))]),
            )],
        );
        assert!(
            registry.validate(&state).is_err(),
            "duplicate addresses should fail validation"
        );
    }

    /// Scenario: Duplicate addresses — error kind is ConstraintViolation for field "ipv4.addresses"
    #[test]
    fn test_duplicate_addresses_error_kind_is_constraint_violation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![(
                "ipv4",
                make_map(&[("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                    Value::String("10.0.1.50/24".to_string()),
                ]))]),
            )],
        );
        let errs = registry.validate(&state).unwrap_err();
        let has_constraint = errs
            .errors()
            .iter()
            .any(|e| e.field == "ipv4.addresses" && e.kind == ValidationErrorKind::ConstraintViolation);
        assert!(
            has_constraint,
            "expected ConstraintViolation for 'ipv4.addresses', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: Duplicate addresses — message mentions the duplicated CIDR string
    #[test]
    fn test_duplicate_addresses_message_mentions_duplicated_value() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![(
                "ipv4",
                make_map(&[("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                    Value::String("10.0.1.50/24".to_string()),
                ]))]),
            )],
        );
        let errs = registry.validate(&state).unwrap_err();
        let dup_err = errs
            .errors()
            .iter()
            .find(|e| e.field == "ipv4.addresses" && e.kind == ValidationErrorKind::ConstraintViolation)
            .expect("should have a ConstraintViolation for 'ipv4.addresses'");
        assert!(
            dup_err.message.contains("10.0.1.50/24"),
            "error message should mention the duplicate address '10.0.1.50/24', got: {}",
            dup_err.message
        );
    }

    /// Non-duplicate distinct addresses pass validation
    #[test]
    fn test_distinct_addresses_pass_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![(
                "ipv4",
                make_map(&[("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                    Value::String("10.0.1.51/24".to_string()),
                ]))]),
            )],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "two distinct addresses should pass validation"
        );
    }

    /// Scenario: IPv6 addresses are accepted in the ipv6 sub-object
    #[test]
    fn test_ipv6_address_is_accepted() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![(
                "ipv6",
                make_map(&[("addresses", Value::List(vec![Value::String("2001:db8::50/64".to_string())]))]),
            )],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "IPv6 address in 'ipv6.addresses' sub-object should be accepted"
        );
    }

    /// Scenario: Map-format addresses with lifetimes are accepted by schema
    #[test]
    fn test_address_map_with_lifetime_is_accepted() {
        let registry = SchemaRegistry::new();
        let mut addr_map = IndexMap::new();
        addr_map.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        addr_map.insert("valid_lft".to_string(), Value::U64(3600));
        addr_map.insert("preferred_lft".to_string(), Value::U64(1800));
        let state = make_state(
            "ethernet",
            vec![("ipv4", make_map(&[("addresses", Value::List(vec![Value::Map(addr_map)]))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "map-format address with lifetimes should be accepted in ipv4.addresses"
        );
    }

    /// Scenario: Duplicate addresses in map format are detected
    #[test]
    fn test_duplicate_map_format_addresses_rejected() {
        let registry = SchemaRegistry::new();
        let mut m1 = IndexMap::new();
        m1.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m1.insert("valid_lft".to_string(), Value::U64(3600));
        let mut m2 = IndexMap::new();
        m2.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m2.insert("valid_lft".to_string(), Value::U64(7200));
        let state = make_state(
            "ethernet",
            vec![("ipv4", make_map(&[("addresses", Value::List(vec![Value::Map(m1), Value::Map(m2)]))]))],
        );
        let result = registry.validate(&state);
        assert!(result.is_err(), "duplicate map-format addresses should be rejected");
    }

    /// Scenario: Route with mtu, table, tos is accepted inside ipv4 sub-object
    #[test]
    fn test_route_with_mtu_table_tos_accepted() {
        let registry = SchemaRegistry::new();
        let mut route = IndexMap::new();
        route.insert("destination".to_string(), Value::String("10.0.0.0/8".to_string()));
        route.insert("gateway".to_string(), Value::String("10.0.1.1".to_string()));
        route.insert("metric".to_string(), Value::U64(100));
        route.insert("mtu".to_string(), Value::U64(1400));
        route.insert("table".to_string(), Value::U64(100));
        route.insert("tos".to_string(), Value::U64(16));
        let state = make_state(
            "ethernet",
            vec![("ipv4", make_map(&[("routes", Value::List(vec![Value::Map(route)]))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "route with mtu/table/tos should be accepted inside ipv4 sub-object"
        );
    }

    // ── Criterion 17: schema declares all read-only fields ────────────────────

    /// name field is read-only (x-netfyr-writable: false)
    #[test]
    fn test_field_info_name_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info = registry.field_info("ethernet", "name").expect("name should have field info");
        assert!(!info.writable, "name should be read-only (x-netfyr-writable: false)");
    }

    /// driver field must be in the ethernet schema and be read-only.
    #[test]
    fn test_ethernet_schema_driver_field_is_read_only() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "driver")
            .expect(
                "driver must be present in the ethernet schema with x-netfyr-writable: false \
                 (criterion 17)",
            );
        assert!(!info.writable, "driver should be read-only (x-netfyr-writable: false)");
    }

    /// Criterion 17: every field known to be read-only (carrier, speed, mac, driver, name)
    /// must be present in the ethernet schema with x-netfyr-writable: false.
    #[test]
    fn test_ethernet_schema_declares_all_spec_read_only_fields_criterion_17() {
        let registry = SchemaRegistry::new();
        // speed is nested inside the "ethernet" sub-object, not a top-level field.
        // The top-level "ethernet" field is itself read-only.
        let required_read_only = ["carrier", "ethernet", "mac", "driver", "name", "type"];

        for &field in &required_read_only {
            let info = registry.field_info("ethernet", field).unwrap_or_else(|| {
                panic!(
                    "field '{}' must be present in the ethernet schema (criterion 17: all \
                     read-only hardware fields must be declared with x-netfyr-writable: false)",
                    field
                )
            });
            assert!(
                !info.writable,
                "field '{}' must have x-netfyr-writable: false in the ethernet schema \
                 (criterion 17)",
                field
            );
        }
    }

    /// Unknown field error should reference the field name in the error's field path
    #[test]
    fn test_unknown_field_error_references_field_name() {
        let registry = SchemaRegistry::new();
        let state = make_state("ethernet", vec![("mtt", Value::U64(1500))]);
        let errs = registry.validate(&state).unwrap_err();
        let unknown_err = errs
            .errors()
            .iter()
            .find(|e| e.kind == ValidationErrorKind::UnknownField)
            .expect("should have an UnknownField error");
        assert_eq!(
            unknown_err.field, "mtt",
            "UnknownField error should reference 'mtt', got: {:?}",
            unknown_err.field
        );
    }

    // ── Feature: Schema field inventory pinning ──────────────────────────────

    #[test]
    fn test_ethernet_schema_has_exactly_the_expected_fields() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut fields: Vec<&str> = schema.field_names();
        fields.sort();

        let expected = vec![
            "carrier",
            "driver",
            "enabled",
            "ethernet",
            "ipv4",
            "ipv6",
            "mac",
            "mtu",
            "name",
            "type",
        ];

        assert_eq!(
            fields, expected,
            "ethernet schema fields diverged from pinned set.\n\
             If you added or removed a field in the schema, update this \
             test AND the corresponding backend query/apply code."
        );
    }

    #[test]
    fn test_ethernet_writable_fields_match_expected_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut writable: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| i.writable))
            .collect();
        writable.sort();

        let expected = vec!["enabled", "ipv4", "ipv6", "mtu"];

        assert_eq!(
            writable, expected,
            "ethernet writable fields diverged from pinned set.\n\
             If you changed a field's writable attribute, update this test \
             AND the apply code in netfyr-backend."
        );
    }

    #[test]
    fn test_ethernet_readonly_fields_match_expected_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut readonly: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| !i.writable))
            .collect();
        readonly.sort();

        let expected = vec!["carrier", "driver", "ethernet", "mac", "name", "type"];

        assert_eq!(
            readonly, expected,
            "ethernet read-only fields diverged from pinned set.\n\
             If you changed a field's writable attribute, update this test \
             AND the query code in netfyr-backend."
        );
    }

    // ── Feature: comparison_keys() method (SPEC-408) ──────────────────────────

    /// Scenario: comparison_keys returns Some(["address"]) for the addresses field
    ///
    /// The "addresses" field in ip.json declares x-netfyr-comparison-keys=["address"].
    /// This is the key mechanism behind DHCP renewal stability: valid_lft/preferred_lft
    /// changes during T1 renewal are ignored because only the "address" key determines
    /// whether two list items are the same.
    #[test]
    fn test_comparison_keys_addresses_returns_some_with_address_key() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("ethernet", "addresses");
        assert_eq!(
            keys,
            Some(vec!["address".to_string()]),
            "comparison_keys(\"ethernet\", \"addresses\") must return Some([\"address\"]) \
             — this is required for DHCP renewal stability (SPEC-408)"
        );
    }

    /// Scenario: comparison_keys returns None for a field that has no x-netfyr-comparison-keys
    ///
    /// Fields without x-netfyr-comparison-keys (like mtu) use full-value equality.
    /// Callers that receive None must fall back to full PartialEq comparison.
    #[test]
    fn test_comparison_keys_returns_none_for_field_without_comparison_keys() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("ethernet", "mtu");
        assert_eq!(
            keys,
            None,
            "comparison_keys(\"ethernet\", \"mtu\") must return None \
             because mtu has no x-netfyr-comparison-keys annotation"
        );
    }

    /// Scenario: comparison_keys returns None for an unknown entity type
    ///
    /// An unknown entity type has no registered schema, so no comparison keys
    /// can exist for any of its fields. The diff engine falls back to full-value
    /// equality for all fields of unregistered entity types.
    #[test]
    fn test_comparison_keys_unknown_entity_type_returns_none() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("nonexistent_type", "addresses");
        assert!(
            keys.is_none(),
            "comparison_keys for an unknown entity type must return None, \
             got: {:?}",
            keys
        );
    }

    /// Scenario: comparison_keys returns None for an unknown field name
    ///
    /// A field not in the schema has no comparison key annotation.
    /// The diff engine falls back to full-value equality.
    #[test]
    fn test_comparison_keys_unknown_field_returns_none() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("ethernet", "no_such_field");
        assert!(
            keys.is_none(),
            "comparison_keys for an unknown field must return None, \
             got: {:?}",
            keys
        );
    }

    /// Scenario: comparison_keys returns None for routes (no comparison keys defined)
    ///
    /// The routes field has no x-netfyr-comparison-keys in ip.json, so
    /// full-value equality is used when comparing routes.
    #[test]
    fn test_comparison_keys_routes_returns_none() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("ethernet", "routes");
        assert!(
            keys.is_none(),
            "comparison_keys(\"ethernet\", \"routes\") must return None \
             because routes has no x-netfyr-comparison-keys annotation"
        );
    }

    /// Scenario: addresses field with same IP key but changed preferred_lft also
    /// produces no diff
    ///
    /// During DHCP renewal, both valid_lft and preferred_lft change. Because
    /// x-netfyr-comparison-keys=["address"] only checks the "address" key,
    /// changes to either lifetime field must be invisible to the diff engine.
    /// This test verifies via SchemaRegistry that the comparison_keys for
    /// "addresses" exclude both lifetime fields.
    #[test]
    fn test_comparison_keys_for_addresses_excludes_preferred_lft() {
        let registry = SchemaRegistry::new();
        let keys = registry
            .comparison_keys("ethernet", "addresses")
            .expect("addresses must have comparison keys");
        assert!(
            !keys.contains(&"preferred_lft".to_string()),
            "comparison_keys for addresses must NOT include preferred_lft \
             — lifetime changes during DHCP renewal must not trigger a diff"
        );
        assert!(
            !keys.contains(&"valid_lft".to_string()),
            "comparison_keys for addresses must NOT include valid_lft \
             — lifetime changes during DHCP renewal must not trigger a diff"
        );
        assert!(
            keys.contains(&"address".to_string()),
            "comparison_keys for addresses must include \"address\" \
             — this is the identity key for DHCP renewal stability"
        );
    }

    // ── Feature: Schema registry initialization — WiFi ────────────────────────

    /// Scenario: Registry loads with both ethernet and wifi schemas — entity_types includes "wifi"
    #[test]
    fn test_registry_loads_wifi_schema_on_new() {
        let registry = SchemaRegistry::new();
        let types = registry.entity_types();
        assert!(
            types.contains(&"wifi"),
            "entity_types() should include 'wifi', got: {:?}",
            types
        );
    }

    /// Scenario: Registry loads with both schemas — get_schema("wifi") returns Some
    #[test]
    fn test_registry_get_schema_wifi_returns_some() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.get_schema("wifi").is_some(),
            "get_schema(\"wifi\") should return Some"
        );
    }

    // ── Feature: Ethernet sub-object validation ───────────────────────────────

    /// Scenario: Valid ethernet state with ethernet sub-object passes validation
    #[test]
    fn test_valid_ethernet_state_with_ethernet_sub_object_passes_validation() {
        let registry = SchemaRegistry::new();
        let mut eth_map = IndexMap::new();
        eth_map.insert("speed".to_string(), Value::U64(1000));
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("ethernet", Value::Map(eth_map)),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "ethernet state with ethernet sub-object {{speed: 1000}} should pass validation"
        );
    }

    /// Scenario: Technology sub-object in writable validation is rejected
    /// The "ethernet" field is read-only (x-netfyr-writable: false).
    #[test]
    fn test_ethernet_sub_object_in_writable_validation_is_rejected() {
        let registry = SchemaRegistry::new();
        let mut eth_map = IndexMap::new();
        eth_map.insert("speed".to_string(), Value::U64(1000));
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("ethernet", Value::Map(eth_map)),
            ],
        );
        let result = registry.validate_writable(&state);
        assert!(result.is_err(), "validate_writable should reject read-only field 'ethernet'");
        let errs = result.unwrap_err();
        let has_readonly = errs
            .errors()
            .iter()
            .any(|e| e.field == "ethernet" && e.kind == ValidationErrorKind::ReadOnlyField);
        assert!(
            has_readonly,
            "expected ReadOnlyField error for field 'ethernet', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: Technology sub-object in regular validation passes
    /// validate() does not check writability — read-only fields are valid in query results.
    #[test]
    fn test_ethernet_sub_object_in_regular_validation_passes() {
        let registry = SchemaRegistry::new();
        let mut eth_map = IndexMap::new();
        eth_map.insert("speed".to_string(), Value::U64(1000));
        let state = make_state(
            "ethernet",
            vec![
                ("mtu", Value::U64(1500)),
                ("ethernet", Value::Map(eth_map)),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "validate() should accept the read-only 'ethernet' sub-object (valid in query results)"
        );
    }

    // ── Feature: WiFi schema validation ──────────────────────────────────────

    /// Scenario: Valid wifi state passes validation (mtu + addresses)
    #[test]
    fn test_valid_wifi_state_passes_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("ipv4", make_map(&[("addresses", Value::List(vec![Value::String("10.0.1.50/24".to_string())]))])),
            ],
        );
        assert!(registry.validate(&state).is_ok(), "valid wifi state should pass validation");
    }

    /// Scenario: Valid wifi state with wifi sub-object passes validation
    #[test]
    fn test_valid_wifi_state_with_wifi_sub_object_passes_validation() {
        let registry = SchemaRegistry::new();
        let mut wifi_map = IndexMap::new();
        wifi_map.insert("ssid".to_string(), Value::String("home".to_string()));
        wifi_map.insert("mode".to_string(), Value::String("sta".to_string()));
        wifi_map.insert("frequency".to_string(), Value::U64(5180));
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("wifi", Value::Map(wifi_map)),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "wifi state with wifi sub-object should pass validation"
        );
    }

    /// Scenario: WiFi sub-object in writable validation is rejected
    /// The "wifi" field is read-only (x-netfyr-writable: false).
    #[test]
    fn test_wifi_sub_object_in_writable_validation_is_rejected() {
        let registry = SchemaRegistry::new();
        let mut wifi_map = IndexMap::new();
        wifi_map.insert("ssid".to_string(), Value::String("home".to_string()));
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("wifi", Value::Map(wifi_map)),
            ],
        );
        let result = registry.validate_writable(&state);
        assert!(result.is_err(), "validate_writable should reject read-only field 'wifi'");
        let errs = result.unwrap_err();
        let has_readonly = errs
            .errors()
            .iter()
            .any(|e| e.field == "wifi" && e.kind == ValidationErrorKind::ReadOnlyField);
        assert!(
            has_readonly,
            "expected ReadOnlyField error for field 'wifi', got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: WiFi sub-object in regular validation passes
    /// validate() does not check writability — technology sub-objects are valid in query results.
    #[test]
    fn test_wifi_sub_object_in_regular_validation_passes() {
        let registry = SchemaRegistry::new();
        let mut wifi_map = IndexMap::new();
        wifi_map.insert("ssid".to_string(), Value::String("home".to_string()));
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("wifi", Value::Map(wifi_map)),
            ],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "validate() should accept the read-only 'wifi' sub-object (valid in query results)"
        );
    }

    /// WiFi MTU below minimum is rejected — inherited constraint applies to wifi too
    #[test]
    fn test_wifi_mtu_below_minimum_is_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state("wifi", vec![("mtu", Value::U64(10))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "wifi mtu=10 should fail validation (minimum is 68)");
        let errs = result.unwrap_err();
        let has_out_of_range = errs
            .errors()
            .iter()
            .any(|e| e.field == "mtu" && e.kind == ValidationErrorKind::OutOfRange);
        assert!(has_out_of_range, "expected OutOfRange error for wifi 'mtu', got: {:?}", errs.errors());
    }

    /// WiFi unknown field is rejected — additionalProperties: false applies
    #[test]
    fn test_wifi_unknown_field_is_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state("wifi", vec![("bogus_field", Value::Bool(true))]);
        let result = registry.validate(&state);
        assert!(result.is_err(), "unknown field in wifi entity should be rejected");
        let errs = result.unwrap_err();
        let has_unknown = errs.errors().iter().any(|e| e.kind == ValidationErrorKind::UnknownField);
        assert!(has_unknown, "expected UnknownField error for wifi entity, got: {:?}", errs.errors());
    }

    // ── Feature: Field info queries — ethernet sub-object ─────────────────────

    /// Scenario: Query ethernet technology sub-object metadata — type is Object, writable false
    #[test]
    fn test_field_info_ethernet_sub_object_type_is_object() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "ethernet")
            .expect("field_info(\"ethernet\", \"ethernet\") should return Some");
        assert_eq!(
            info.field_type,
            FieldType::Object,
            "the 'ethernet' sub-object field should have FieldType::Object"
        );
    }

    /// Scenario: Query ethernet technology sub-object metadata — writable is false
    #[test]
    fn test_field_info_ethernet_sub_object_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "ethernet")
            .expect("field_info(\"ethernet\", \"ethernet\") should return Some");
        assert!(
            !info.writable,
            "the 'ethernet' sub-object field should be read-only (writable: false)"
        );
    }

    // ── Feature: Field info queries — wifi fields ─────────────────────────────

    /// Scenario: Query wifi field metadata — mtu has type Integer
    #[test]
    fn test_field_info_wifi_mtu_type_is_integer() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "mtu")
            .expect("field_info(\"wifi\", \"mtu\") should return Some");
        assert_eq!(info.field_type, FieldType::Integer, "wifi mtu should have FieldType::Integer");
    }

    /// Scenario: Query wifi field metadata — mtu is writable
    #[test]
    fn test_field_info_wifi_mtu_is_writable() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "mtu")
            .expect("field_info(\"wifi\", \"mtu\") should return Some");
        assert!(info.writable, "wifi mtu should be writable (x-netfyr-writable: true)");
    }

    /// Scenario: Query wifi field metadata — mtu is not required
    #[test]
    fn test_field_info_wifi_mtu_is_not_required() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "mtu")
            .expect("field_info(\"wifi\", \"mtu\") should return Some");
        assert!(!info.required, "wifi mtu should not be required");
    }

    /// Scenario: Query wifi field metadata — mtu constraints include min=68 and max=65535
    #[test]
    fn test_field_info_wifi_mtu_constraints() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "mtu")
            .expect("field_info(\"wifi\", \"mtu\") should return Some");
        let constraints = info.constraints.expect("wifi mtu should have constraints");
        assert_eq!(constraints.min, Some(68), "wifi mtu minimum should be 68");
        assert_eq!(constraints.max, Some(65535), "wifi mtu maximum should be 65535");
    }

    /// Scenario: Query wifi technology sub-object metadata — type is Object
    #[test]
    fn test_field_info_wifi_sub_object_type_is_object() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "wifi")
            .expect("field_info(\"wifi\", \"wifi\") should return Some");
        assert_eq!(
            info.field_type,
            FieldType::Object,
            "the 'wifi' sub-object field should have FieldType::Object"
        );
    }

    /// Scenario: Query wifi technology sub-object metadata — writable is false
    #[test]
    fn test_field_info_wifi_sub_object_is_not_writable() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("wifi", "wifi")
            .expect("field_info(\"wifi\", \"wifi\") should return Some");
        assert!(
            !info.writable,
            "the 'wifi' sub-object field should be read-only (writable: false)"
        );
    }

    /// Scenario: Query unknown wifi field returns None
    #[test]
    fn test_field_info_wifi_unknown_field_returns_none() {
        let registry = SchemaRegistry::new();
        assert!(
            registry.field_info("wifi", "nonexistent").is_none(),
            "field_info for unknown wifi field should return None"
        );
    }

    // ── Feature: Schema-to-code divergence protection (spec-correct pinned sets) ─
    //
    // NOTE: The existing tests (test_ethernet_schema_has_exactly_the_expected_fields,
    // test_ethernet_readonly_fields_match_expected_set, etc.) in this file were written
    // with incorrect pinned sets — they expect "speed" as a top-level field and omit
    // "ethernet" and "type". "speed" is a sub-property of the "ethernet" object, not a
    // top-level field. Those tests are bugs from the implementation phase and will be
    // caught by the verify phase.
    //
    // The tests below use the spec-correct pinned sets from SPEC-006.

    /// Scenario: Ethernet schema fields match spec pinned set (correct per SPEC-006)
    #[test]
    fn test_ethernet_schema_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut fields: Vec<&str> = schema.field_names();
        fields.sort();

        let expected = vec![
            "carrier",
            "driver",
            "enabled",
            "ethernet",
            "ipv4",
            "ipv6",
            "mac",
            "mtu",
            "name",
            "type",
        ];

        assert_eq!(
            fields, expected,
            "ethernet schema fields diverged from the SPEC-006 pinned set.\n\
             If you added or removed a field in the JSON schema, update this test \
             AND the corresponding backend query/apply code.\n\
             NOTE: 'ipv4' and 'ipv6' are sub-objects; their sub-properties (addresses, \
             routes, dns_servers) are not top-level fields."
        );
    }

    /// Scenario: Ethernet writable fields match spec pinned set (SPEC-006)
    #[test]
    fn test_ethernet_writable_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut writable: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| i.writable))
            .collect();
        writable.sort();

        let expected = vec!["enabled", "ipv4", "ipv6", "mtu"];

        assert_eq!(
            writable, expected,
            "ethernet writable fields diverged from the SPEC-006 pinned set.\n\
             If you changed a field's x-netfyr-writable attribute, update this test \
             AND the apply code in netfyr-backend."
        );
    }

    /// Scenario: Ethernet read-only fields match spec pinned set (SPEC-006)
    #[test]
    fn test_ethernet_readonly_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let mut readonly: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| !i.writable))
            .collect();
        readonly.sort();

        let expected = vec![
            "carrier",
            "driver",
            "ethernet",
            "mac",
            "name",
            "type",
        ];

        assert_eq!(
            readonly, expected,
            "ethernet read-only fields diverged from the SPEC-006 pinned set.\n\
             If you changed a field's x-netfyr-writable attribute, update this test \
             AND the query code in netfyr-backend."
        );
    }

    /// Scenario: WiFi schema fields match spec pinned set (SPEC-006)
    #[test]
    fn test_wifi_schema_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("wifi").expect("wifi schema must exist");
        let mut fields: Vec<&str> = schema.field_names();
        fields.sort();

        let expected = vec![
            "carrier",
            "driver",
            "enabled",
            "ipv4",
            "ipv6",
            "mac",
            "mtu",
            "name",
            "type",
            "wifi",
        ];

        assert_eq!(
            fields, expected,
            "wifi schema fields diverged from the SPEC-006 pinned set.\n\
             If you added or removed a field in the JSON schema, update this test \
             AND the corresponding backend query/apply code."
        );
    }

    /// Scenario: WiFi writable fields match spec pinned set (SPEC-006)
    #[test]
    fn test_wifi_writable_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("wifi").expect("wifi schema must exist");
        let mut writable: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| i.writable))
            .collect();
        writable.sort();

        let expected = vec!["enabled", "ipv4", "ipv6", "mtu"];

        assert_eq!(
            writable, expected,
            "wifi writable fields diverged from the SPEC-006 pinned set.\n\
             If you changed a field's x-netfyr-writable attribute, update this test \
             AND the apply code in netfyr-backend."
        );
    }

    /// Scenario: WiFi read-only fields match spec pinned set (SPEC-006)
    #[test]
    fn test_wifi_readonly_field_set_matches_spec_pinned_set() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("wifi").expect("wifi schema must exist");
        let mut readonly: Vec<&str> = schema
            .field_names()
            .into_iter()
            .filter(|f| schema.field_info(f).is_some_and(|i| !i.writable))
            .collect();
        readonly.sort();

        let expected = vec![
            "carrier",
            "driver",
            "mac",
            "name",
            "type",
            "wifi",
        ];

        assert_eq!(
            readonly, expected,
            "wifi read-only fields diverged from the SPEC-006 pinned set.\n\
             If you changed a field's x-netfyr-writable attribute, update this test \
             AND the query code in netfyr-backend."
        );
    }

    /// Scenario: validate_writable passes for a wifi state with only writable fields
    #[test]
    fn test_valid_writable_wifi_state_passes_writable_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("ipv4", make_map(&[("addresses", Value::List(vec![Value::String("192.168.1.10/24".to_string())]))])),
            ],
        );
        assert!(
            registry.validate_writable(&state).is_ok(),
            "wifi state with only writable fields (mtu, ipv4.addresses) should pass validate_writable"
        );
    }

    /// Scenario: wifi comparison_keys for addresses returns Some(["address"])
    #[test]
    fn test_wifi_comparison_keys_addresses_returns_some_with_address_key() {
        let registry = SchemaRegistry::new();
        let keys = registry.comparison_keys("wifi", "addresses");
        assert_eq!(
            keys,
            Some(vec!["address".to_string()]),
            "comparison_keys(\"wifi\", \"addresses\") must return Some([\"address\"])"
        );
    }

    // ── Feature: IPv6-specific fields ─────────────────────────────────────────

    /// Scenario: IPv6 link_local valid enum value "eui64" is accepted
    #[test]
    fn test_ipv6_link_local_valid_enum_accepted() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("link_local", Value::String("eui64".to_string()))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "ipv6.link_local = \"eui64\" should be accepted"
        );
    }

    /// Scenario: IPv6 link_local valid enum value "none" is accepted
    #[test]
    fn test_ipv6_link_local_none_enum_accepted() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("link_local", Value::String("none".to_string()))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "ipv6.link_local = \"none\" should be accepted"
        );
    }

    /// Scenario: Invalid ipv6 link_local value is rejected with ConstraintViolation
    #[test]
    fn test_ipv6_link_local_invalid_enum_rejected() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("link_local", Value::String("random".to_string()))]))],
        );
        let result = registry.validate(&state);
        assert!(result.is_err(), "ipv6.link_local = \"random\" should be rejected");
        let errs = result.unwrap_err();
        let has_constraint = errs
            .errors()
            .iter()
            .any(|e| e.field.contains("link_local") && e.kind == ValidationErrorKind::ConstraintViolation);
        assert!(
            has_constraint,
            "expected ConstraintViolation for ipv6.link_local with invalid enum value, got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: IPv6 dad_transmits integer field is accepted
    #[test]
    fn test_ipv6_dad_transmits_accepted() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[
                ("link_local", Value::String("eui64".to_string())),
                ("dad_transmits", Value::U64(1)),
            ]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "ipv6.dad_transmits = 1 should be accepted"
        );
    }

    /// Scenario: IPv6 map-format addresses with DAD state are accepted
    #[test]
    fn test_ipv6_dad_state_in_map_address_accepted() {
        let registry = SchemaRegistry::new();
        let mut addr_map = IndexMap::new();
        addr_map.insert("address".to_string(), Value::String("2001:db8::1/64".to_string()));
        addr_map.insert("valid_lft".to_string(), Value::U64(86400));
        addr_map.insert("preferred_lft".to_string(), Value::U64(14400));
        addr_map.insert("dad_state".to_string(), Value::String("preferred".to_string()));
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("addresses", Value::List(vec![Value::Map(addr_map)]))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "IPv6 map-format address with dad_state should be accepted"
        );
    }

    /// Scenario: IPv6 route validation within the ipv6 sub-object passes
    #[test]
    fn test_ipv6_route_validation_within_sub_object() {
        let registry = SchemaRegistry::new();
        let route = make_route(Some("2001:db8::/32"), Some("fe80::1"), Some(100));
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("routes", Value::List(vec![route]))]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "IPv6 route inside ipv6 sub-object should pass validation"
        );
    }

    /// Scenario: IPv6 route without required destination is rejected
    #[test]
    fn test_ipv6_route_without_destination_rejected() {
        let registry = SchemaRegistry::new();
        let route = make_route(None, Some("fe80::1"), None);
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[("routes", Value::List(vec![route]))]))],
        );
        let result = registry.validate(&state);
        assert!(result.is_err(), "IPv6 route without destination should be rejected");
        let errs = result.unwrap_err();
        let has_missing = errs
            .errors()
            .iter()
            .any(|e| e.kind == ValidationErrorKind::MissingRequired);
        assert!(
            has_missing,
            "expected MissingRequired for missing IPv6 route destination, got: {:?}",
            errs.errors()
        );
    }

    /// Scenario: field_info for ipv4 returns Object type, writable, keep_when_absent
    #[test]
    fn test_field_info_ipv4_is_object_writable_keep_when_absent() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "ipv4")
            .expect("field_info(\"ethernet\", \"ipv4\") should return Some");
        assert_eq!(info.field_type, FieldType::Object, "ipv4 field should have FieldType::Object");
        assert!(info.writable, "ipv4 field should be writable");
        assert!(info.keep_when_absent, "ipv4 field should have keep_when_absent = true");
    }

    /// Scenario: field_info for ipv6 returns Object type, writable, keep_when_absent
    #[test]
    fn test_field_info_ipv6_is_object_writable_keep_when_absent() {
        let registry = SchemaRegistry::new();
        let info = registry
            .field_info("ethernet", "ipv6")
            .expect("field_info(\"ethernet\", \"ipv6\") should return Some");
        assert_eq!(info.field_type, FieldType::Object, "ipv6 field should have FieldType::Object");
        assert!(info.writable, "ipv6 field should be writable");
        assert!(info.keep_when_absent, "ipv6 field should have keep_when_absent = true");
    }

    /// Scenario: sub_field_info for ipv4.addresses returns comparison_keys and writable
    #[test]
    fn test_sub_field_info_ipv4_addresses() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let info = schema
            .sub_field_info("ipv4", "addresses")
            .expect("sub_field_info(\"ipv4\", \"addresses\") should return Some");
        assert_eq!(
            info.comparison_keys,
            vec!["address".to_string()],
            "ipv4.addresses should have comparison_keys=[\"address\"]"
        );
        assert!(info.writable, "ipv4.addresses should be writable");
    }

    /// Scenario: sub_field_info for ipv6.link_local returns writable and keep_when_absent
    #[test]
    fn test_sub_field_info_ipv6_link_local() {
        let registry = SchemaRegistry::new();
        let schema = registry.get_schema("ethernet").expect("ethernet schema must exist");
        let info = schema
            .sub_field_info("ipv6", "link_local")
            .expect("sub_field_info(\"ipv6\", \"link_local\") should return Some");
        assert!(info.writable, "ipv6.link_local should be writable");
        assert!(info.keep_when_absent, "ipv6.link_local should have keep_when_absent = true");
    }

    /// Scenario: WiFi validate_writable passes with writable ipv4/ipv6 sub-objects
    #[test]
    fn test_wifi_with_ipv4_ipv6_passes_writable_validation() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "wifi",
            vec![
                ("mtu", Value::U64(1500)),
                ("ipv4", make_map(&[("addresses", Value::List(vec![Value::String("10.0.0.1/24".to_string())]))])),
                ("ipv6", make_map(&[("addresses", Value::List(vec![Value::String("2001:db8::1/64".to_string())]))])),
            ],
        );
        assert!(
            registry.validate_writable(&state).is_ok(),
            "wifi state with ipv4/ipv6 sub-objects should pass validate_writable"
        );
    }

    /// Scenario: IPv6 dns_search and nat64_prefix read-only fields pass regular validation
    #[test]
    fn test_ipv6_dns_search_and_nat64_prefix_accepted() {
        let registry = SchemaRegistry::new();
        let state = make_state(
            "ethernet",
            vec![("ipv6", make_map(&[
                ("dns_search", Value::List(vec![Value::String("example.com".to_string())])),
                ("nat64_prefix", Value::String("64:ff9b::/96".to_string())),
            ]))],
        );
        assert!(
            registry.validate(&state).is_ok(),
            "ipv6 dns_search and nat64_prefix should pass regular validation"
        );
    }
}
