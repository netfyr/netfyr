//! Policy store — persists submitted policies to disk as individual YAML files.
//!
//! The store manages a directory (typically `/var/lib/netfyr/policies/`) where
//! each policy is written as `{policy_name}.yaml`. Replace-all semantics ensure
//! that a `netfyr apply` always replaces the entire policy set atomically: new
//! files are written to `.yaml.tmp` first, then renamed, then stale files are
//! removed. A crash at any point leaves the directory in a recoverable state.
//!
//! An ephemeral store (`PolicyStore::ephemeral`) operates entirely in memory
//! with no filesystem I/O, used for dry-run operations.

use anyhow::Context;
use netfyr_policy::{parse_policy_yaml, Policy};
use netfyr_state::serialize_state_to_value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

// ── Name sanitization ─────────────────────────────────────────────────────────

/// Sanitizes a policy name so it is safe to use as a filename stem.
///
/// Keeps lowercase ASCII letters, digits, hyphens, and underscores. All other
/// characters (including uppercase letters, spaces, slashes, dots) are replaced
/// with `_`. An empty result falls back to `"_unnamed"`.
fn sanitize_policy_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "_unnamed".to_string()
    } else {
        sanitized
    }
}

// ── Serialization helper ──────────────────────────────────────────────────────

/// Serializes a `Policy` to a YAML string in the standard policy file format.
///
/// The output matches the format that `parse_policy_yaml` / `load_policy_file`
/// can parse back, so a persisted file can be copied directly to
/// `/etc/netfyr/policies/` and used with `netfyr apply`.
///
/// The YAML document structure is:
/// ```yaml
/// kind: policy
/// name: <name>
/// factory: <factory>
/// priority: <priority>
/// selector: ...   # omitted if None
/// state: ...      # omitted if None
/// states: ...     # omitted if None
/// ```
fn serialize_policy_to_string(policy: &Policy) -> anyhow::Result<String> {
    let mut map = serde_yaml::Mapping::new();

    map.insert(
        serde_yaml::Value::String("kind".to_string()),
        serde_yaml::Value::String("policy".to_string()),
    );

    map.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(policy.name.clone()),
    );

    // FactoryType derives Serialize with rename_all = "lowercase":
    // Static → "static", Dhcpv4 → "dhcpv4".
    let factory_value =
        serde_yaml::to_value(&policy.factory_type).context("failed to serialize factory type")?;
    map.insert(
        serde_yaml::Value::String("factory".to_string()),
        factory_value,
    );

    map.insert(
        serde_yaml::Value::String("priority".to_string()),
        serde_yaml::Value::Number(serde_yaml::Number::from(u64::from(policy.priority))),
    );

    // Selector derives Serialize with skip_serializing_if on all optional fields,
    // so this produces a clean mapping with only set fields.
    if let Some(selector) = &policy.selector {
        let sel_value =
            serde_yaml::to_value(selector).context("failed to serialize selector")?;
        map.insert(serde_yaml::Value::String("selector".to_string()), sel_value);
    }

    // serialize_state_to_value produces the flat format: type, selector fields,
    // and configuration fields all at the top level (no nested provenance metadata).
    if let Some(state) = &policy.state {
        let state_value = serialize_state_to_value(state);
        map.insert(serde_yaml::Value::String("state".to_string()), state_value);
    }

    if let Some(states) = &policy.states {
        let states_seq: Vec<serde_yaml::Value> =
            states.iter().map(serialize_state_to_value).collect();
        map.insert(
            serde_yaml::Value::String("states".to_string()),
            serde_yaml::Value::Sequence(states_seq),
        );
    }

    serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .context("failed to serialize policy to YAML")
}

// ── PolicyStore ───────────────────────────────────────────────────────────────

/// Disk-backed policy store with replace-all semantics.
///
/// Persists policies as individual `{name}.yaml` files in a directory. Supports
/// atomic per-file writes via `.yaml.tmp` → rename. An ephemeral store (`dir:
/// None`) operates in memory only, used for dry-run operations.
pub struct PolicyStore {
    /// Backing directory. `None` for ephemeral (in-memory only) stores.
    dir: Option<PathBuf>,
    /// Current policy set in load or submission order.
    policies: Vec<Policy>,
}

impl PolicyStore {
    /// Opens (or creates) the store directory and loads all existing policies.
    ///
    /// Creates the directory with `fs::create_dir_all` if it does not exist.
    /// Scans for `*.yaml` files, sorts them lexicographically, and parses each.
    /// Malformed files are skipped with a warning — partial recovery is better
    /// than failing to start.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create policy store directory {}", dir.display()))?;

        // Collect non-directory entries with a `.yaml` extension (not `.yaml.tmp`).
        let mut entries: Vec<_> = fs::read_dir(dir)
            .with_context(|| format!("failed to read policy store directory {}", dir.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
            .filter(|e| {
                let fname = e.file_name();
                let s = fname.to_string_lossy();
                // Accept only exact .yaml extension — .yaml.tmp ends with .tmp, not .yaml.
                s.ends_with(".yaml")
            })
            .collect();

        // Sort lexicographically by filename for deterministic load order.
        entries.sort_by_key(|e| e.file_name());

        let mut policies = Vec::new();
        for entry in entries {
            let path = entry.path();
            let fname = entry.file_name();
            let fname_display = fname.to_string_lossy();

            let contents = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Skipping policy file {}: failed to read: {}",
                        fname_display,
                        e
                    );
                    continue;
                }
            };

            match parse_policy_yaml(&contents) {
                Ok(parsed) => policies.extend(parsed),
                Err(e) => {
                    tracing::warn!(
                        "Skipping malformed policy file {}: {}",
                        fname_display,
                        e
                    );
                }
            }
        }

        let count = policies.len();
        tracing::debug!(count, "loaded policies from directory");
        Ok(PolicyStore {
            dir: Some(dir.to_path_buf()),
            policies,
        })
    }

    /// Creates an ephemeral, in-memory-only store (no disk I/O).
    ///
    /// Used for dry-run operations where policies are evaluated but not
    /// persisted.
    pub fn ephemeral(policies: Vec<Policy>) -> Self {
        PolicyStore { dir: None, policies }
    }

    /// Replaces all policies with a new set. Persists to disk atomically.
    ///
    /// **Algorithm** (disk-backed store):
    /// 1. Write each new policy to `{name}.yaml.tmp` (fail fast on error).
    /// 2. Rename each `.tmp` → `.yaml` (fail fast on error).
    /// 3. Remove stale `.yaml` files not in the new set (best-effort).
    /// 4. Remove any leftover `.yaml.tmp` files (best-effort).
    /// 5. Update in-memory state and return the previous policy set.
    ///
    /// `self.policies` is only updated if all writes and renames succeed.
    ///
    /// **Ephemeral store**: updates in-memory only, always returns `Ok`.
    pub fn replace_all(&mut self, new_policies: Vec<Policy>) -> anyhow::Result<Vec<Policy>> {
        // Ephemeral: in-memory only, no disk I/O.
        let Some(ref dir_owned) = self.dir else {
            let old = std::mem::replace(&mut self.policies, new_policies);
            return Ok(old);
        };
        let dir = dir_owned.clone();

        // Compute sanitized filename stem for each policy.
        let sanitized_names: Vec<String> =
            new_policies.iter().map(|p| sanitize_policy_name(&p.name)).collect();

        // Warn about collisions (two policies that sanitize to the same name).
        let mut seen_names: HashSet<&str> = HashSet::new();
        for (sanitized, policy) in sanitized_names.iter().zip(new_policies.iter()) {
            if !seen_names.insert(sanitized.as_str()) {
                tracing::warn!(
                    "Policy '{}' sanitizes to '{}' which conflicts with another policy; \
                     last write wins",
                    policy.name,
                    sanitized
                );
            }
        }

        // ── Step 1: Write phase — write each policy to a .yaml.tmp file ──────────
        for (sanitized, policy) in sanitized_names.iter().zip(new_policies.iter()) {
            let content = serialize_policy_to_string(policy)?;
            let tmp_path = dir.join(format!("{}.yaml.tmp", sanitized));
            fs::write(&tmp_path, content)
                .with_context(|| format!("failed to write policy to {}", tmp_path.display()))?;
        }

        // ── Step 2: Rename phase — rename .tmp → .yaml ────────────────────────
        for sanitized in &sanitized_names {
            let tmp_path = dir.join(format!("{}.yaml.tmp", sanitized));
            let final_path = dir.join(format!("{}.yaml", sanitized));
            fs::rename(&tmp_path, &final_path).with_context(|| {
                format!(
                    "failed to rename {} to {}",
                    tmp_path.display(),
                    final_path.display()
                )
            })?;
        }

        // ── Step 3: Remove stale .yaml files (best-effort) ────────────────────
        let new_filenames: HashSet<String> =
            sanitized_names.iter().map(|name| format!("{}.yaml", name)).collect();

        let mut removed: usize = 0;
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname_str = fname.to_string_lossy();
                if fname_str.ends_with(".yaml") && !new_filenames.contains(fname_str.as_ref()) {
                    match fs::remove_file(entry.path()) {
                        Ok(()) => removed += 1,
                        Err(e) => {
                            tracing::warn!(
                                "failed to remove stale policy file {}: {}",
                                entry.path().display(),
                                e
                            );
                        }
                    }
                }
            }
        }

        // ── Step 4: Clean up leftover .yaml.tmp files (best-effort) ──────────
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname_str = fname.to_string_lossy();
                if fname_str.ends_with(".yaml.tmp") {
                    if let Err(e) = fs::remove_file(entry.path()) {
                        tracing::warn!(
                            "failed to remove leftover tmp file {}: {}",
                            entry.path().display(),
                            e
                        );
                    }
                }
            }
        }

        // ── Step 5: Update in-memory state ────────────────────────────────────
        let old_policies = std::mem::replace(&mut self.policies, new_policies);
        let written = sanitized_names.len();
        tracing::debug!(written, removed, "policies persisted");
        Ok(old_policies)
    }

    /// Returns the current set of policies.
    pub fn policies(&self) -> &[Policy] {
        &self.policies
    }

    /// Returns `true` if the store contains no policies.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Returns the number of policies in the store.
    pub fn len(&self) -> usize {
        self.policies.len()
    }
}

// ── PolicyStore tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_policy::{parse_policy_yaml, FactoryType, Policy};
    use std::fs;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Parse a minimal valid `kind: policy` YAML document with the given name.
    fn make_policy(name: &str) -> Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: static\npriority: 100\n\
             state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n"
        );
        parse_policy_yaml(&yaml).unwrap().into_iter().next().unwrap()
    }

    /// Build a policy from the standard template but override the `name` field.
    ///
    /// This allows names containing characters that would be awkward in YAML
    /// (e.g., spaces or slashes) while still producing a structurally valid Policy.
    fn make_policy_named(name: &str) -> Policy {
        let mut p = make_policy("placeholder");
        p.name = name.to_string();
        p
    }

    /// Write a minimal valid policy YAML file to `dir/filename`.
    fn write_policy_file(dir: &tempfile::TempDir, filename: &str, policy_name: &str) {
        let content = format!(
            "kind: policy\nname: {policy_name}\nfactory: static\npriority: 100\n\
             state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n"
        );
        fs::write(dir.path().join(filename), content).unwrap();
    }

    // ── Feature: Policy store initialization ──────────────────────────────────

    #[test]
    fn test_load_from_directory_with_existing_policies_loads_all_three() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "aaa.yaml", "policy-aaa");
        write_policy_file(&dir, "bbb.yaml", "policy-bbb");
        write_policy_file(&dir, "ccc.yaml", "policy-ccc");

        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store.len(), 3);
    }

    #[test]
    fn test_load_returns_policies_in_lexicographic_filename_order() {
        let dir = tempfile::tempdir().unwrap();
        // Write in reverse order; expect alphabetical (filename) load order.
        write_policy_file(&dir, "ccc.yaml", "policy-ccc");
        write_policy_file(&dir, "aaa.yaml", "policy-aaa");
        write_policy_file(&dir, "bbb.yaml", "policy-bbb");

        let store = PolicyStore::load(dir.path()).unwrap();

        let names: Vec<&str> = store.policies().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["policy-aaa", "policy-bbb", "policy-ccc"]);
    }

    #[test]
    fn test_load_from_empty_directory_returns_zero_policies() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_from_empty_directory_is_empty_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let store = PolicyStore::load(dir.path()).unwrap();

        assert!(store.is_empty());
    }

    #[test]
    fn test_load_creates_directory_if_missing() {
        let base = tempfile::tempdir().unwrap();
        let new_dir = base.path().join("policies");
        assert!(!new_dir.exists(), "directory must not exist before test");

        let store = PolicyStore::load(&new_dir).unwrap();

        assert!(new_dir.exists(), "PolicyStore::load should create the directory");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_load_skips_malformed_file_and_loads_valid_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "valid.yaml", "valid-policy");
        // Syntactically invalid YAML — serde_yaml will fail to parse this.
        fs::write(dir.path().join("corrupt.yaml"), "this: [unclosed bracket").unwrap();

        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store.len(), 1, "corrupt.yaml must be skipped; only valid.yaml loaded");
        assert_eq!(store.policies()[0].name, "valid-policy");
    }

    #[test]
    fn test_load_skips_bad_policy_document_other_policies_still_loaded() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "aaa.yaml", "policy-aaa");
        // Valid YAML structure, but not a `kind: policy` document.
        fs::write(dir.path().join("mmm.yaml"), "not_a_policy: true").unwrap();
        write_policy_file(&dir, "zzz.yaml", "policy-zzz");

        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store.len(), 2, "bad document must be skipped; aaa and zzz loaded");
    }

    #[test]
    fn test_load_ignores_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        // Place a valid policy document in a .yaml.tmp file — must be ignored.
        let tmp_content = "kind: policy\nname: policy-b\nfactory: static\npriority: 100\n\
                           state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n";
        fs::write(dir.path().join("policy-b.yaml.tmp"), tmp_content).unwrap();

        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store.len(), 1, "only the .yaml file should be loaded; .yaml.tmp ignored");
        assert_eq!(store.policies()[0].name, "policy-a");
    }

    #[test]
    fn test_load_tmp_file_content_not_surfaced_as_a_policy() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_content = "kind: policy\nname: policy-b\nfactory: static\npriority: 100\n\
                           state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n";
        fs::write(dir.path().join("policy-b.yaml.tmp"), tmp_content).unwrap();

        let store = PolicyStore::load(dir.path()).unwrap();

        assert!(
            store.policies().iter().all(|p| p.name != "policy-b"),
            ".tmp file content must not appear as a loaded policy"
        );
    }

    // ── Feature: Replace-all operation ────────────────────────────────────────

    #[test]
    fn test_replace_all_new_files_exist_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![make_policy("policy-c"), make_policy("policy-d")])
            .unwrap();

        assert!(
            dir.path().join("policy-c.yaml").exists(),
            "policy-c.yaml must be on disk after replace_all"
        );
        assert!(
            dir.path().join("policy-d.yaml").exists(),
            "policy-d.yaml must be on disk after replace_all"
        );
    }

    #[test]
    fn test_replace_all_old_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![make_policy("policy-c"), make_policy("policy-d")])
            .unwrap();

        assert!(
            !dir.path().join("policy-a.yaml").exists(),
            "policy-a.yaml must be removed after replace_all"
        );
        assert!(
            !dir.path().join("policy-b.yaml").exists(),
            "policy-b.yaml must be removed after replace_all"
        );
    }

    #[test]
    fn test_replace_all_updates_in_memory_policy_set() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![make_policy("policy-b"), make_policy("policy-c")])
            .unwrap();

        assert_eq!(store.len(), 2);
        let names: Vec<&str> = store.policies().iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"policy-b") && names.contains(&"policy-c"));
    }

    #[test]
    fn test_replace_all_returns_previous_policy_set() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        let previous = store
            .replace_all(vec![make_policy("policy-c"), make_policy("policy-d")])
            .unwrap();

        assert_eq!(previous.len(), 2);
        let prev_names: Vec<&str> = previous.iter().map(|p| p.name.as_str()).collect();
        assert!(
            prev_names.contains(&"policy-a") && prev_names.contains(&"policy-b"),
            "returned previous set must contain policy-a and policy-b"
        );
    }

    #[test]
    fn test_replace_all_no_tmp_files_remain_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![make_policy("policy-a"), make_policy("policy-b")])
            .unwrap();

        let tmp_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".yaml.tmp"))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .yaml.tmp files should remain after a successful replace_all"
        );
    }

    #[test]
    fn test_replace_all_with_empty_vec_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        write_policy_file(&dir, "policy-c.yaml", "policy-c");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![]).unwrap();

        assert!(store.is_empty(), "store must be empty after replace_all([])");
    }

    #[test]
    fn test_replace_all_with_empty_vec_removes_all_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![]).unwrap();

        let yaml_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".yaml"))
            .collect();
        assert!(yaml_files.is_empty(), "no .yaml files must remain on disk after clearing");
    }

    #[test]
    fn test_replace_all_does_not_update_in_memory_state_on_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let mut store = PolicyStore::load(dir.path()).unwrap();

        // Block the write by pre-creating a *directory* at the .yaml.tmp path.
        // fs::write will fail with EISDIR because the target exists as a directory.
        fs::create_dir(dir.path().join("policy-c.yaml.tmp")).unwrap();

        let result =
            store.replace_all(vec![make_policy("policy-c"), make_policy("policy-d")]);

        assert!(result.is_err(), "replace_all must return Err when write fails");
        assert_eq!(
            store.len(),
            2,
            "in-memory policy count must not change on write failure"
        );
        let names: Vec<&str> = store.policies().iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"policy-a"),
            "policy-a must still be in memory after failed replace_all"
        );
        assert!(
            names.contains(&"policy-b"),
            "policy-b must still be in memory after failed replace_all"
        );
    }

    #[test]
    fn test_replace_all_cleans_up_leftover_tmp_files_from_prior_crash() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate a .yaml.tmp file left by a prior incomplete replace_all.
        fs::write(dir.path().join("stale.yaml.tmp"), "leftover content from prior crash")
            .unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_policy("policy-a")]).unwrap();

        assert!(
            !dir.path().join("stale.yaml.tmp").exists(),
            "stale.yaml.tmp must be cleaned up by replace_all"
        );
        assert!(dir.path().join("policy-a.yaml").exists());
    }

    // ── Feature: Crash recovery ───────────────────────────────────────────────

    #[test]
    fn test_crash_during_write_previous_state_intact_on_restart() {
        // Simulate: A and B on disk; crash happened after writing C.yaml.tmp
        // but before renaming it — so C.yaml.tmp exists, C.yaml does not.
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        let tmp_content = "kind: policy\nname: policy-c\nfactory: static\npriority: 100\n\
                           state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n";
        fs::write(dir.path().join("policy-c.yaml.tmp"), tmp_content).unwrap();

        // Daemon restarts and loads.
        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(
            store.len(),
            2,
            "only the previous policies (A and B) should be loaded after crash recovery"
        );
        let names: Vec<&str> = store.policies().iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"policy-a") && names.contains(&"policy-b"),
            "policies A and B must be present"
        );
        assert!(
            !names.contains(&"policy-c"),
            "policy-c must not be loaded from a .tmp file"
        );
    }

    #[test]
    fn test_crash_after_rename_before_cleanup_loads_superset() {
        // Simulate: crash after C.yaml and D.yaml were renamed from .tmp,
        // but before A.yaml and B.yaml (previous set) were removed.
        // On restart all four .yaml files exist → stale-but-valid superset.
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        write_policy_file(&dir, "policy-c.yaml", "policy-c");
        write_policy_file(&dir, "policy-d.yaml", "policy-d");

        let store = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(
            store.len(),
            4,
            "all four files (stale superset) must be loaded after crash recovery"
        );
    }

    #[test]
    fn test_next_successful_replace_all_cleans_up_crash_recovery_superset() {
        // After crash-recovery superset (A+B+C+D), the next successful replace_all
        // with just C and D should remove A and B.
        let dir = tempfile::tempdir().unwrap();
        write_policy_file(&dir, "policy-a.yaml", "policy-a");
        write_policy_file(&dir, "policy-b.yaml", "policy-b");
        write_policy_file(&dir, "policy-c.yaml", "policy-c");
        write_policy_file(&dir, "policy-d.yaml", "policy-d");
        let mut store = PolicyStore::load(dir.path()).unwrap();
        assert_eq!(store.len(), 4);

        store
            .replace_all(vec![make_policy("policy-c"), make_policy("policy-d")])
            .unwrap();

        assert_eq!(store.len(), 2);
        assert!(
            !dir.path().join("policy-a.yaml").exists(),
            "stale policy-a.yaml must be cleaned up"
        );
        assert!(
            !dir.path().join("policy-b.yaml").exists(),
            "stale policy-b.yaml must be cleaned up"
        );
        assert!(dir.path().join("policy-c.yaml").exists());
        assert!(dir.path().join("policy-d.yaml").exists());
    }

    // ── Feature: File naming ──────────────────────────────────────────────────

    #[test]
    fn test_policy_name_maps_to_yaml_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_policy("office-network")]).unwrap();

        assert!(
            dir.path().join("office-network.yaml").exists(),
            "policy 'office-network' must be persisted as 'office-network.yaml'"
        );
    }

    #[test]
    fn test_policy_name_with_hyphens_preserved_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_policy("dhcp-eth0")]).unwrap();

        assert!(dir.path().join("dhcp-eth0.yaml").exists());
    }

    #[test]
    fn test_policy_name_with_underscores_preserved_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![make_policy("datacenter_spine")])
            .unwrap();

        assert!(dir.path().join("datacenter_spine.yaml").exists());
    }

    #[test]
    fn test_invalid_characters_in_policy_name_sanitized_to_underscores() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        // Space (' ') and slash ('/') are not valid — replaced with '_'.
        let policy = make_policy_named("my policy/v2");
        store.replace_all(vec![policy]).unwrap();

        let files_on_disk: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            dir.path().join("my_policy_v2.yaml").exists(),
            "policy 'my policy/v2' must be persisted as 'my_policy_v2.yaml'; found: {:?}",
            files_on_disk
        );
    }

    // ── Feature: Ephemeral store ──────────────────────────────────────────────

    #[test]
    fn test_ephemeral_store_holds_policies_in_memory() {
        let policies = vec![make_policy("policy-a"), make_policy("policy-b")];
        let store = PolicyStore::ephemeral(policies);

        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        let names: Vec<&str> = store.policies().iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"policy-a") && names.contains(&"policy-b"));
    }

    #[test]
    fn test_ephemeral_store_replace_all_updates_in_memory_only() {
        let mut store = PolicyStore::ephemeral(vec![make_policy("policy-a")]);

        let result = store.replace_all(vec![make_policy("policy-b")]);

        assert!(result.is_ok(), "ephemeral replace_all must always succeed");
        assert_eq!(store.len(), 1);
        assert_eq!(store.policies()[0].name, "policy-b");
    }

    #[test]
    fn test_ephemeral_store_replace_all_returns_previous_policies() {
        let mut store = PolicyStore::ephemeral(vec![make_policy("policy-a")]);

        let previous = store.replace_all(vec![make_policy("policy-b")]).unwrap();

        assert_eq!(previous.len(), 1);
        assert_eq!(previous[0].name, "policy-a");
    }

    // ── Feature: Persisted file format ───────────────────────────────────────

    #[test]
    fn test_persisted_file_parseable_by_spec007_policy_parser() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();
        let result = parse_policy_yaml(&content);

        assert!(
            result.is_ok(),
            "persisted file must be parseable by parse_policy_yaml: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_persisted_file_contains_kind_policy_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();

        assert!(
            content.contains("kind: policy"),
            "persisted file must contain 'kind: policy'"
        );
    }

    #[test]
    fn test_persisted_file_policy_name_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();
        let policies = parse_policy_yaml(&content).unwrap();

        assert_eq!(policies[0].name, "office-network");
    }

    #[test]
    fn test_persisted_file_factory_type_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();
        let policies = parse_policy_yaml(&content).unwrap();

        assert_eq!(policies[0].factory_type, FactoryType::Static);
    }

    #[test]
    fn test_persisted_file_priority_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();
        let policies = parse_policy_yaml(&content).unwrap();

        assert_eq!(policies[0].priority, 100);
    }

    #[test]
    fn test_persisted_file_state_or_states_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();
        store.replace_all(vec![make_policy("office-network")]).unwrap();

        let content =
            fs::read_to_string(dir.path().join("office-network.yaml")).unwrap();
        let policies = parse_policy_yaml(&content).unwrap();

        assert!(
            policies[0].state.is_some() || policies[0].states.is_some(),
            "persisted policy must have state or states present"
        );
    }

    // ── Feature: DHCPv4 policy persistence ───────────────────────────────────

    /// Build a minimal valid DHCPv4 policy with a named-interface selector.
    fn make_dhcp_policy(name: &str, interface: &str) -> Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: dhcpv4\npriority: 50\n\
             selector:\n  name: {interface}\n"
        );
        parse_policy_yaml(&yaml).unwrap().into_iter().next().unwrap()
    }

    /// Scenario: DHCPv4 factory type roundtrips through persistence.
    /// After persisting a DHCPv4 policy and reloading it, the factory type
    /// must be `FactoryType::Dhcpv4`.
    #[test]
    fn test_dhcpv4_policy_factory_type_roundtrips_through_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_dhcp_policy("dhcp-eth0", "eth0")]).unwrap();

        let content = fs::read_to_string(dir.path().join("dhcp-eth0.yaml")).unwrap();
        let loaded = parse_policy_yaml(&content).unwrap();

        assert_eq!(
            loaded[0].factory_type,
            FactoryType::Dhcpv4,
            "factory type must be Dhcpv4 after serialization roundtrip"
        );
    }

    /// Scenario: DHCPv4 policy selector interface name roundtrips.
    /// The selector's `name` field (the interface) must survive serialization.
    #[test]
    fn test_dhcpv4_policy_selector_interface_name_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_dhcp_policy("dhcp-eth0", "eth0")]).unwrap();

        let content = fs::read_to_string(dir.path().join("dhcp-eth0.yaml")).unwrap();
        let loaded = parse_policy_yaml(&content).unwrap();

        let selector = loaded[0].selector.as_ref().expect("DHCPv4 policy must have a selector");
        assert_eq!(
            selector.name.as_deref(),
            Some("eth0"),
            "selector interface name must survive the serialization roundtrip"
        );
    }

    /// Scenario: DHCPv4 policy priority roundtrips through persistence.
    #[test]
    fn test_dhcpv4_policy_priority_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_dhcp_policy("dhcp-eth0", "eth0")]).unwrap();

        let content = fs::read_to_string(dir.path().join("dhcp-eth0.yaml")).unwrap();
        let loaded = parse_policy_yaml(&content).unwrap();

        assert_eq!(
            loaded[0].priority, 50,
            "priority must survive the serialization roundtrip"
        );
    }

    /// Scenario: DHCPv4 policy persisted file is parseable by the policy parser.
    #[test]
    fn test_dhcpv4_policy_persisted_file_is_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store.replace_all(vec![make_dhcp_policy("dhcp-eth0", "eth0")]).unwrap();

        let content = fs::read_to_string(dir.path().join("dhcp-eth0.yaml")).unwrap();
        let result = parse_policy_yaml(&content);

        assert!(
            result.is_ok(),
            "persisted DHCPv4 policy file must be parseable by parse_policy_yaml: {:?}",
            result.err()
        );
    }

    /// Scenario: A PolicyStore loaded from disk with a DHCPv4 policy on restart
    /// correctly re-loads the factory type.
    #[test]
    fn test_dhcpv4_policy_survives_store_reload() {
        let dir = tempfile::tempdir().unwrap();

        // First store instance — persist a DHCPv4 policy.
        let mut store1 = PolicyStore::load(dir.path()).unwrap();
        store1.replace_all(vec![make_dhcp_policy("dhcp-eth0", "eth0")]).unwrap();
        assert_eq!(store1.len(), 1);
        drop(store1);

        // Second store instance simulates a daemon restart loading from disk.
        let store2 = PolicyStore::load(dir.path()).unwrap();

        assert_eq!(store2.len(), 1, "reloaded store must have 1 policy");
        assert_eq!(
            store2.policies()[0].factory_type,
            FactoryType::Dhcpv4,
            "reloaded policy must have Dhcpv4 factory type"
        );
        assert_eq!(
            store2.policies()[0].selector.as_ref().and_then(|s| s.name.as_deref()),
            Some("eth0"),
            "reloaded policy must have correct interface selector"
        );
    }

    /// Scenario: Mixed static and DHCPv4 policies both persist and reload correctly.
    #[test]
    fn test_mixed_static_and_dhcpv4_policies_both_persist_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolicyStore::load(dir.path()).unwrap();

        store
            .replace_all(vec![
                make_policy("static-eth1"),
                make_dhcp_policy("dhcp-eth0", "eth0"),
            ])
            .unwrap();

        // Reload to verify persistence.
        let reloaded = PolicyStore::load(dir.path()).unwrap();
        assert_eq!(reloaded.len(), 2, "both policies must be persisted and reloaded");

        let factory_types: std::collections::HashSet<String> = reloaded
            .policies()
            .iter()
            .map(|p| format!("{:?}", p.factory_type))
            .collect();
        assert!(
            factory_types.contains("Static"),
            "static factory must be present after reload"
        );
        assert!(
            factory_types.contains("Dhcpv4"),
            "DHCPv4 factory must be present after reload"
        );
    }
}
