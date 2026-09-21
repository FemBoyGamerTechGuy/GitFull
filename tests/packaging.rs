//! Distro-native packaging recipes stay in sync with the crate.
//!
//! gitfull ships build recipes for Arch (`packaging/PKGBUILD`),
//! Debian/Ubuntu (`packaging/debian/`), Fedora/RHEL
//! (`packaging/gitfull.spec`) and Void (`packaging/void/…/template`),
//! plus a man page (`docs/gitfull.1`) — see docs/PACKAGING.md.
//! These tests pin the packaging brief's invariants:
//!
//! * the version matches Cargo.toml in every recipe and the man page;
//! * `git` + `curl` are declared as the only runtime dependencies
//!   (that is what gitfull execs as host tools; everything else it
//!   builds itself as toolchains);
//! * no recipe code ever references `/var/lib/gitfull` — packages must
//!   NOT create runtime state at install time (gitfull creates it on
//!   the first state-changing run via `Config::ensure_root`);
//! * no maintainer scripts / RPM scriptlets exist (nothing executes at
//!   package install time);
//! * the shipped example conf carries each format's no-clobber
//!   semantics, and the real `/etc/gitfull.conf` is never owned by any
//!   package;
//! * the man page exists, is versioned, and is shipped by every recipe.

use std::fs;
use std::path::PathBuf;

/// Recipe files whose *code* (non-comment lines) is checked.
const RECIPES: &[&str] = &[
    "packaging/PKGBUILD",
    "packaging/gitfull.spec",
    "packaging/debian/control",
    "packaging/debian/rules",
    "packaging/void/srcpkgs/gitfull/template",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel))
        .unwrap_or_else(|e| panic!("packaging file missing: {rel}: {e}"))
}

fn exists(rel: &str) -> bool {
    root().join(rel).exists()
}

/// Non-comment lines only ('#' starts a comment in shell, make, RPM spec
/// and xbps-src template syntax).
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

fn crate_version() -> String {
    for line in read("Cargo.toml").lines() {
        if line.trim_start().starts_with("version = ") {
            return line
                .trim()
                .trim_start_matches("version = ")
                .trim_matches('"')
                .to_string();
        }
    }
    panic!("no `version = ` line found in Cargo.toml");
}

#[test]
fn recipe_versions_match_crate_version() {
    let v = crate_version();

    let pkgbuild = read("packaging/PKGBUILD");
    assert!(
        pkgbuild.lines().any(|l| l == format!("pkgver={v}")),
        "PKGBUILD pkgver must equal the crate version"
    );
    assert!(
        pkgbuild.lines().any(|l| l == "pkgrel=1"),
        "PKGBUILD pkgrel"
    );

    let spec = read("packaging/gitfull.spec");
    let ver_line = spec
        .lines()
        .find(|l| l.trim_start().starts_with("Version:"))
        .expect("spec has a Version: line");
    assert_eq!(
        ver_line.trim().trim_start_matches("Version:").trim(),
        v,
        "spec Version must equal the crate version"
    );
    let rel_line = spec
        .lines()
        .find(|l| l.trim_start().starts_with("Release:"))
        .expect("spec has a Release: line");
    assert!(
        rel_line.trim().trim_start_matches("Release:").trim().starts_with("1"),
        "spec Release must start at 1"
    );

    let changelog = read("packaging/debian/changelog");
    assert!(
        changelog.starts_with(&format!("gitfull ({v}) ")),
        "debian changelog must open with the crate version"
    );

    let template = read("packaging/void/srcpkgs/gitfull/template");
    assert!(
        template.lines().any(|l| l == format!("version={v}")),
        "xbps template version must equal the crate version"
    );
    assert!(
        template.lines().any(|l| l == "revision=1"),
        "xbps template revision"
    );

    let man = read("docs/gitfull.1");
    assert!(
        man.starts_with(".TH GITFULL 1 "),
        "man page must start with a .TH header"
    );
    assert!(
        man.contains(&format!("gitfull {v}")),
        "man page .TH must carry the crate version"
    );
}

#[test]
fn recipes_declare_git_and_curl_runtime_deps() {
    let pkgbuild = read("packaging/PKGBUILD");
    assert!(
        pkgbuild.contains("depends=('git' 'curl')"),
        "PKGBUILD must depend on git and curl"
    );

    let control = read("packaging/debian/control");
    let dep = control
        .lines()
        .find(|l| l.starts_with("Depends:"))
        .expect("debian control has a Depends: line");
    assert!(dep.contains("git"), "Depends must include git: {dep}");
    assert!(dep.contains("curl"), "Depends must include curl: {dep}");

    let spec = read("packaging/gitfull.spec");
    let req = spec
        .lines()
        .find(|l| l.trim_start().starts_with("Requires:"))
        .expect("spec has a Requires: line");
    assert!(req.contains("git"), "Requires must include git: {req}");
    assert!(req.contains("curl"), "Requires must include curl: {req}");

    let template = read("packaging/void/srcpkgs/gitfull/template");
    assert!(
        template.contains("depends=\"git curl\""),
        "xbps template must depend on git and curl"
    );
}

#[test]
fn recipes_never_create_or_own_runtime_state() {
    for rel in RECIPES {
        for line in code_lines(&read(rel)) {
            assert!(
                !line.contains("/var/lib"),
                "{rel}: recipe code must not reference /var/lib — packages \
                 must not create runtime state at install time (gitfull \
                 creates it on first use): {line:?}"
            );
        }
    }
    // Debian: no maintainer scripts — nothing runs at package install
    for script in ["postinst", "preinst", "prerm", "postrm", "triggers"] {
        assert!(
            !exists(&format!("packaging/debian/{script}")),
            "debian/{script} must not exist (no maintainer scripts by design)"
        );
    }
    // RPM: no scriptlets either (%prep/%build/%install/%check are fine —
    // they run at *build* time in the packaging sandbox; scriptlets run
    // on the *user's* machine at package install time)
    let spec = read("packaging/gitfull.spec");
    for line in code_lines(&spec) {
        let t = line.trim_start();
        for section in ["%pre", "%post", "%preun", "%postun", "%trigger"] {
            let is_section = t == section || t.starts_with(&format!("{section} "));
            assert!(
                !is_section,
                "spec scriptlet {section} must not exist (nothing runs at \
                 package install time)"
            );
        }
    }
}

#[test]
fn example_conf_is_protected_and_real_conf_never_owned() {
    // each format's no-clobber mechanism for the shipped example
    assert!(
        read("packaging/PKGBUILD")
            .contains("backup=('etc/gitfull.conf.example')"),
        "PKGBUILD must list the example conf in backup=()"
    );
    assert_eq!(
        read("packaging/debian/conffiles").trim(),
        "/etc/gitfull.conf.example",
        "debian conffiles must contain exactly the example conf"
    );
    assert!(
        read("packaging/gitfull.spec")
            .contains("%config(noreplace) %{_sysconfdir}/gitfull.conf.example"),
        "spec must mark the example conf %config(noreplace)"
    );
    assert!(
        read("packaging/void/srcpkgs/gitfull/template")
            .contains("conf_files=\"/etc/gitfull.conf.example\""),
        "xbps template must list the example conf in conf_files"
    );
    // and no recipe line may install the REAL conf — only the .example
    for rel in RECIPES {
        for line in code_lines(&read(rel)) {
            if line.contains("gitfull.conf") {
                assert!(
                    line.contains("gitfull.conf.example"),
                    "{rel}: only the .example may be shipped, never the real \
                     /etc/gitfull.conf: {line:?}"
                );
            }
        }
    }
}

#[test]
fn man_page_is_present_and_shipped_by_every_recipe() {
    let man = read("docs/gitfull.1");
    assert!(man.contains(".SH NAME"), "man page has a NAME section");
    assert!(man.contains(".SH DESCRIPTION"), "man page has DESCRIPTION");
    assert!(
        man.contains("sudo gitfull install"),
        "man page examples use sudo for state-changing commands"
    );
    for rel in [
        "packaging/PKGBUILD",
        "packaging/gitfull.spec",
        "packaging/debian/rules",
        "packaging/void/srcpkgs/gitfull/template",
    ] {
        assert!(
            read(rel).contains("gitfull.1"),
            "{rel}: every recipe must ship the man page"
        );
    }
}

#[test]
fn build_scripts_present_and_sane() {
    for script in [
        "packaging/make-archive.sh",
        "packaging/build-arch.sh",
        "packaging/build-deb.sh",
        "packaging/build-rpm.sh",
    ] {
        let text = read(script);
        assert!(
            text.starts_with("#!/usr/bin/env bash"),
            "{script}: bash shebang"
        );
        assert!(
            text.contains("set -euo pipefail"),
            "{script}: strict mode"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root().join(script))
                .unwrap_or_else(|e| panic!("{script}: {e}"))
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "{script}: must be executable");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root().join("packaging/debian/rules"))
            .expect("debian/rules must exist")
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "debian/rules must be executable");
    }
    assert_eq!(
        read("packaging/debian/source/format").trim(),
        "3.0 (native)",
        "debian source format"
    );
    // the binary goes to /usr/bin (distro-owned tree); /usr/local/bin
    // stays gitfull's own territory for the apps IT installs
    assert!(
        read("packaging/PKGBUILD").contains("\"${pkgdir}/usr/bin/gitfull\""),
        "PKGBUILD installs to /usr/bin"
    );
    assert!(
        read("packaging/debian/rules").contains("usr/bin/gitfull"),
        "debian rules install to /usr/bin"
    );
    assert!(
        read("packaging/gitfull.spec").contains("%{_bindir}/gitfull"),
        "spec installs to %{{_bindir}}"
    );
}
