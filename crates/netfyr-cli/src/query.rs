//! Implementation of the `netfyr query` subcommand.
//!
//! Two runtime modes are supported, detected automatically:
//!
//! 1. **Daemon-free**: Connection to daemon fails → query kernel directly via netlink.
//! 2. **Daemon**: Connection succeeds → query daemon via Varlink.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use indexmap::IndexMap;

use netfyr_backend::{BackendError, BackendRegistry, NetlinkBackend};
use netfyr_state::{MacAddr, Selector, State};
use netfyr_varlink::{VarlinkClient, VarlinkError, VarlinkSelector, VarlinkState};

use crate::daemon_socket_path;

/// Valid selector keys for the `--selector` / `-s` flag.
const VALID_SELECTOR_KEYS: &[&str] = &["type", "name", "driver", "mac", "pci_path"];

// ── Output format ─────────────────────────────────────────────────────────────

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Yaml,
    Json,
}

// ── CLI argument struct ───────────────────────────────────────────────────────

#[derive(Args)]
pub struct QueryArgs {
    /// Selector filters (can be specified multiple times, AND logic).
    /// Format: key=value (e.g., name=eth0, type=ethernet, driver=ixgbe)
    #[arg(long, short = 's', value_parser = parse_selector)]
    pub selector: Vec<(String, String)>,

    /// Output format: yaml (default), json
    #[arg(long, short = 'o', default_value = "yaml")]
    pub output: OutputFormat,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Run the `query` subcommand.
///
/// Parses selectors, detects daemon vs. daemon-free mode, queries the system
/// state, and prints the results in the requested format.
pub async fn run_query(args: QueryArgs) -> Result<ExitCode> {
    let (entity_type, selector) = extract_type_and_selector(&args.selector)?;

    // Detect runtime mode: try connecting to the daemon socket.
    let socket_path = daemon_socket_path();
    match VarlinkClient::connect(&socket_path).await {
        Ok(mut client) => {
            return run_query_daemon(
                &mut client,
                entity_type.as_deref(),
                selector.as_ref(),
                &args.output,
            )
            .await;
        }
        Err(VarlinkError::ConnectionFailed(_)) => {
            // Socket not found or connection refused — fall through to daemon-free mode.
        }
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context("unexpected error connecting to daemon socket")
            );
        }
    }

    // Daemon-free mode: query kernel directly via netlink.
    run_query_local(entity_type.as_deref(), selector.as_ref(), &args.output).await
}

// ── Selector parsing ──────────────────────────────────────────────────────────

/// clap value_parser for `--selector key=value` arguments.
///
/// Validates that the key is in `VALID_SELECTOR_KEYS` at parse time, producing
/// a clap-style error before the async runtime starts. Value validation (e.g.,
/// MAC address format) is deferred to `extract_type_and_selector`.
fn parse_selector(s: &str) -> Result<(String, String), String> {
    let eq = s.find('=').ok_or_else(|| {
        format!(
            "selector must be in key=value format, got: {:?}. Valid keys: {}",
            s,
            VALID_SELECTOR_KEYS.join(", ")
        )
    })?;
    let key = &s[..eq];
    let value = &s[eq + 1..];

    if !VALID_SELECTOR_KEYS.contains(&key) {
        return Err(format!(
            "invalid selector key {:?}; valid keys: {}",
            key,
            VALID_SELECTOR_KEYS.join(", ")
        ));
    }

    Ok((key.to_string(), value.to_string()))
}

/// Splits `type=X` from the selector list and builds a `Selector` from the
/// remaining fields (name, driver, mac, pci_path).
///
/// Returns `(entity_type, selector)`:
/// - `entity_type`: the value of `type=` if present, else `None`
/// - `selector`: a `Selector` built from non-type fields, or `None` if there are none
fn extract_type_and_selector(
    selectors: &[(String, String)],
) -> Result<(Option<String>, Option<Selector>)> {
    let mut entity_type: Option<String> = None;
    let mut remaining: Vec<(&str, &str)> = Vec::new();

    for (key, value) in selectors {
        if key == "type" {
            entity_type = Some(value.clone());
        } else {
            remaining.push((key, value));
        }
    }

    if remaining.is_empty() {
        return Ok((entity_type, None));
    }

    // Build a Selector from the remaining key-value pairs.
    let mut sel = Selector::new();
    for (key, value) in &remaining {
        match *key {
            "name" => sel.name = Some(value.to_string()),
            "driver" => sel.driver = Some(value.to_string()),
            "pci_path" => sel.pci_path = Some(value.to_string()),
            "mac" => {
                sel.mac = Some(value.parse::<MacAddr>().map_err(|e| {
                    anyhow::anyhow!("invalid MAC address {:?}: {}", value, e)
                })?);
            }
            // `parse_selector` already validates keys, but handle defensively.
            other => anyhow::bail!("unsupported selector key: {:?}", other),
        }
    }

    Ok((entity_type, Some(sel)))
}

// ── Daemon-free mode ──────────────────────────────────────────────────────────

async fn run_query_local(
    entity_type: Option<&str>,
    selector: Option<&Selector>,
    output: &OutputFormat,
) -> Result<ExitCode> {
    let registry = create_backend_registry();

    let maps: Vec<IndexMap<String, serde_json::Value>> = if let Some(et) = entity_type {
        let et = et.to_string();
        match registry.query(&et, selector).await {
            Ok(state_set) => state_set.iter().map(state_to_flat_map).collect(),
            Err(BackendError::UnsupportedEntityType(t)) => {
                let mut valid = registry.supported_entities();
                valid.sort();
                eprintln!(
                    "Error: unknown entity type {:?}. Valid types: {}",
                    t,
                    valid.join(", ")
                );
                return Ok(ExitCode::from(2u8));
            }
            // NotFound means an entity type is known but no entity matched the
            // selector — treat this as an empty result (exit 0), not an error.
            Err(BackendError::NotFound { .. }) => Vec::new(),
            Err(e) => {
                return Err(anyhow::Error::from(e).context("backend query failed"));
            }
        }
    } else {
        // No entity type specified — query all supported entity types and merge.
        // Iterating per entity type delegates driver/mac/pci_path selector
        // matching to the backend (which reads sysfs), avoiding the broken
        // post-filter path where State.selector only carries name.
        let mut all_maps: Vec<IndexMap<String, serde_json::Value>> = Vec::new();
        let mut entity_types = registry.supported_entities();
        entity_types.sort(); // deterministic output order
        for et in entity_types {
            match registry.query(&et, selector).await {
                Ok(state_set) => {
                    all_maps.extend(state_set.iter().map(state_to_flat_map));
                }
                Err(BackendError::NotFound { .. }) => {}
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("backend query failed for entity type {et:?}")));
                }
            }
        }
        all_maps
    };

    print_output(&maps, output)?;
    Ok(ExitCode::from(0u8))
}

fn create_backend_registry() -> BackendRegistry {
    let mut registry = BackendRegistry::new();
    // NetlinkBackend is the only backend; registration cannot fail for a single backend.
    registry
        .register(Arc::new(NetlinkBackend::new()))
        .expect("failed to register NetlinkBackend");
    registry
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_query_daemon(
    client: &mut VarlinkClient,
    entity_type: Option<&str>,
    selector: Option<&Selector>,
    output: &OutputFormat,
) -> Result<ExitCode> {
    let vs = build_varlink_selector(entity_type, selector);
    let states = client
        .query(vs.as_ref())
        .await
        .context("daemon query failed")?;

    let maps: Vec<IndexMap<String, serde_json::Value>> =
        states.iter().map(varlink_state_to_flat_map).collect();

    print_output(&maps, output)?;
    Ok(ExitCode::from(0u8))
}

/// Build a `VarlinkSelector` from separate entity type and selector arguments.
///
/// Returns `None` when both arguments are `None` — this signals "query all" to
/// the daemon. Otherwise constructs a selector with the non-None fields set.
fn build_varlink_selector(
    entity_type: Option<&str>,
    selector: Option<&Selector>,
) -> Option<VarlinkSelector> {
    if entity_type.is_none() && selector.is_none() {
        return None;
    }

    let (name, driver, mac, pci_path) = selector
        .map(|sel| {
            (
                sel.name.clone(),
                sel.driver.clone(),
                sel.mac.as_ref().map(|m| m.to_string()),
                sel.pci_path.clone(),
            )
        })
        .unwrap_or_default();

    Some(VarlinkSelector {
        entity_type: entity_type.map(str::to_string),
        name,
        driver,
        mac,
        pci_path,
        ..Default::default()
    })
}

// ── Output formatting ─────────────────────────────────────────────────────────

/// Converts a `State` to a flat ordered map with `"type"` first, followed by
/// all field values. Strips `FieldValue` wrappers (provenance is not user-facing).
fn state_to_flat_map(state: &State) -> IndexMap<String, serde_json::Value> {
    let mut map = IndexMap::new();
    // "type" first for human readability.
    map.insert(
        "type".to_string(),
        serde_json::Value::String(state.entity_type.clone()),
    );
    for (key, fv) in &state.fields {
        let json_val = serde_json::to_value(&fv.value).unwrap_or(serde_json::Value::Null);
        map.insert(key.clone(), json_val);
    }
    map
}

/// Converts a `VarlinkState` to a flat ordered map with `"type"` first.
///
/// `VarlinkState.fields` is already `serde_json::Map<String, serde_json::Value>`,
/// so no value conversion is needed — only reordering into an `IndexMap`.
fn varlink_state_to_flat_map(vs: &VarlinkState) -> IndexMap<String, serde_json::Value> {
    let mut map = IndexMap::new();
    map.insert(
        "type".to_string(),
        serde_json::Value::String(vs.entity_type.clone()),
    );
    for (key, val) in &vs.fields {
        map.insert(key.clone(), val.clone());
    }
    map
}

/// Serialize and print the list of flat maps in the requested format.
fn print_output(maps: &[IndexMap<String, serde_json::Value>], format: &OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Yaml => {
            let yaml = serde_yaml::to_string(maps).context("failed to serialize output as YAML")?;
            print!("{}", yaml);
        }
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(maps).context("failed to serialize output as JSON")?;
            println!("{}", json);
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, Value};

    fn fv(v: Value) -> FieldValue {
        FieldValue { value: v, provenance: Provenance::KernelDefault }
    }

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut field_map = IndexMap::new();
        for (k, v) in fields {
            field_map.insert(k.to_string(), fv(v));
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        }
    }

    // ── parse_selector tests ──────────────────────────────────────────────────

    /// AC: Invalid selector key shows error listing valid selector keys (type, name, driver, mac, pci_path).
    #[test]
    fn test_parse_selector_invalid_key_returns_error_listing_valid_keys() {
        let result = parse_selector("invalid_key=value");
        assert!(result.is_err(), "invalid key must return an error");
        let err = result.unwrap_err();
        assert!(
            err.contains("type")
                && err.contains("name")
                && err.contains("driver")
                && err.contains("mac")
                && err.contains("pci_path"),
            "error must list all valid keys: type, name, driver, mac, pci_path; got: {err}"
        );
    }

    /// Selector without a '=' sign is rejected with an error.
    #[test]
    fn test_parse_selector_missing_equals_returns_error() {
        let result = parse_selector("nameeth0");
        assert!(result.is_err(), "missing '=' must return an error");
    }

    /// AC: Valid key 'name' is accepted and parsed as (name, value).
    #[test]
    fn test_parse_selector_name_key_returns_tuple() {
        let result = parse_selector("name=eth0");
        assert!(result.is_ok(), "name=eth0 must be accepted; err={:?}", result.err());
        assert_eq!(result.unwrap(), ("name".to_string(), "eth0".to_string()));
    }

    /// AC: Valid key 'type' is accepted.
    #[test]
    fn test_parse_selector_type_key_returns_tuple() {
        let result = parse_selector("type=ethernet");
        assert!(result.is_ok(), "type=ethernet must be accepted");
        assert_eq!(result.unwrap(), ("type".to_string(), "ethernet".to_string()));
    }

    /// AC: Valid key 'driver' is accepted.
    #[test]
    fn test_parse_selector_driver_key_returns_tuple() {
        let result = parse_selector("driver=ixgbe");
        assert!(result.is_ok(), "driver=ixgbe must be accepted");
        assert_eq!(result.unwrap(), ("driver".to_string(), "ixgbe".to_string()));
    }

    /// AC: Valid key 'mac' is accepted (value validation is deferred).
    #[test]
    fn test_parse_selector_mac_key_returns_tuple() {
        let result = parse_selector("mac=aa:bb:cc:dd:ee:ff");
        assert!(result.is_ok(), "mac=aa:bb:cc:dd:ee:ff must be accepted at parse time");
        assert_eq!(result.unwrap(), ("mac".to_string(), "aa:bb:cc:dd:ee:ff".to_string()));
    }

    /// AC: Valid key 'pci_path' is accepted.
    #[test]
    fn test_parse_selector_pci_path_key_returns_tuple() {
        let result = parse_selector("pci_path=0000:03:00.0");
        assert!(result.is_ok(), "pci_path=0000:03:00.0 must be accepted");
        assert_eq!(result.unwrap(), ("pci_path".to_string(), "0000:03:00.0".to_string()));
    }

    /// An empty value after '=' is valid (key= is allowed).
    #[test]
    fn test_parse_selector_empty_value_is_valid() {
        let result = parse_selector("name=");
        assert!(result.is_ok(), "empty value must be accepted; err={:?}", result.err());
        assert_eq!(result.unwrap(), ("name".to_string(), "".to_string()));
    }

    // ── extract_type_and_selector tests ──────────────────────────────────────

    /// Empty selector list returns (None, None).
    #[test]
    fn test_extract_type_and_selector_empty_list_returns_none_none() {
        let (entity_type, selector) = extract_type_and_selector(&[]).unwrap();
        assert!(entity_type.is_none(), "entity_type must be None for empty input");
        assert!(selector.is_none(), "selector must be None for empty input");
    }

    /// AC: type=ethernet only → (Some("ethernet"), None).
    #[test]
    fn test_extract_type_and_selector_type_only_returns_entity_type_none_selector() {
        let selectors = vec![("type".to_string(), "ethernet".to_string())];
        let (entity_type, selector) = extract_type_and_selector(&selectors).unwrap();
        assert_eq!(entity_type, Some("ethernet".to_string()));
        assert!(selector.is_none(), "selector must be None when only type= is present");
    }

    /// AC: name=eth0 only → (None, Some(Selector { name: "eth0" })).
    #[test]
    fn test_extract_type_and_selector_name_only_builds_named_selector() {
        let selectors = vec![("name".to_string(), "eth0".to_string())];
        let (entity_type, selector) = extract_type_and_selector(&selectors).unwrap();
        assert!(entity_type.is_none());
        let sel = selector.expect("selector must be Some when name= is given");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    /// AC: type=ethernet + name=eth0 splits correctly: type extracted, name goes to Selector.
    #[test]
    fn test_extract_type_and_selector_type_and_name_splits_correctly() {
        let selectors = vec![
            ("type".to_string(), "ethernet".to_string()),
            ("name".to_string(), "eth0".to_string()),
        ];
        let (entity_type, selector) = extract_type_and_selector(&selectors).unwrap();
        assert_eq!(entity_type, Some("ethernet".to_string()));
        let sel = selector.expect("selector must be Some for name= field");
        assert_eq!(sel.name, Some("eth0".to_string()));
    }

    /// driver=ixgbe builds a Selector with driver field set.
    #[test]
    fn test_extract_type_and_selector_driver_builds_selector_with_driver() {
        let selectors = vec![("driver".to_string(), "ixgbe".to_string())];
        let (_, selector) = extract_type_and_selector(&selectors).unwrap();
        let sel = selector.expect("selector must be Some for driver= field");
        assert_eq!(sel.driver, Some("ixgbe".to_string()));
    }

    /// mac=aa:bb:cc:dd:ee:ff parses into Selector.mac correctly.
    #[test]
    fn test_extract_type_and_selector_mac_parses_correctly() {
        let selectors = vec![("mac".to_string(), "aa:bb:cc:dd:ee:ff".to_string())];
        let (_, selector) = extract_type_and_selector(&selectors).unwrap();
        let sel = selector.expect("selector must be Some for mac= field");
        let mac = sel.mac.expect("mac must be set on Selector");
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    /// AC: Invalid MAC address returns an error.
    #[test]
    fn test_extract_type_and_selector_invalid_mac_returns_error() {
        let selectors = vec![("mac".to_string(), "not-a-mac".to_string())];
        let result = extract_type_and_selector(&selectors);
        assert!(result.is_err(), "invalid MAC address must return an error");
    }

    /// pci_path=0000:03:00.0 builds a Selector with pci_path set.
    #[test]
    fn test_extract_type_and_selector_pci_path_builds_selector() {
        let selectors = vec![("pci_path".to_string(), "0000:03:00.0".to_string())];
        let (_, selector) = extract_type_and_selector(&selectors).unwrap();
        let sel = selector.expect("selector must be Some for pci_path= field");
        assert_eq!(sel.pci_path, Some("0000:03:00.0".to_string()));
    }

    /// AC: Multiple non-type selectors (AND logic) — name + driver both set on Selector.
    #[test]
    fn test_extract_type_and_selector_multiple_non_type_fields_builds_combined_selector() {
        let selectors = vec![
            ("name".to_string(), "eth0".to_string()),
            ("driver".to_string(), "ixgbe".to_string()),
        ];
        let (entity_type, selector) = extract_type_and_selector(&selectors).unwrap();
        assert!(entity_type.is_none());
        let sel = selector.expect("selector must be Some");
        assert_eq!(sel.name, Some("eth0".to_string()));
        assert_eq!(sel.driver, Some("ixgbe".to_string()));
    }

    // ── build_varlink_selector tests ──────────────────────────────────────────

    /// AC: Both None → None, meaning "query all" (no filter sent to daemon).
    #[test]
    fn test_build_varlink_selector_both_none_returns_none() {
        let result = build_varlink_selector(None, None);
        assert!(result.is_none(), "both None must return None to signal 'query all'");
    }

    /// entity_type only → Some VarlinkSelector with entity_type set.
    #[test]
    fn test_build_varlink_selector_entity_type_only_sets_entity_type() {
        let result = build_varlink_selector(Some("ethernet"), None);
        let vs = result.expect("must return Some when entity_type is given");
        assert_eq!(vs.entity_type, Some("ethernet".to_string()));
        assert!(vs.name.is_none());
        assert!(vs.driver.is_none());
    }

    /// Selector with name only → Some VarlinkSelector with name set.
    #[test]
    fn test_build_varlink_selector_name_selector_only_sets_name() {
        let sel = Selector::with_name("eth0");
        let result = build_varlink_selector(None, Some(&sel));
        let vs = result.expect("must return Some when selector is given");
        assert_eq!(vs.name, Some("eth0".to_string()));
        assert!(vs.entity_type.is_none());
    }

    /// Both entity_type and selector → merged into a single VarlinkSelector.
    #[test]
    fn test_build_varlink_selector_entity_type_and_selector_merged() {
        let sel = Selector::with_name("eth0");
        let result = build_varlink_selector(Some("ethernet"), Some(&sel));
        let vs = result.expect("must return Some");
        assert_eq!(vs.entity_type, Some("ethernet".to_string()));
        assert_eq!(vs.name, Some("eth0".to_string()));
    }

    /// Selector with driver → VarlinkSelector.driver is set.
    #[test]
    fn test_build_varlink_selector_driver_selector_sets_driver() {
        let sel = Selector { driver: Some("ixgbe".to_string()), ..Default::default() };
        let result = build_varlink_selector(None, Some(&sel));
        let vs = result.expect("must return Some");
        assert_eq!(vs.driver, Some("ixgbe".to_string()));
    }

    // ── state_to_flat_map tests ───────────────────────────────────────────────

    /// AC: Output shows type first, then config fields at the top level.
    #[test]
    fn test_state_to_flat_map_type_is_the_first_key() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let map = state_to_flat_map(&state);
        let first_key = map.keys().next().expect("map must not be empty");
        assert_eq!(first_key, "type", "\"type\" must be the first key in the flat map");
    }

    /// type value matches entity_type on the State.
    #[test]
    fn test_state_to_flat_map_type_value_matches_entity_type() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let map = state_to_flat_map(&state);
        assert_eq!(map["type"], serde_json::json!("ethernet"));
    }

    /// AC: Config fields appear at the top level — mtu, carrier, name all included.
    #[test]
    fn test_state_to_flat_map_includes_all_field_values_at_top_level() {
        let state = make_state(
            "ethernet",
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),
                ("carrier", Value::Bool(true)),
                ("name", Value::String("eth0".to_string())),
            ],
        );
        let map = state_to_flat_map(&state);
        assert_eq!(map.get("mtu"), Some(&serde_json::json!(1500u64)));
        assert_eq!(map.get("carrier"), Some(&serde_json::json!(true)));
        assert_eq!(map.get("name"), Some(&serde_json::json!("eth0")));
    }

    /// Provenance (internal detail) is not included in the flat map output.
    #[test]
    fn test_state_to_flat_map_does_not_include_provenance() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let map = state_to_flat_map(&state);
        assert!(
            !map.contains_key("provenance"),
            "provenance must not appear in the flat map; keys: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// State with no fields yields a flat map containing only "type".
    #[test]
    fn test_state_to_flat_map_no_fields_yields_only_type() {
        let state = make_state("ethernet", "eth0", vec![]);
        let map = state_to_flat_map(&state);
        assert_eq!(map.len(), 1, "flat map with no fields must have exactly 1 key ('type')");
        assert!(map.contains_key("type"));
    }

    // ── varlink_state_to_flat_map tests ──────────────────────────────────────

    use netfyr_varlink::VarlinkState;
    use netfyr_varlink::types::VarlinkSelector as VarlinkSel;

    fn make_varlink_state(
        entity_type: &str,
        fields: Vec<(&str, serde_json::Value)>,
    ) -> VarlinkState {
        let mut field_map = serde_json::Map::new();
        for (k, v) in fields {
            field_map.insert(k.to_string(), v);
        }
        VarlinkState {
            entity_type: entity_type.to_string(),
            selector: VarlinkSel::default(),
            fields: field_map,
        }
    }

    /// AC (daemon mode): "type" is first key in flat map from VarlinkState.
    #[test]
    fn test_varlink_state_to_flat_map_type_is_first_key() {
        let vs = make_varlink_state("ethernet", vec![("mtu", serde_json::json!(1500u64))]);
        let map = varlink_state_to_flat_map(&vs);
        let first_key = map.keys().next().expect("map must not be empty");
        assert_eq!(first_key, "type", "\"type\" must be the first key in the flat map");
    }

    /// AC (daemon mode): "type" value matches entity_type from VarlinkState.
    #[test]
    fn test_varlink_state_to_flat_map_type_value_matches_entity_type() {
        let vs = make_varlink_state("ethernet", vec![("mtu", serde_json::json!(1500u64))]);
        let map = varlink_state_to_flat_map(&vs);
        assert_eq!(map["type"], serde_json::json!("ethernet"));
    }

    /// AC (daemon mode): All fields from VarlinkState appear at top level of flat map.
    #[test]
    fn test_varlink_state_to_flat_map_includes_all_fields_at_top_level() {
        let vs = make_varlink_state(
            "ethernet",
            vec![
                ("mtu", serde_json::json!(1500u64)),
                ("carrier", serde_json::json!(true)),
                ("name", serde_json::json!("eth0")),
            ],
        );
        let map = varlink_state_to_flat_map(&vs);
        assert_eq!(map.get("mtu"), Some(&serde_json::json!(1500u64)));
        assert_eq!(map.get("carrier"), Some(&serde_json::json!(true)));
        assert_eq!(map.get("name"), Some(&serde_json::json!("eth0")));
    }

    /// AC (daemon mode): VarlinkState with no fields yields a flat map with only "type".
    #[test]
    fn test_varlink_state_to_flat_map_empty_fields_yields_only_type() {
        let vs = make_varlink_state("ethernet", vec![]);
        let map = varlink_state_to_flat_map(&vs);
        assert_eq!(
            map.len(),
            1,
            "flat map with no fields must have exactly 1 key (\"type\")"
        );
        assert!(map.contains_key("type"));
    }

    // ── daemon_socket_path tests ──────────────────────────────────────────────

    // Env var mutations are serialised to avoid races between parallel test threads.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// AC: socket path defaults to /run/netfyr/netfyr.sock when env var is absent.
    #[test]
    fn test_daemon_socket_path_returns_default_when_env_not_set() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let original = std::env::var("NETFYR_SOCKET_PATH").ok();
        std::env::remove_var("NETFYR_SOCKET_PATH");
        let path = daemon_socket_path();
        match original {
            Some(v) => std::env::set_var("NETFYR_SOCKET_PATH", v),
            None => std::env::remove_var("NETFYR_SOCKET_PATH"),
        }
        assert_eq!(path, "/run/netfyr/netfyr.sock");
    }

    /// AC: socket path uses NETFYR_SOCKET_PATH env var when set (enables test overrides).
    #[test]
    fn test_daemon_socket_path_uses_env_var_when_set() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let original = std::env::var("NETFYR_SOCKET_PATH").ok();
        std::env::set_var("NETFYR_SOCKET_PATH", "/tmp/custom-test.sock");
        let path = daemon_socket_path();
        match original {
            Some(v) => std::env::set_var("NETFYR_SOCKET_PATH", v),
            None => std::env::remove_var("NETFYR_SOCKET_PATH"),
        }
        assert_eq!(path, "/tmp/custom-test.sock");
    }

    // ── build_varlink_selector: additional field coverage ────────────────────

    use netfyr_state::MacAddr;

    /// AC: Selector.mac (MacAddr) is serialized to a string in VarlinkSelector.mac.
    #[test]
    fn test_build_varlink_selector_mac_in_selector_serialized_to_string() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().expect("valid MAC");
        let sel = Selector { mac: Some(mac), ..Default::default() };
        let result = build_varlink_selector(None, Some(&sel));
        let vs = result.expect("must return Some when selector has mac");
        assert_eq!(vs.mac, Some("aa:bb:cc:dd:ee:ff".to_string()));
    }

    /// AC: pci_path field in Selector is forwarded to VarlinkSelector.pci_path.
    #[test]
    fn test_build_varlink_selector_pci_path_is_forwarded() {
        let sel = Selector { pci_path: Some("0000:03:00.0".to_string()), ..Default::default() };
        let result = build_varlink_selector(None, Some(&sel));
        let vs = result.expect("must return Some when selector has pci_path");
        assert_eq!(vs.pci_path, Some("0000:03:00.0".to_string()));
    }

    // ── Serialization / output format tests ──────────────────────────────────

    /// AC: YAML output format produces valid parseable YAML (sequence).
    #[test]
    fn test_yaml_serialization_of_flat_maps_produces_valid_yaml_sequence() {
        let mut map = IndexMap::new();
        map.insert("type".to_string(), serde_json::json!("ethernet"));
        map.insert("mtu".to_string(), serde_json::json!(1500u64));

        let yaml = serde_yaml::to_string(&vec![map]).expect("must serialize to YAML");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&yaml).expect("must parse as valid YAML");
        assert!(parsed.is_sequence(), "YAML output must be a sequence");
    }

    /// AC: JSON output format produces valid parseable JSON array.
    #[test]
    fn test_json_serialization_of_flat_maps_produces_valid_json_array() {
        let mut map = IndexMap::new();
        map.insert("type".to_string(), serde_json::json!("ethernet"));
        map.insert("mtu".to_string(), serde_json::json!(1500u64));

        let json =
            serde_json::to_string_pretty(&vec![map]).expect("must serialize to JSON");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("must parse as valid JSON");
        assert!(parsed.is_array(), "JSON output must be an array");
    }

    /// AC: No matching entities → empty list (YAML empty sequence).
    #[test]
    fn test_empty_flat_maps_serializes_to_empty_yaml_sequence() {
        let maps: Vec<IndexMap<String, serde_json::Value>> = vec![];
        let yaml = serde_yaml::to_string(&maps).expect("must serialize");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&yaml).expect("must be valid YAML");
        assert!(parsed.is_sequence(), "empty list must produce a YAML sequence");
        assert!(
            parsed.as_sequence().unwrap().is_empty(),
            "empty input must produce an empty YAML sequence"
        );
    }

    /// AC: No matching entities → empty JSON array.
    #[test]
    fn test_empty_flat_maps_serializes_to_empty_json_array() {
        let maps: Vec<IndexMap<String, serde_json::Value>> = vec![];
        let json = serde_json::to_string_pretty(&maps).expect("must serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("must be valid JSON");
        assert!(parsed.is_array(), "empty input must produce a JSON array");
        assert!(
            parsed.as_array().unwrap().is_empty(),
            "empty input must produce an empty JSON array"
        );
    }

    /// JSON output is a pretty-printed array (suitable for piping to jq).
    #[test]
    fn test_json_output_is_pretty_printed_array_for_jq_compatibility() {
        let mut map = IndexMap::new();
        map.insert("type".to_string(), serde_json::json!("ethernet"));
        map.insert("mtu".to_string(), serde_json::json!(1500u64));

        let json = serde_json::to_string_pretty(&vec![map]).expect("must serialize");
        // jq requires the input to be a valid JSON value.
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("must be parseable by jq");
        let arr = parsed.as_array().expect("top-level must be an array");
        assert_eq!(
            arr[0]["mtu"].as_u64(),
            Some(1500),
            "jq '.[].mtu' must produce 1500"
        );
    }
}
