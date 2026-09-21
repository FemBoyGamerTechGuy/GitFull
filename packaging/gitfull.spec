# gitfull — RPM package build (Fedora / RHEL family).
#
# Quick path: ./packaging/build-rpm.sh  (points rpmbuild's _sourcedir at
# the archive from packaging/make-archive.sh and keeps all build
# directories inside a temp stage).
# Manual path: rpmbuild -bb packaging/gitfull.spec
#   --define "_sourcedir <dir containing gitfull-%{version}.tar.gz>"
#   --define "_topdir <scratch dir>"
#
# Design notes (docs/PACKAGING.md has the full rationale):
#   * binary -> %{_bindir} (/usr/bin): distro-owned tree. /usr/local/bin
#     stays gitfull's own territory for the apps IT installs.
#   * /etc/gitfull.conf.example only — the user's /etc/gitfull.conf is
#     never owned by this package, so upgrades can never touch it.
#     %config(noreplace) keeps a locally modified example across
#     upgrades (rpmsave/rpmnew semantics).
#   * nothing creates /var/lib/gitfull at package-install time: gitfull
#     creates it on the first state-changing run (cfg.ensure_root()).

Name:           gitfull
Version:        0.1.0
Release:        1%{?dist}
Summary:        Forge-agnostic package manager for Git-forge-hosted repositories
License:        Proprietary
URL:            https://github.com/FemBoyGamerTechGuy/GitFull
Source0:        gitfull-%{version}.tar.gz
# gitfull execs exactly two host tools at runtime (git for clones, curl
# for downloads and read-only forge search); everything else it builds
# itself as toolchains. The binary itself links only Rust std.
Requires:       git curl
BuildRequires:  cargo
# On RHEL without a packaged cargo: build with rustup's cargo on PATH and
# drop the BuildRequires line above (see docs/PACKAGING.md).

%description
gitfull treats any Git forge — GitHub, GitLab, Gitea, Codeberg, or
self-hosted — as an installable package source: repositories are cloned
into a per-app sandbox that gitfull manages, required toolchains are
provisioned from source, and exactly one artifact crosses the sandbox
boundary: the final binary, hashed and audit-logged.

The host compiler is used exactly once (to build the seed GCC). No OS
package manager is invoked by gitfull at runtime. Proprietary software —
all rights reserved.

%prep
%setup -q

%build
cargo build --release --locked

%install
install -Dm 0755 target/release/gitfull %{buildroot}%{_bindir}/gitfull
install -Dm 0644 config/gitfull.conf.example \
    %{buildroot}%{_sysconfdir}/gitfull.conf.example
# pre-compressed with -n (no timestamp) for reproducible output; brp
# scripts leave an already-gzipped man page alone
gzip -9n -c docs/gitfull.1 > %{buildroot}%{_mandir}/man1/gitfull.1.gz

%check
# offline test suite (96+ tests); rpmbuild --nocheck skips it
cargo test --release --locked --quiet

%files
%license LICENSE
%doc README.md
%doc docs/ARCHITECTURE.md docs/AUDIT.md docs/CONFIG.md docs/PACKAGING.md
%{_bindir}/gitfull
%config(noreplace) %{_sysconfdir}/gitfull.conf.example
%{_mandir}/man1/gitfull.1.gz

%changelog
* Mon Sep 21 2026 FemBoyGamerTechGuy <FemBoyGamerTechGuy@users.noreply.github.com> - 0.1.0-1
- Initial packaging of gitfull 0.1.0: forge-agnostic package manager
  with root privilege model, automatic toolchain provisioning, and
  ranked multi-forge search for bare package names.
