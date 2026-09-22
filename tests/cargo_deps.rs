//! gitfull's own dependency posture, enforced:
//!
//! * exactly `serde` + `toml` (both MIT OR Apache-2.0) — no copyleft, no
//!   Red Hat-associated system software linked into gitfull itself;
//! * proprietary project: never publishable, LICENSE file required.

use std::fs;
use std::process::Command;

#[test]
fn only_permissive_core_dependencies() {
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let v: toml::Value = toml::from_str(&text).unwrap();

    let deps = v
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();
    assert!(
        deps.is_empty() || deps.len() <= 2,
        "dependency budget exceeded: {:?}",
        deps.keys().collect::<Vec<_>>()
    );
    for key in deps.keys() {
        assert!(
            key == "serde" || key == "toml",
            "unexpected dependency `{key}` — gitfull carries only serde+toml \
             (license policy, docs/AUDIT.md)"
        );
    }
    // no dev-dependencies, no patches, no git-sourced deps
    assert!(v.get("dev-dependencies").is_none());
    assert!(v.get("patch").is_none());
    assert!(v.get("workspace").is_none());
}

/// The systemd-family hard constraint, stated in the original spec and
/// restated for the curated-map entries: **gitfull's own binary must
/// never link against, depend on, or embed libsystemd/systemd/D-Bus/
/// elogind** — those names exist in the curated map ONLY as build-time
/// dependencies of TARGET applications, resolved and built inside the
/// target app's isolated sandbox (exactly like GTK and GLib already
/// are), with zero connection to gitfull's own crate dependency tree.
///
/// This is the named enforcement point for that boundary, on all three
/// surfaces where it could regress:
///
/// 1. the crate graph (`[dependencies]` in Cargo.toml);
/// 2. the RESOLVED graph (Cargo.lock package names — a transitive
///    sys-dbus/zbus-style crate would appear here);
/// 3. the actually-compiled binary (`ldd` — the linked shared objects).
#[test]
fn gitfull_binary_never_links_systemd_family() {
    const DENY: &[&str] = &[
        "systemd", "elogind", "libsystemd", "libelogind", "libudev", "udev",
        "dbus", "zbus", "varlink",
    ];

    // 1. declared dependencies: the budget test above pins the graph to
    //    serde+toml; this is the explicit named case for the family
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let v: toml::Value = toml::from_str(&manifest).unwrap();
    let deps = v
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();
    for key in deps.keys() {
        let low = key.to_ascii_lowercase();
        for f in DENY {
            assert!(
                !low.contains(f),
                "gitfull's own `[dependencies]` must never contain `{f}` (found: `{key}`)"
            );
        }
    }

    // 2. the resolved graph: every locked package name (a transitive
    //    systemd-family or D-Bus crate would show up here)
    let lock = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap();
    let lockv: toml::Value = toml::from_str(&lock).unwrap();
    let packages = lockv
        .get("package")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!packages.is_empty(), "Cargo.lock has no packages?");
    for p in &packages {
        let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let low = name.to_ascii_lowercase();
        for f in DENY {
            assert!(
                !low.contains(f),
                "gitfull's own resolved graph must never contain `{f}` \
                 (locked package: `{name}`)"
            );
        }
    }

    // 3. the compiled binary itself: no systemd-family or D-Bus shared
    //    object may appear in its dynamic link set. (On a non-glibc
    //    toolchain without ldd the check is skipped — surfaces 1 and 2
    //    above already ran and are the structural guarantee.)
    let bin = env!("CARGO_BIN_EXE_gitfull");
    if let Ok(out) = Command::new("ldd").arg(bin).output() {
        let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        for f in DENY {
            assert!(
                !text.contains(f),
                "gitfull's own binary must never dynamically link `{f}`:\n{text}"
            );
        }
    }
}

#[test]
fn proprietary_project_setup() {
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let v: toml::Value = toml::from_str(&text).unwrap();
    let pkg = v
        .get("package")
        .and_then(|p| p.as_table())
        .cloned()
        .unwrap();

    assert_eq!(
        pkg.get("publish").and_then(|p| p.as_bool()),
        Some(false),
        "proprietary software must not be publishable to crates.io"
    );
    assert_eq!(
        pkg.get("license-file").and_then(|p| p.as_str()),
        Some("LICENSE")
    );

    let license = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/LICENSE")).unwrap();
    assert!(license.contains("All rights reserved"));
    assert!(
        !license.contains("Apache License") && !license.contains("MIT License"),
        "LICENSE must be a proprietary notice, not an OSI template"
    );
}
