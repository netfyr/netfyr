//! Integration tests for RPM packaging artefacts (SPEC-502).
//!
//! These tests verify that the files required for Fedora RPM packaging exist
//! at the correct paths in the workspace, are executable where required, and
//! contain the content mandated by the acceptance criteria.
//!
//! Acceptance criteria covered:
//!   - netfyr.spec exists at the workspace root
//!   - Build script exists, is executable, and reads Name/Version from the spec
//!   - dist/systemd/netfyr.service has ExecStart, Type=notify, network ordering
//!   - dist/systemd/netfyr.socket has ListenStream=/run/netfyr/netfyr.sock
//!   - Spec file has %cargo_prep, %cargo_build, %check sections
//!   - Spec file declares a daemon subpackage with correct Requires
//!   - Spec file lists %license LICENSE in both %files sections
//!   - Spec file declares config directories with %dir
//!   - Spec file includes systemd scriptlet macros
//!   - Spec file uses cargo run -p xtask (not cargo xtask alias)
//!   - Spec file has ExclusiveArch: %{rust_arches}
//!   - Spec file has BuildRequires on rust-packaging and systemd-rpm-macros
//!   - Spec file lists all required FHS paths and man pages
//!   - Spec Source1 is a vendor tarball for offline builds
//!   - .gitignore excludes the vendor/ directory

use std::fs;
use std::path::{Path, PathBuf};

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Absolute path to the workspace root (one level above xtask/).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ must have a parent directory (the workspace root)")
        .to_path_buf()
}

/// Read a file from the workspace and return its contents.
///
/// Panics with a descriptive message if the file is missing or unreadable,
/// since the test has already failed at that point.
fn read_workspace_file(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {relative}: {e} (path: {})", path.display()))
}

// ── Scenario: RPM spec file exists ───────────────────────────────────────────

/// AC: netfyr.spec must exist at the workspace root.
#[test]
fn test_spec_file_exists_at_workspace_root() {
    let spec = workspace_root().join("netfyr.spec");
    assert!(
        spec.exists(),
        "netfyr.spec must exist at workspace root ({})",
        spec.display()
    );
}

/// AC: the spec file must declare the package Name as "netfyr".
#[test]
fn test_spec_declares_name_netfyr() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("Name:") && content.contains("netfyr"),
        "netfyr.spec must declare 'Name: netfyr'"
    );
}

/// AC: the spec file must declare a Version field.
#[test]
fn test_spec_declares_version() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("Version:"),
        "netfyr.spec must declare a Version field"
    );
}

// ── Scenario: Build script exists and is executable ──────────────────────────

/// AC: scripts/build-rpm.sh must exist.
#[test]
fn test_build_script_exists() {
    let script = workspace_root().join("scripts/build-rpm.sh");
    assert!(
        script.exists(),
        "scripts/build-rpm.sh must exist ({})",
        script.display()
    );
}

/// AC: scripts/build-rpm.sh must have the executable bit set.
#[test]
#[cfg(unix)]
fn test_build_script_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let script = workspace_root().join("scripts/build-rpm.sh");
    let metadata = fs::metadata(&script).unwrap_or_else(|e| {
        panic!("cannot stat scripts/build-rpm.sh: {e}")
    });
    let mode = metadata.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/build-rpm.sh must have the executable bit set (mode: {:o})",
        mode
    );
}

/// AC: scripts/build-rpm.sh must read the Name from netfyr.spec (not hard-code it).
#[test]
fn test_build_script_reads_name_from_spec() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    // The script must grep the Name field from the spec file.
    assert!(
        content.contains("grep") && content.contains("Name:"),
        "scripts/build-rpm.sh must extract Name from netfyr.spec using grep"
    );
}

/// AC: scripts/build-rpm.sh must read the Version from netfyr.spec (not hard-code it).
#[test]
fn test_build_script_reads_version_from_spec() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("grep") && content.contains("Version:"),
        "scripts/build-rpm.sh must extract Version from netfyr.spec using grep"
    );
}

/// AC: build script must create a source tarball with git archive.
#[test]
fn test_build_script_uses_git_archive_for_source_tarball() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("git archive"),
        "scripts/build-rpm.sh must use 'git archive' to create the source tarball"
    );
}

/// AC: build script must create a vendor tarball for offline cargo builds.
#[test]
fn test_build_script_creates_vendor_tarball() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("cargo vendor"),
        "scripts/build-rpm.sh must run 'cargo vendor' to create the vendor tarball"
    );
    assert!(
        content.contains("vendor.tar.gz") || content.contains("vendor/"),
        "scripts/build-rpm.sh must package the vendor directory into a tarball"
    );
}

/// AC: build script must invoke rpmbuild to produce the RPMs.
#[test]
fn test_build_script_invokes_rpmbuild() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("rpmbuild"),
        "scripts/build-rpm.sh must invoke 'rpmbuild' to build the RPMs"
    );
}

// ── Scenario: Systemd service unit ───────────────────────────────────────────

/// AC: dist/systemd/netfyr.service must exist.
#[test]
fn test_service_unit_exists() {
    let service = workspace_root().join("dist/systemd/netfyr.service");
    assert!(
        service.exists(),
        "dist/systemd/netfyr.service must exist ({})",
        service.display()
    );
}

/// AC: ExecStart must point to /usr/bin/netfyr-daemon.
#[test]
fn test_service_unit_exec_start_points_to_daemon_binary() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("ExecStart=/usr/bin/netfyr-daemon"),
        "dist/systemd/netfyr.service must have 'ExecStart=/usr/bin/netfyr-daemon'"
    );
}

/// AC: Type must be "notify" for sd_notify integration.
#[test]
fn test_service_unit_type_is_notify() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("Type=notify"),
        "dist/systemd/netfyr.service must have 'Type=notify'"
    );
}

/// AC: service unit must declare network ordering (After=network-pre.target).
#[test]
fn test_service_unit_orders_after_network_pre_target() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("After=network-pre.target"),
        "dist/systemd/netfyr.service must have 'After=network-pre.target'"
    );
}

/// AC: service unit must declare Before=network.target.
#[test]
fn test_service_unit_orders_before_network_target() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("Before=network.target"),
        "dist/systemd/netfyr.service must have 'Before=network.target'"
    );
}

// ── Scenario: Systemd socket unit ────────────────────────────────────────────

/// AC: dist/systemd/netfyr.socket must exist.
#[test]
fn test_socket_unit_exists() {
    let socket = workspace_root().join("dist/systemd/netfyr.socket");
    assert!(
        socket.exists(),
        "dist/systemd/netfyr.socket must exist ({})",
        socket.display()
    );
}

/// AC: ListenStream must point to /run/netfyr/netfyr.sock.
#[test]
fn test_socket_unit_listen_stream_is_varlink_socket() {
    let content = read_workspace_file("dist/systemd/netfyr.socket");
    assert!(
        content.contains("ListenStream=/run/netfyr/netfyr.sock"),
        "dist/systemd/netfyr.socket must have 'ListenStream=/run/netfyr/netfyr.sock'"
    );
}

/// AC: socket unit must appear in [Socket] section.
#[test]
fn test_socket_unit_has_socket_section() {
    let content = read_workspace_file("dist/systemd/netfyr.socket");
    assert!(
        content.contains("[Socket]"),
        "dist/systemd/netfyr.socket must have a [Socket] section"
    );
}

// ── Scenario: Spec file — build macros ───────────────────────────────────────

/// AC: %cargo_prep must be called in %prep to set up offline Cargo builds.
#[test]
fn test_spec_has_cargo_prep_in_prep_section() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%cargo_prep"),
        "netfyr.spec must call %cargo_prep in %prep"
    );
}

/// AC: %cargo_build must be called in %build.
#[test]
fn test_spec_has_cargo_build_in_build_section() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%cargo_build"),
        "netfyr.spec must use %cargo_build macro in %build"
    );
}

/// AC: man page generation must use "cargo run -p xtask", not "cargo xtask"
///     (the alias may not exist after %cargo_prep overwrites .cargo/config.toml).
///
/// Note: the spec may mention "cargo xtask" inside a comment warning against it;
/// only non-comment lines are checked.
#[test]
fn test_spec_uses_cargo_run_p_xtask_not_alias() {
    let content = read_workspace_file("netfyr.spec");

    // Check that no non-comment line invokes "cargo xtask" as a command.
    let non_comment_invokes_alias = content.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.starts_with('#') && trimmed.contains("cargo xtask")
    });
    assert!(
        !non_comment_invokes_alias,
        "netfyr.spec must NOT invoke 'cargo xtask' alias on a non-comment line; \
         use 'cargo run -p xtask' instead"
    );

    // Check that the spec actually does invoke the xtask via cargo run.
    assert!(
        content.contains("cargo run") && content.contains("xtask"),
        "netfyr.spec must use 'cargo run -p xtask -- man' for man page generation"
    );
}

/// AC: %check section must exist and smoke-test the binaries.
#[test]
fn test_spec_has_check_section() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%check"),
        "netfyr.spec must have a %check section"
    );
}

/// AC: %check must at minimum verify the daemon binary runs.
#[test]
fn test_spec_check_smoke_tests_daemon_binary() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("netfyr-daemon --help") || content.contains("netfyr-daemon --version"),
        "netfyr.spec %check must smoke-test the netfyr-daemon binary"
    );
}

// ── Scenario: Spec file — subpackage declaration ──────────────────────────────

/// AC: spec must declare a "daemon" subpackage.
#[test]
fn test_spec_declares_daemon_subpackage() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%package daemon"),
        "netfyr.spec must declare a daemon subpackage with '%package daemon'"
    );
}

/// AC: daemon subpackage must Require the base netfyr package at the same version.
#[test]
fn test_spec_daemon_subpackage_requires_base_package() {
    let content = read_workspace_file("netfyr.spec");
    // Look for a Requires line that references %{name} (the base package).
    assert!(
        content.contains("Requires:") && content.contains("%{name}"),
        "netfyr-daemon subpackage must declare 'Requires: %{{name}} = %{{version}}-%{{release}}'"
    );
}

/// AC: daemon subpackage must Require systemd.
#[test]
fn test_spec_daemon_subpackage_requires_systemd() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("Requires:") && content.contains("systemd"),
        "netfyr-daemon subpackage must declare 'Requires: systemd'"
    );
}

// ── Scenario: Spec file — %files sections ────────────────────────────────────

/// AC: both %files sections must carry a %license LICENSE directive.
#[test]
fn test_spec_has_license_directive_in_both_files_sections() {
    let content = read_workspace_file("netfyr.spec");
    let count = content.matches("%license LICENSE").count();
    assert!(
        count >= 2,
        "netfyr.spec must have '%license LICENSE' in both %files and %files daemon sections \
         (found {} occurrence(s))",
        count
    );
}

/// AC: %files must include the CLI binary via %{_bindir}/netfyr.
#[test]
fn test_spec_files_includes_cli_binary() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_bindir}/netfyr"),
        "netfyr.spec %files must include '%{{_bindir}}/netfyr'"
    );
}

/// AC: %files daemon must include the daemon binary via %{_bindir}/netfyr-daemon.
#[test]
fn test_spec_files_daemon_includes_daemon_binary() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_bindir}/netfyr-daemon"),
        "netfyr.spec %files daemon must include '%{{_bindir}}/netfyr-daemon'"
    );
}

/// AC: %files must list the section-1 man pages.
#[test]
fn test_spec_files_includes_section_1_man_pages() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_mandir}/man1/netfyr.1"),
        "netfyr.spec %files must include netfyr.1 man page"
    );
    assert!(
        content.contains("%{_mandir}/man1/netfyr-apply.1"),
        "netfyr.spec %files must include netfyr-apply.1 man page"
    );
    assert!(
        content.contains("%{_mandir}/man1/netfyr-query.1"),
        "netfyr.spec %files must include netfyr-query.1 man page"
    );
}

/// AC: %files must list the section-5 YAML reference man page.
#[test]
fn test_spec_files_includes_section_5_man_page() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_mandir}/man5/netfyr.yaml.5"),
        "netfyr.spec %files must include netfyr.yaml.5 man page"
    );
}

/// AC: %files must list the section-7 examples man page.
#[test]
fn test_spec_files_includes_section_7_man_page() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_mandir}/man7/netfyr-examples.7"),
        "netfyr.spec %files must include netfyr-examples.7 man page"
    );
}

/// AC: %files daemon must list the systemd service unit.
#[test]
fn test_spec_files_daemon_includes_service_unit() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_unitdir}/netfyr.service"),
        "netfyr.spec %files daemon must include '%{{_unitdir}}/netfyr.service'"
    );
}

/// AC: %files daemon must list the systemd socket unit.
#[test]
fn test_spec_files_daemon_includes_socket_unit() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_unitdir}/netfyr.socket"),
        "netfyr.spec %files daemon must include '%{{_unitdir}}/netfyr.socket'"
    );
}

/// AC: %files must declare /etc/netfyr and /etc/netfyr/policies/ with %dir
///     (so they are owned by the package but their contents are not tracked —
///      user config files survive uninstall).
#[test]
fn test_spec_files_declares_config_directories_with_dir() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%dir %{_sysconfdir}/netfyr"),
        "netfyr.spec must declare config directory '/etc/netfyr' with %dir"
    );
    assert!(
        content.contains("%dir %{_sysconfdir}/netfyr/policies"),
        "netfyr.spec must declare config directory '/etc/netfyr/policies' with %dir"
    );
}

// ── Scenario: Spec file — systemd scriptlets ──────────────────────────────────

/// AC: spec must have a %post daemon section that calls %systemd_post.
#[test]
fn test_spec_has_post_daemon_section_with_systemd_post() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%post daemon"),
        "netfyr.spec must have a '%post daemon' section"
    );
    assert!(
        content.contains("%systemd_post"),
        "netfyr.spec '%post daemon' must call %systemd_post"
    );
}

/// AC: spec must have a %preun daemon section that calls %systemd_preun.
#[test]
fn test_spec_has_preun_daemon_section_with_systemd_preun() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%preun daemon"),
        "netfyr.spec must have a '%preun daemon' section"
    );
    assert!(
        content.contains("%systemd_preun"),
        "netfyr.spec '%preun daemon' must call %systemd_preun"
    );
}

/// AC: spec must have a %postun daemon section that calls %systemd_postun_with_restart.
#[test]
fn test_spec_has_postun_daemon_section_with_systemd_postun() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%postun daemon"),
        "netfyr.spec must have a '%postun daemon' section"
    );
    assert!(
        content.contains("%systemd_postun_with_restart"),
        "netfyr.spec '%postun daemon' must call %systemd_postun_with_restart"
    );
}

// ── Scenario: Spec file — architecture and build requirements ─────────────────

/// AC: ExclusiveArch must be set to %{rust_arches} so the package only builds
///     on architectures supported by Rust.
#[test]
fn test_spec_has_exclusive_arch_rust_arches() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("ExclusiveArch:") && content.contains("%{rust_arches}"),
        "netfyr.spec must have 'ExclusiveArch: %{{rust_arches}}'"
    );
}

/// AC: BuildRequires must include rust-packaging (provides %cargo_build etc.).
#[test]
fn test_spec_build_requires_rust_packaging() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("BuildRequires:") && content.contains("rust-packaging"),
        "netfyr.spec must have 'BuildRequires: rust-packaging'"
    );
}

/// AC: BuildRequires must include systemd-rpm-macros (provides %systemd_post etc.).
#[test]
fn test_spec_build_requires_systemd_rpm_macros() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("BuildRequires:") && content.contains("systemd-rpm-macros"),
        "netfyr.spec must have 'BuildRequires: systemd-rpm-macros'"
    );
}

// ── Scenario: Spec file — source tarballs ─────────────────────────────────────

/// AC: Source0 must be a .tar.gz (created by git archive).
#[test]
fn test_spec_source0_is_tar_gz() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("Source0:") && content.contains(".tar.gz"),
        "netfyr.spec must declare 'Source0' as a .tar.gz tarball"
    );
}

/// AC: Source1 must be a vendor tarball so %cargo_prep can work offline.
#[test]
fn test_spec_source1_is_vendor_tarball() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("Source1:") && content.contains("vendor"),
        "netfyr.spec must declare 'Source1' as a vendor tarball for offline Cargo builds"
    );
}

// ── Scenario: .gitignore excludes vendor/ ────────────────────────────────────

/// AC: vendor/ must be excluded from git to prevent thousands of vendored crate
///     files from being committed (the build script creates the vendor tarball
///     on the fly from 'cargo vendor').
#[test]
fn test_gitignore_excludes_vendor_directory() {
    let gitignore_path = workspace_root().join(".gitignore");
    assert!(
        gitignore_path.exists(),
        ".gitignore must exist at the workspace root"
    );
    let content = read_workspace_file(".gitignore");
    let excludes_vendor = content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "/vendor" || trimmed == "vendor/" || trimmed == "vendor"
    });
    assert!(
        excludes_vendor,
        ".gitignore must exclude the vendor/ directory to prevent vendored crates from \
         being committed (add '/vendor' or 'vendor/')"
    );
}

// ── Scenario: Spec file — LICENSE must not be manually installed ──────────────

/// AC: the spec comment must acknowledge that LICENSE is handled by %license,
///     and no manual install of LICENSE should be present in %install.
///
/// A manual `install ... LICENSE` alongside a `%license LICENSE` directive
/// causes an RPM packaging error ("listed twice").  The spec must rely solely
/// on the %license macro.
#[test]
fn test_spec_does_not_manually_install_license_file() {
    let content = read_workspace_file("netfyr.spec");
    // Collect lines from the %install section that manually copy the LICENSE file.
    // We look for "install" followed by "LICENSE" on the same line.
    let manual_install = content.lines().any(|line| {
        let l = line.trim();
        // Skip comment lines.
        !l.starts_with('#') && l.contains("install") && l.contains("LICENSE")
    });
    assert!(
        !manual_install,
        "netfyr.spec must NOT manually 'install' the LICENSE file in %install; \
         use only the '%license LICENSE' directive in %files"
    );
}

// ── Scenario: examples/policies/ must exist ──────────────────────────────────

/// AC: the spec installs examples from examples/policies/*.yaml, so that
///     directory must exist and contain at least one example policy file.
#[test]
fn test_examples_policies_directory_exists_with_files() {
    let examples_dir = workspace_root().join("examples/policies");
    assert!(
        examples_dir.exists() && examples_dir.is_dir(),
        "examples/policies/ must exist (spec installs files from it)"
    );
    let yaml_files: Vec<_> = fs::read_dir(&examples_dir)
        .expect("cannot read examples/policies/")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .collect();
    assert!(
        !yaml_files.is_empty(),
        "examples/policies/ must contain at least one .yaml policy file \
         (spec installs examples/policies/*.yaml)"
    );
}

// ── Scenario: Service unit — Restart and directory directives ────────────────

/// AC: service unit must restart on failure (Restart=on-failure).
#[test]
fn test_service_unit_restarts_on_failure() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("Restart=on-failure"),
        "dist/systemd/netfyr.service must have 'Restart=on-failure'"
    );
}

/// AC: service unit must declare RuntimeDirectory=netfyr so systemd creates
///     /run/netfyr/ before the daemon starts (the Varlink socket lives there).
#[test]
fn test_service_unit_declares_runtime_directory() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("RuntimeDirectory=netfyr"),
        "dist/systemd/netfyr.service must have 'RuntimeDirectory=netfyr'"
    );
}

/// AC: service unit must declare StateDirectory=netfyr so systemd creates
///     /var/lib/netfyr/ for persistent daemon state.
#[test]
fn test_service_unit_declares_state_directory() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("StateDirectory=netfyr"),
        "dist/systemd/netfyr.service must have 'StateDirectory=netfyr'"
    );
}

/// AC: service unit must be installed in multi-user.target.
#[test]
fn test_service_unit_installed_in_multi_user_target() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("WantedBy=multi-user.target"),
        "dist/systemd/netfyr.service [Install] must have 'WantedBy=multi-user.target'"
    );
}

/// AC: service unit must declare Wants=network-pre.target.
#[test]
fn test_service_unit_wants_network_pre_target() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("Wants=network-pre.target"),
        "dist/systemd/netfyr.service must have 'Wants=network-pre.target'"
    );
}

// ── Scenario: Socket unit — mode and install target ──────────────────────────

/// AC: socket unit must set SocketMode=0666 so unprivileged callers can reach
///     the Varlink socket.
#[test]
fn test_socket_unit_mode_is_0666() {
    let content = read_workspace_file("dist/systemd/netfyr.socket");
    assert!(
        content.contains("SocketMode=0666"),
        "dist/systemd/netfyr.socket must have 'SocketMode=0666'"
    );
}

/// AC: socket unit must be installed in sockets.target so it is activated at boot.
#[test]
fn test_socket_unit_installed_in_sockets_target() {
    let content = read_workspace_file("dist/systemd/netfyr.socket");
    assert!(
        content.contains("WantedBy=sockets.target"),
        "dist/systemd/netfyr.socket [Install] must have 'WantedBy=sockets.target'"
    );
}

// ── Scenario: Spec file — additional content checks ──────────────────────────

/// AC: spec file must declare License: MIT (Fedora requires SPDX identifiers).
#[test]
fn test_spec_declares_mit_license() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("License:") && content.contains("MIT"),
        "netfyr.spec must declare 'License: MIT'"
    );
}

/// AC: %prep must use %autosetup to apply patches and set up the source tree.
#[test]
fn test_spec_prep_uses_autosetup() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%autosetup"),
        "netfyr.spec %prep must use '%autosetup' to set up the source tree"
    );
}

/// AC: %files must list netfyr-history.1 man page.
#[test]
fn test_spec_files_includes_netfyr_history_man_page() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_mandir}/man1/netfyr-history.1"),
        "netfyr.spec %files must include netfyr-history.1 man page"
    );
}

/// AC: %files must list netfyr-revert.1 man page.
#[test]
fn test_spec_files_includes_netfyr_revert_man_page() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_mandir}/man1/netfyr-revert.1"),
        "netfyr.spec %files must include netfyr-revert.1 man page"
    );
}

/// AC: daemon subpackage must have a %description daemon section.
#[test]
fn test_spec_has_description_for_daemon_subpackage() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%description daemon"),
        "netfyr.spec must have a '%description daemon' section for the daemon subpackage"
    );
}

/// AC: %install must install example policies to the doc directory.
#[test]
fn test_spec_install_copies_example_policies_to_docdir() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_docdir}") && content.contains("examples/policies"),
        "netfyr.spec %install must copy example policies to %{{_docdir}}/%{{name}}/examples/policies/"
    );
}

/// AC: %files must include the example policies under %{_docdir}.
#[test]
fn test_spec_files_includes_docdir_examples() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%{_docdir}/%{name}/examples/policies/"),
        "netfyr.spec %files must list '%{{_docdir}}/%{{name}}/examples/policies/'"
    );
}

/// AC: %check must smoke-test the CLI binary (not just the daemon).
#[test]
fn test_spec_check_smoke_tests_cli_binary() {
    let content = read_workspace_file("netfyr.spec");
    // The %check section should verify both binaries work.
    // We look for netfyr --help or netfyr --version (without -daemon suffix).
    let check_start = content.find("%check").expect("%check section must exist");
    let check_end = content[check_start..]
        .find("\n%")
        .map(|i| check_start + i)
        .unwrap_or(content.len());
    let check_section = &content[check_start..check_end];
    assert!(
        check_section.contains("netfyr --help")
            || check_section.contains("netfyr --version")
            || check_section.contains("target/release/netfyr "),
        "netfyr.spec %check must smoke-test the netfyr CLI binary; %check section:\n{check_section}"
    );
}

/// AC: spec URL must reference the project's source repository.
#[test]
fn test_spec_url_references_project_repository() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("URL:") && content.contains("netfyr"),
        "netfyr.spec must have a URL: field referencing the netfyr project repository"
    );
}

/// AC: spec must have a %changelog entry.
#[test]
fn test_spec_has_changelog() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%changelog"),
        "netfyr.spec must have a %changelog section (required by Fedora packaging guidelines)"
    );
}

/// AC: spec %prep must extract the vendor tarball from Source1.
#[test]
fn test_spec_prep_extracts_vendor_tarball() {
    let content = read_workspace_file("netfyr.spec");
    let prep_start = content.find("%prep").expect("%prep section must exist");
    let build_start = content.find("%build").expect("%build section must exist");
    let prep_section = &content[prep_start..build_start];
    assert!(
        prep_section.contains("%{SOURCE1}") || prep_section.contains("SOURCE1"),
        "netfyr.spec %prep must extract the vendor tarball from Source1"
    );
}

/// AC: %cargo_prep must pass the vendor directory flag (-v vendor) so cargo
///     uses the vendored crates for an offline build.
#[test]
fn test_spec_cargo_prep_uses_vendor_flag() {
    let content = read_workspace_file("netfyr.spec");
    assert!(
        content.contains("%cargo_prep") && content.contains("vendor"),
        "netfyr.spec must call '%cargo_prep -v vendor' to enable offline cargo builds"
    );
}

/// AC: build script must copy the spec file to rpmbuild/SPECS/.
#[test]
fn test_build_script_copies_spec_to_rpmbuild_specs() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("SPECS") && (content.contains("cp") || content.contains("copy")),
        "scripts/build-rpm.sh must copy the spec file to ~/rpmbuild/SPECS/"
    );
}

/// AC: build script must create the standard rpmbuild directory tree.
#[test]
fn test_build_script_creates_rpmbuild_directory_tree() {
    let content = read_workspace_file("scripts/build-rpm.sh");
    assert!(
        content.contains("rpmbuild") && content.contains("BUILD"),
        "scripts/build-rpm.sh must create the ~/rpmbuild/{{BUILD,RPMS,SOURCES,SPECS,SRPMS}} tree"
    );
}

/// AC: service unit must have a [Service] section.
#[test]
fn test_service_unit_has_service_section() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("[Service]"),
        "dist/systemd/netfyr.service must have a [Service] section"
    );
}

/// AC: service unit must have an [Install] section.
#[test]
fn test_service_unit_has_install_section() {
    let content = read_workspace_file("dist/systemd/netfyr.service");
    assert!(
        content.contains("[Install]"),
        "dist/systemd/netfyr.service must have an [Install] section"
    );
}

/// AC: socket unit must have an [Install] section.
#[test]
fn test_socket_unit_has_install_section() {
    let content = read_workspace_file("dist/systemd/netfyr.socket");
    assert!(
        content.contains("[Install]"),
        "dist/systemd/netfyr.socket must have an [Install] section"
    );
}

/// AC: spec %install must install the CLI binary with correct permissions (0755).
#[test]
fn test_spec_install_sets_executable_permissions_for_cli_binary() {
    let content = read_workspace_file("netfyr.spec");
    let install_start = content.find("%install").expect("%install section must exist");
    let check_start = content.find("%check").expect("%check section must exist");
    let install_section = &content[install_start..check_start];
    assert!(
        install_section.contains("0755") && install_section.contains("netfyr"),
        "netfyr.spec %install must install binaries with 0755 permissions"
    );
}

/// AC: spec %install must install the daemon binary.
#[test]
fn test_spec_install_installs_daemon_binary() {
    let content = read_workspace_file("netfyr.spec");
    let install_start = content.find("%install").expect("%install section must exist");
    let check_start = content.find("%check").expect("%check section must exist");
    let install_section = &content[install_start..check_start];
    assert!(
        install_section.contains("netfyr-daemon"),
        "netfyr.spec %install must install the netfyr-daemon binary"
    );
}

/// AC: spec %install must install the systemd service unit file.
#[test]
fn test_spec_install_installs_service_unit() {
    let content = read_workspace_file("netfyr.spec");
    let install_start = content.find("%install").expect("%install section must exist");
    let check_start = content.find("%check").expect("%check section must exist");
    let install_section = &content[install_start..check_start];
    assert!(
        install_section.contains("netfyr.service"),
        "netfyr.spec %install must install the netfyr.service systemd unit"
    );
}

/// AC: spec %install must install the systemd socket unit file.
#[test]
fn test_spec_install_installs_socket_unit() {
    let content = read_workspace_file("netfyr.spec");
    let install_start = content.find("%install").expect("%install section must exist");
    let check_start = content.find("%check").expect("%check section must exist");
    let install_section = &content[install_start..check_start];
    assert!(
        install_section.contains("netfyr.socket"),
        "netfyr.spec %install must install the netfyr.socket systemd unit"
    );
}
