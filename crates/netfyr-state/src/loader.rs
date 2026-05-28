//! File and directory loading for YAML-formatted network state.
//!
//! `load_file` reads a single `.yaml`/`.yml` file (which may contain multiple
//! `---`-separated documents). `load_dir` recursively collects all such files
//! from a directory tree, skipping hidden entries, and returns a `StateSet`.

use crate::set::StateSet;
use crate::yaml::{parse_yaml, YamlError};
use crate::State;
use std::path::Path;
use walkdir::WalkDir;

/// Reads `path` and parses all YAML documents it contains.
///
/// Returns a `Vec<State>` (one entry per document). IO errors are wrapped with
/// the file path for context.
pub fn load_file(path: &Path) -> Result<Vec<State>, YamlError> {
    let content = std::fs::read_to_string(path).map_err(|source| YamlError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_yaml(&content)
}

/// Recursively loads all `.yaml` and `.yml` files from `path`.
///
/// Rules:
/// - Hidden files and directories (names starting with `.`) are skipped
///   entirely, including their subtrees.
/// - Non-YAML files are ignored.
/// - Duplicate `(entity_type, selector_key)` pairs across files are an error.
/// - An empty directory returns an empty `StateSet`.
pub fn load_dir(path: &Path) -> Result<StateSet, YamlError> {
    let mut set = StateSet::new();

    let walker = WalkDir::new(path).into_iter().filter_entry(|entry| {
        // Always descend into the root (depth == 0); the user explicitly named it.
        if entry.depth() == 0 {
            return true;
        }
        // Skip hidden files and directories (prunes entire hidden subtrees).
        entry
            .file_name()
            .to_str()
            .is_none_or(|name| !name.starts_with('.'))
    });

    for entry_result in walker {
        let entry = entry_result.map_err(|e| {
            // Extract the path before consuming the error.
            let path = e.path().map(|p| p.to_path_buf()).unwrap_or_default();
            let source = e
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("directory traversal error"));
            YamlError::Io { path, source }
        })?;

        if !entry.file_type().is_file() {
            continue;
        }

        let file_path = entry.path();
        let ext = file_path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("yaml") | Some("yml")) {
            continue;
        }

        let states = load_file(file_path)?;
        for state in states {
            let entity_type = state.entity_type.clone();
            let selector_key = state.selector.key();
            if set.get(&entity_type, &selector_key).is_some() {
                return Err(YamlError::DuplicateKey {
                    entity_type,
                    selector_key,
                    path: file_path.to_path_buf(),
                });
            }
            set.insert(state);
        }
    }

    Ok(set)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml::YamlError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Creates a unique temporary directory for a test and returns its path.
    ///
    /// Uses process ID + a monotonically-increasing counter so that parallel
    /// tests within the same process never collide.
    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("netfyr_test_{}_{}_{}", std::process::id(), n, label));
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all failed: {e}"));
        dir
    }

    // ── load_dir ──────────────────────────────────────────────────────────────

    /// Scenario: Load all YAML files from a directory — 3 entities from .yaml + .yml files
    #[test]
    fn test_load_dir_loads_all_yaml_and_yml_files_three_entities() {
        let dir = temp_dir("load_all");
        fs::write(dir.join("eth0.yaml"), "selector:\n  name: eth0\nmtu: 1500\n").unwrap();
        fs::write(dir.join("dns.yaml"), "selector:\n  name: primary\n").unwrap();
        fs::write(dir.join("bond.yml"), "selector:\n  name: bond0\n").unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 3, "expected 3 entities");
        // entity_type is empty in policy input format; selector_key comes from selector.name
        assert!(set.get("", "eth0").is_some(), "eth0 should be present");
        assert!(set.get("", "primary").is_some(), "primary should be present");
        assert!(set.get("", "bond0").is_some(), "bond0 should be present");
    }

    /// Scenario: Load multi-document file from directory — 2 entities from one file
    #[test]
    fn test_load_dir_multi_document_file_produces_two_entities() {
        let dir = temp_dir("multi_doc");
        fs::write(
            dir.join("interfaces.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 2, "expected 2 entities from a two-document file");
        assert!(set.get("", "eth0").is_some());
        assert!(set.get("", "eth1").is_some());
    }

    /// Scenario: Skip hidden files in directory — .backup.yaml is not loaded
    #[test]
    fn test_load_dir_skips_hidden_files() {
        let dir = temp_dir("hidden");
        fs::write(dir.join("eth0.yaml"), "selector:\n  name: eth0\nmtu: 1500\n").unwrap();
        fs::write(
            dir.join(".backup.yaml"),
            "selector:\n  name: backup\nmtu: 1500\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 1, "only the non-hidden file should be loaded");
        assert!(set.get("", "eth0").is_some());
        assert!(
            set.get("", "backup").is_none(),
            ".backup.yaml should have been skipped"
        );
    }

    /// Scenario: Error on duplicate entity keys within a directory
    #[test]
    fn test_load_dir_error_on_duplicate_entity_keys() {
        let dir = temp_dir("dup_key");
        fs::write(dir.join("file1.yaml"), "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();
        fs::write(dir.join("file2.yaml"), "type: ethernet\nname: eth0\nmtu: 9000\n").unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert!(result.is_err(), "duplicate entity key should return an error");
        assert!(
            matches!(result.unwrap_err(), YamlError::DuplicateKey { .. }),
            "error should be DuplicateKey"
        );
    }

    /// Scenario: Error on invalid YAML syntax in a file
    #[test]
    fn test_load_dir_error_on_invalid_yaml_syntax() {
        let dir = temp_dir("invalid_yaml");
        // Unclosed flow sequence is definitively invalid YAML.
        fs::write(dir.join("bad.yaml"), "[unclosed bracket\n").unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert!(result.is_err(), "invalid YAML should return an error");
    }

    /// Scenario: Empty directory returns empty StateSet with len() 0
    #[test]
    fn test_load_dir_empty_directory_returns_empty_stateset() {
        let dir = temp_dir("empty");

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir on empty directory should succeed");
        assert!(set.is_empty(), "StateSet should be empty for an empty directory");
        assert_eq!(set.len(), 0);
    }

    /// Non-YAML files (e.g., .txt, .json) are ignored by load_dir
    #[test]
    fn test_load_dir_ignores_non_yaml_files() {
        let dir = temp_dir("non_yaml");
        fs::write(dir.join("eth0.yaml"), "type: ethernet\nname: eth0\n").unwrap();
        fs::write(dir.join("readme.txt"), "This is not YAML config\n").unwrap();
        fs::write(dir.join("config.json"), "{\"type\": \"ethernet\"}\n").unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 1, "only the .yaml file should be loaded");
    }

    // ── load_file ─────────────────────────────────────────────────────────────

    /// load_file reads a single YAML file and returns all contained states
    #[test]
    fn test_load_file_single_document_returns_one_state() {
        let dir = temp_dir("load_file_single");
        let path = dir.join("eth0.yaml");
        fs::write(&path, "selector:\n  name: eth0\nmtu: 1500\n").unwrap();

        let result = load_file(&path);
        let _ = fs::remove_dir_all(&dir);

        let states = result.expect("load_file should succeed");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].entity_type, "");
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
    }

    /// Scenario: load_file multi-document — two states from one file with separator
    #[test]
    fn test_load_file_multi_document_returns_two_states() {
        let dir = temp_dir("load_file_multi");
        let path = dir.join("interfaces.yaml");
        fs::write(
            &path,
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        )
        .unwrap();

        let result = load_file(&path);
        let _ = fs::remove_dir_all(&dir);

        let states = result.expect("load_file should succeed for multi-document YAML");
        assert_eq!(states.len(), 2, "expected 2 states from a two-document file");
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
        assert_eq!(states[1].selector.name, Some("eth1".to_string()));
    }

    /// load_file on a non-existent path returns YamlError::Io
    #[test]
    fn test_load_file_nonexistent_path_returns_io_error() {
        let path = PathBuf::from("/nonexistent/path/does_not_exist.yaml");
        let result = load_file(&path);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), YamlError::Io { .. }),
            "expected Io error for missing file"
        );
    }

    // ── SPEC-005: selector sub-mapping format tests ───────────────────────────
    //
    // The policy input YAML format uses a "selector:" sub-mapping rather than
    // top-level selector keys. entity_type is always empty after parsing
    // (determined later by the backend). The StateSet key is ("", selector_key).

    /// Scenario: Load all YAML files from a directory (selector sub-mapping format)
    /// Three files (.yaml and .yml) → StateSet contains 3 entities
    #[test]
    fn test_load_dir_selector_submapping_three_entities() {
        let dir = temp_dir("submap_load_all");
        fs::write(
            dir.join("eth0.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n",
        )
        .unwrap();
        fs::write(
            dir.join("dns.yaml"),
            "selector:\n  name: primary\n",
        )
        .unwrap();
        fs::write(
            dir.join("bond.yml"),
            "selector:\n  name: bond0\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 3, "expected 3 entities");
        // entity_type is empty in selector sub-mapping format
        assert!(set.get("", "eth0").is_some(), "eth0 entity should be present");
        assert!(set.get("", "primary").is_some(), "primary entity should be present");
        assert!(set.get("", "bond0").is_some(), "bond0 entity should be present");
    }

    /// Scenario: Load multi-document file from directory (selector sub-mapping format)
    /// One file with two documents → StateSet contains 2 entities
    #[test]
    fn test_load_dir_selector_submapping_multi_document_two_entities() {
        let dir = temp_dir("submap_multi_doc");
        fs::write(
            dir.join("interfaces.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n---\nselector:\n  name: eth1\nmtu: 9000\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 2, "expected 2 entities from a two-document file");
        assert!(set.get("", "eth0").is_some());
        assert!(set.get("", "eth1").is_some());
    }

    /// Scenario: Skip hidden files in directory (selector sub-mapping format)
    /// Hidden file .backup.yaml is not loaded; only the visible file is
    #[test]
    fn test_load_dir_selector_submapping_skips_hidden_files() {
        let dir = temp_dir("submap_hidden");
        fs::write(
            dir.join("eth0.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n",
        )
        .unwrap();
        fs::write(
            dir.join(".backup.yaml"),
            "selector:\n  name: backup\nmtu: 1500\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 1, "only the non-hidden file should be loaded");
        assert!(set.get("", "eth0").is_some());
        assert!(
            set.get("", "backup").is_none(),
            ".backup.yaml should have been skipped"
        );
    }

    /// Scenario: Error on duplicate entity keys within a directory
    /// Two files with the same selector name → DuplicateKey error
    #[test]
    fn test_load_dir_selector_submapping_duplicate_entity_key_is_error() {
        let dir = temp_dir("submap_dup_key");
        fs::write(
            dir.join("file1.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n",
        )
        .unwrap();
        fs::write(
            dir.join("file2.yaml"),
            "selector:\n  name: eth0\nmtu: 9000\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert!(result.is_err(), "duplicate entity key should return an error");
        assert!(
            matches!(result.unwrap_err(), YamlError::DuplicateKey { .. }),
            "error should be DuplicateKey variant"
        );
    }

    /// Scenario: Error on invalid YAML syntax (selector sub-mapping format)
    /// A file with a parse error causes load_dir to fail
    #[test]
    fn test_load_dir_selector_submapping_invalid_yaml_is_error() {
        let dir = temp_dir("submap_invalid_yaml");
        fs::write(dir.join("bad.yaml"), "[unclosed bracket\n").unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        assert!(result.is_err(), "invalid YAML should return an error");
    }

    /// Scenario: Empty directory returns empty StateSet
    #[test]
    fn test_load_dir_selector_submapping_empty_directory_returns_empty_stateset() {
        let dir = temp_dir("submap_empty");

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir on empty directory should succeed");
        assert!(set.is_empty(), "StateSet should be empty for an empty directory");
        assert_eq!(set.len(), 0);
    }

    /// Scenario: load_file with selector sub-mapping format returns correct State fields
    #[test]
    fn test_load_file_selector_submapping_entity_type_empty_and_selector_name_set() {
        let dir = temp_dir("submap_load_file");
        let path = dir.join("eth0.yaml");
        fs::write(&path, "selector:\n  name: eth0\nmtu: 1500\n").unwrap();

        let result = load_file(&path);
        let _ = fs::remove_dir_all(&dir);

        let states = result.expect("load_file should succeed");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].entity_type, "", "entity_type must be empty");
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
        assert_eq!(states[0].fields["mtu"].value, crate::Value::U64(1500));
    }

    /// Scenario: Skip hidden subdirectory in load_dir
    /// A hidden subdirectory and all its contents are excluded
    #[test]
    fn test_load_dir_selector_submapping_skips_hidden_subdirectory() {
        let dir = temp_dir("submap_hidden_dir");
        fs::write(
            dir.join("eth0.yaml"),
            "selector:\n  name: eth0\nmtu: 1500\n",
        )
        .unwrap();
        let hidden_sub = dir.join(".hidden");
        fs::create_dir_all(&hidden_sub).unwrap();
        fs::write(
            hidden_sub.join("eth1.yaml"),
            "selector:\n  name: eth1\nmtu: 9000\n",
        )
        .unwrap();

        let result = load_dir(&dir);
        let _ = fs::remove_dir_all(&dir);

        let set = result.expect("load_dir should succeed");
        assert_eq!(set.len(), 1, "hidden subdirectory contents should be excluded");
        assert!(set.get("", "eth0").is_some());
        assert!(set.get("", "eth1").is_none(), "eth1 inside .hidden/ should be skipped");
    }
}
