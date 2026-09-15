//! Core data model and YAML serialization for netfyr network device state.
//!
//! This crate is a **pure representation** layer: it defines the types
//! every other netfyr crate builds on and the YAML formats for exchanging
//! state. It deliberately does not:
//!
//! - merge or deduplicate contributions to the same device (that is the
//!   reconciliation layer, which alone can resolve which contributions
//!   target the same live device);
//! - query or depend on the live system.
//!
//! Field names, types, and writability *are* validated here: the
//! [`schema`] module embeds the JSON-Schema fragments that define the
//! data model for each entity type and collects every violation up
//! front, before any system change is attempted.
//!
//! A [`State`] is one source's contribution to one device: a device type
//! (`State::device_type`), a selector identifying the target
//! (`State::match_spec`), the configuration fields being contributed
//! (`State::fields`), plus runtime-only bookkeeping (`State::source`,
//! `State::priority`, `State::metadata`). The same type is used for
//! *desired* state (contributions from policies or dynamic providers,
//! typically a partial match and empty device type) and for *actual* state
//! (queried from the kernel, a fully populated match and device type).
//!
//! Configuration is represented as a plain ordered `Vec<State>`:
//! contributions coexist, are never merged or rejected here, and their list
//! order is significant (the kernel uses the first address on an interface
//! as the primary source address).
//!
//! # YAML formats
//!
//! Two formats differ only in whether they carry a `match:` section: the
//! query output format (what `netfyr query` prints) places the device's
//! fields at the top level; the policy format (what the user writes for
//! `netfyr apply`) adds a `match:` section identifying the target. A single
//! document may carry a top-level sequence of device mappings (one entry
//! per device); multi-document YAML (`---` separators) is not part of the
//! format and is an error. See [`from_yaml`], [`to_yaml_query`], and
//! [`to_yaml_policy`].
//!
//! # Reserved keys
//!
//! At the top level of a device mapping, the keys `match`, `type`, and
//! `kind` are reserved by the format; a device field with one of those
//! names cannot be expressed in YAML. Nested keys (inside `fields` values)
//! are not affected.
//!
//! # Value deserialization
//!
//! YAML strings are decoded with the schema at their property path. An
//! ordinary string field remains [`Value::String`] even when it resembles an
//! address; a field declared with the `ipv4-cidr` format becomes
//! [`Value::IpNetwork`]. YAML floats and nulls have no [`Value`] variant and
//! are parse errors; quote such values to store them as strings.

pub mod apply;
pub mod error;
pub mod match_spec;
pub mod schema;
pub mod source;
pub mod state;
pub mod value;
pub mod yaml;

pub use apply::{
    ApplyOptions, ApplyOutcome, MatchBy, Warning, prepare_for_apply,
    prepare_for_apply_with_registry,
};
pub use error::{Error, IndexedValidationError};
pub use match_spec::Match;
pub use schema::{DecodeMode, FieldInfo, FieldType, SchemaRegistry, ValidationError};
pub use source::Source;
pub use state::{State, StateMetadata};
pub use value::Value;
pub use yaml::{from_yaml, to_yaml_policy, to_yaml_query};
