//! Apply-path preparation: auto-generating a match for states that
//! carry none, turning `device_type` into a match constraint, and removing
//! schema-defined read-only fields with a per-key warning.
//!
//! Warnings are returned in the [`ApplyOutcome`] so that the caller
//! (the CLI, later) can decide how to report them. `Ok` with a non-empty
//! `warnings` vector is success. Warnings do not change the outcome, which
//! is what keeps `netfyr query > file && edit && netfyr apply file` usable
//! unattended.

use std::fmt;

use crate::error::{Error, IndexedValidationError};
use crate::schema::SchemaRegistry;
use crate::state::State;
use crate::value::Value;

/// Which field the auto-match reads to identify the device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatchBy {
    /// The interface name (default: human-readable, and deterministic on
    /// most distros' predictable interface naming).
    #[default]
    Name,
    /// The MAC address.
    Mac,
}

/// Options for [`prepare_for_apply`].
#[derive(Clone, Debug, Default)]
pub struct ApplyOptions {
    /// Which field the auto-match reads.
    pub match_by: MatchBy,
}

/// A per-key warning emitted when a read-only field is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The dropped field path, which may be nested.
    pub key: String,
    /// Whether the path is defined read-only by the schema.
    pub known_read_only: bool,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.known_read_only {
            write!(f, "dropping field '{}' (not settable by policy)", self.key)
        } else {
            write!(f, "dropping field '{}'", self.key)
        }
    }
}

/// The result of [`prepare_for_apply`]: the adjusted states and the
/// per-key warnings.
pub struct ApplyOutcome {
    /// The states with matches auto-generated and read-only fields dropped.
    pub states: Vec<State>,
    /// One warning per dropped field, in field order.
    pub warnings: Vec<Warning>,
}

/// Prepare states for `apply`.
///
/// Query observations lose their read-only fields with warnings. The selected
/// `name` or `mac` becomes the match, and `device_type` becomes `match.type`.
/// Explicit policies and already prepared states are validated unchanged.
/// Both input and final prepared states are schema-validated.
pub fn prepare_for_apply(states: &[State], opts: &ApplyOptions) -> Result<ApplyOutcome, Error> {
    let registry = SchemaRegistry::new();
    prepare_for_apply_with_registry(states, opts, &registry)
}

/// Prepare states for apply using a caller-composed schema registry.
///
/// This lets extensions add their own writable schema nodes without
/// duplicating the query-to-apply normalization logic.
pub fn prepare_for_apply_with_registry(
    states: &[State],
    opts: &ApplyOptions,
    registry: &SchemaRegistry,
) -> Result<ApplyOutcome, Error> {
    let mut states = states.to_vec();
    for state in &mut states {
        if state.source == crate::Source::Kernel
            && (!state.device_type.is_empty()
                || state.fields.keys().any(|key| {
                    registry
                        .field_info("base", key)
                        .is_some_and(|field| !field.writable)
                }))
        {
            state.match_spec = Default::default();
        }
    }

    let validation_errors = collect_validation_errors(&states, registry, false);
    if !validation_errors.is_empty() {
        return Err(Error::Validation(validation_errors));
    }

    let mut warnings = Vec::new();

    for (i, state) in states.iter_mut().enumerate() {
        if !state.match_spec.is_empty() {
            continue;
        }

        let match_field = match opts.match_by {
            MatchBy::Name => "name",
            MatchBy::Mac => "mac",
        };
        let value = state
            .fields
            .get(match_field)
            .cloned()
            .ok_or_else(|| Error::NoMatchField {
                field: match_field.to_string(),
                state_index: i,
            })?;
        // A strategy key that is not a string is coerced to its canonical
        // text; query output (the intended input) always carries strings.
        let match_value = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        match opts.match_by {
            MatchBy::Name => {
                state.match_spec.name = Some(match_value);
                // `name` selects the device; it is not a property to set.
                // `shift_remove` keeps the remaining fields in order.
                state.fields.shift_remove(match_field);
            }
            MatchBy::Mac => {
                state.match_spec.mac = Some(match_value);
                state.fields.shift_remove(match_field);
            }
        }

        for key in registry.remove_read_only_fields(state) {
            warnings.push(Warning {
                key,
                known_read_only: true,
            });
        }

        if !state.device_type.is_empty() {
            state.match_spec.r#type = Some(state.device_type.clone());
            state.device_type.clear();
        }
    }

    let validation_errors = collect_validation_errors(&states, registry, true);
    if !validation_errors.is_empty() {
        return Err(Error::Validation(validation_errors));
    }

    Ok(ApplyOutcome { states, warnings })
}

fn collect_validation_errors(
    states: &[State],
    registry: &SchemaRegistry,
    final_states: bool,
) -> Vec<IndexedValidationError> {
    states
        .iter()
        .enumerate()
        .flat_map(|(state_index, state)| {
            let errors = if final_states || !state.match_spec.is_empty() {
                registry.validate_writable(state)
            } else {
                registry.validate(state)
            };
            errors
                .into_iter()
                .map(move |error| IndexedValidationError { state_index, error })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::{
        ApplyOptions, ApplyOutcome, Error, IndexedValidationError, MatchBy, Warning, from_yaml,
        prepare_for_apply, to_yaml_policy,
    };

    /// Extract the error from a `prepare_for_apply` result without requiring
    /// `Debug` on `ApplyOutcome` (which the library does not derive).
    fn err_of(result: Result<ApplyOutcome, Error>) -> Error {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    /// A realistic `netfyr query` output document (no `match:` section).
    const QUERY_YAML: &str = "name: eth0\ntype: ethernet\nmac: aa:bb:cc:dd:ee:ff\ndriver: ixgbe\ncarrier: true\nenabled: true\nmtu: 9000\nipv4:\n  addresses:\n    - ip: 10.0.1.50/24\nethernet:\n  speed: 1000\n";

    #[test]
    fn query_edit_apply_default_strategy() {
        let states = from_yaml(QUERY_YAML).unwrap();
        let outcome = prepare_for_apply(&states, &ApplyOptions::default()).unwrap();
        let s = &outcome.states[0];
        // Match auto-generated from the interface name.
        assert_eq!(s.match_spec.name.as_deref(), Some("eth0"));
        // device_type became a match constraint and is no longer a policy field.
        assert_eq!(s.match_spec.r#type.as_deref(), Some("ethernet"));
        assert!(s.device_type.is_empty());
        // `name` consumed from fields (it became the match, so no warning).
        assert!(!s.fields.contains_key("name"));
        // Retained fields in their relative order.
        let keys: Vec<&str> = s.fields.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["enabled", "mtu", "ipv4"]);
        // One warning per dropped read-only field, in field order; all known.
        assert_eq!(
            outcome.warnings,
            vec![
                Warning {
                    key: "mac".to_string(),
                    known_read_only: true
                },
                Warning {
                    key: "driver".to_string(),
                    known_read_only: true
                },
                Warning {
                    key: "carrier".to_string(),
                    known_read_only: true
                },
                Warning {
                    key: "ethernet.speed".to_string(),
                    known_read_only: true
                },
            ]
        );
        for w in &outcome.warnings {
            assert_eq!(
                w.to_string(),
                format!("dropping field '{}' (not settable by policy)", w.key)
            );
        }
        // Ok with a non-empty warnings vector: warnings don't affect success.
        assert!(!outcome.warnings.is_empty());
    }

    #[test]
    fn match_by_mac_consumes_mac_and_drops_name() {
        let states = from_yaml(QUERY_YAML).unwrap();
        let opts = ApplyOptions {
            match_by: MatchBy::Mac,
        };
        let outcome = prepare_for_apply(&states, &opts).unwrap();
        let s = &outcome.states[0];
        assert_eq!(s.match_spec.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        // `mac` is consumed as the selector, like `name` in the default mode.
        assert!(!s.fields.contains_key("mac"));
        let keys: Vec<&str> = outcome.warnings.iter().map(|w| w.key.as_str()).collect();
        // `name` dropped with a known read-only warning.
        assert!(keys.contains(&"name"));
        assert!(!s.fields.contains_key("name"));
        // driver and carrier still dropped; mac was selector-consumed.
        assert!(keys.contains(&"driver"));
        assert!(keys.contains(&"carrier"));
        assert!(!keys.contains(&"mac"));
    }

    #[test]
    fn missing_strategy_key_is_nomatchfield_with_index() {
        // Single state lacking `name`.
        let states = from_yaml("mtu: 9000").unwrap();
        let err = err_of(prepare_for_apply(&states, &ApplyOptions::default()));
        assert_eq!(
            err,
            Error::NoMatchField {
                field: "name".to_string(),
                state_index: 0
            }
        );
        // Two-state list where the second entry lacks it.
        let states = from_yaml("- name: eth0\n- mtu: 9000").unwrap();
        let err = err_of(prepare_for_apply(&states, &ApplyOptions::default()));
        assert_eq!(
            err,
            Error::NoMatchField {
                field: "name".to_string(),
                state_index: 1
            }
        );
    }

    #[test]
    fn device_type_moves_to_match_and_clears_without_technology_fields() {
        let states = from_yaml("name: eth0\ntype: ethernet\nmtu: 9000").unwrap();
        let outcome = prepare_for_apply(&states, &ApplyOptions::default()).unwrap();
        let state = &outcome.states[0];

        assert_eq!(state.match_spec.r#type.as_deref(), Some("ethernet"));
        assert!(state.device_type.is_empty());
    }

    #[test]
    fn prepared_policy_serialization_is_a_fixed_point() {
        let states = from_yaml(QUERY_YAML).unwrap();
        let prepared = prepare_for_apply(&states, &ApplyOptions::default())
            .unwrap()
            .states;

        let first = to_yaml_policy(&prepared).unwrap();
        let reparsed = from_yaml(&first).unwrap();
        assert!(prepared[0].content_eq(&reparsed[0]));
        assert_eq!(first, to_yaml_policy(&reparsed).unwrap());
    }

    #[test]
    fn match_by_mac_requires_a_mac_field() {
        let states = from_yaml("name: eth0").unwrap();
        let opts = ApplyOptions {
            match_by: MatchBy::Mac,
        };

        assert_eq!(
            err_of(prepare_for_apply(&states, &opts)),
            Error::NoMatchField {
                field: "mac".to_string(),
                state_index: 0,
            }
        );
    }

    #[test]
    fn populated_match_states_are_validated_as_policies() {
        // A state with a populated match cannot set read-only fields.
        let states =
            from_yaml("match:\n  name: eth0\ncarrier: true\ndriver: ixgbe\nmtu: 9000").unwrap();
        assert!(matches!(
            err_of(prepare_for_apply(&states, &ApplyOptions::default())),
            Error::Validation(_)
        ));
    }

    #[test]
    fn matched_invalid_policies_return_all_indexed_validation_errors() {
        let states = from_yaml(
            "- match:\n    name: eth0\n  mtt: 1500\n  mtu: 99999\n- match:\n    name: eth1\n  mtu: 99999\n",
        )
        .unwrap();
        let Error::Validation(errors) =
            err_of(prepare_for_apply(&states, &ApplyOptions::default()))
        else {
            panic!("expected schema validation error");
        };
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].state_index, 0);
        assert_eq!(errors[2].state_index, 1);
    }

    #[test]
    fn apply_rejects_malformed_cidr() {
        let malformed =
            from_yaml("match:\n  name: eth0\nipv4:\n  addresses:\n    - ip: 10.0.0.1/33\n")
                .unwrap();
        let Error::Validation(errors) =
            err_of(prepare_for_apply(&malformed, &ApplyOptions::default()))
        else {
            panic!("expected schema validation error");
        };
        assert!(matches!(
            errors.as_slice(),
            [IndexedValidationError {
                state_index: 0,
                error: crate::ValidationError::InvalidFormat { .. },
            }]
        ));
    }

    #[test]
    fn explicit_policy_type_is_structural_and_accepted() {
        let typed = from_yaml("match:\n  name: eth0\ntype: ethernet\nmtu: 1500\n").unwrap();
        // `device_type` is structural metadata, not a writable field; it
        // must not be rejected as read-only.
        prepare_for_apply(&typed, &ApplyOptions::default()).unwrap();
    }

    #[test]
    fn valid_writable_policy_passes_unchanged() {
        let states = from_yaml("match:\n  name: eth0\nenabled: true\nmtu: 1500\n").unwrap();
        let outcome = prepare_for_apply(&states, &ApplyOptions::default()).unwrap();
        assert!(outcome.warnings.is_empty());
        assert!(states[0].content_eq(&outcome.states[0]));
    }
}
