%global base_version 0.1.0

Name:           netfyr
Version:        %{base_version}
Release:        1%{?dist}
Summary:        Declarative Linux network configuration

License:        MIT
URL:            https://github.com/netfyr/netfyr
Source0:        %{name}-%{base_version}.tar.gz
Source1:        %{name}-%{base_version}-vendor.tar.gz

ExclusiveArch:  %{rust_arches}

BuildRequires:  cargo >= 1.86
BuildRequires:  rust >= 1.86
BuildRequires:  rust-packaging >= 25
BuildRequires:  systemd-rpm-macros

%description
Netfyr is a declarative, policy-based network configuration tool for Linux.
It reads YAML policy files, reconciles them into a desired network state,
diffs against the running system, and applies changes via rtnetlink.

%package daemon
Summary:        Netfyr daemon for dynamic network configuration
Requires:       %{name} = %{version}-%{release}
Requires:       systemd

%description daemon
The netfyr daemon manages dynamic network configuration including DHCPv4.
It listens on a Varlink socket and accepts policy submissions from the
netfyr CLI. Required only when using DHCP or other dynamic factories.

%prep
%autosetup -n %{name}-%{base_version}
tar xf %{SOURCE1}
%cargo_prep -v vendor

%build
%cargo_build

# Generate man pages via direct crate invocation (do NOT use the cargo xtask
# alias — %%cargo_prep may overwrite .cargo/config.toml and remove the alias).
cargo run -p xtask -- man

%install
# Install CLI binary
install -Dpm 0755 target/release/netfyr %{buildroot}%{_bindir}/netfyr

# Install daemon binary
install -Dpm 0755 target/release/netfyr-daemon %{buildroot}%{_bindir}/netfyr-daemon

# Install man pages
install -d %{buildroot}%{_mandir}/man1
install -pm 0644 man/*.1 %{buildroot}%{_mandir}/man1/
install -d %{buildroot}%{_mandir}/man5
install -pm 0644 man/netfyr.yaml.5 %{buildroot}%{_mandir}/man5/
install -d %{buildroot}%{_mandir}/man7
install -pm 0644 man/netfyr-examples.7 %{buildroot}%{_mandir}/man7/
install -d %{buildroot}%{_mandir}/man8
install -pm 0644 man/netfyr-daemon.8 %{buildroot}%{_mandir}/man8/

# Install bash completion
install -d %{buildroot}%{_datadir}/bash-completion/completions
target/release/netfyr completions bash > %{buildroot}%{_datadir}/bash-completion/completions/netfyr

# Install systemd units
install -Dpm 0644 dist/systemd/netfyr.service %{buildroot}%{_unitdir}/netfyr.service
install -Dpm 0644 dist/systemd/netfyr.socket %{buildroot}%{_unitdir}/netfyr.socket

# Create config directories
install -d %{buildroot}%{_sysconfdir}/netfyr/policies

# Install example files
install -d %{buildroot}%{_docdir}/%{name}/examples/policies
install -pm 0644 examples/policies/*.yaml %{buildroot}%{_docdir}/%{name}/examples/policies/

# Note: LICENSE is handled by %%license in %%files — do NOT install it manually
# to avoid a conflict with the %%license directive.

%check
# Smoke-test: verify the built binaries are functional.
target/release/netfyr --help > /dev/null
target/release/netfyr-daemon --help > /dev/null

%post daemon
%systemd_post netfyr.service netfyr.socket

%preun daemon
%systemd_preun netfyr.service netfyr.socket

%postun daemon
%systemd_postun_with_restart netfyr.service netfyr.socket

%files
%license LICENSE
%{_bindir}/netfyr
%{_mandir}/man1/netfyr.1*
%{_mandir}/man1/netfyr-apply.1*
%{_mandir}/man1/netfyr-query.1*
%{_mandir}/man1/netfyr-history.1*
%{_mandir}/man1/netfyr-revert.1*
%{_mandir}/man1/netfyr-completions.1*
%{_mandir}/man1/netfyr-diagnose.1*
%{_mandir}/man1/netfyr-show.1*
%{_mandir}/man5/netfyr.yaml.5*
%{_mandir}/man7/netfyr-examples.7*
%dir %{_sysconfdir}/netfyr
%dir %{_sysconfdir}/netfyr/policies
%{_datadir}/bash-completion/completions/netfyr
%dir %{_docdir}/%{name}
%dir %{_docdir}/%{name}/examples
%{_docdir}/%{name}/examples/policies/

%files daemon
%license LICENSE
%{_bindir}/netfyr-daemon
%{_mandir}/man8/netfyr-daemon.8*
%{_unitdir}/netfyr.service
%{_unitdir}/netfyr.socket

%changelog
* Thu Apr 16 2026 Netfyr Maintainer <netfyr-maintainer@example.com> - 0.1.0-1
- Initial package
