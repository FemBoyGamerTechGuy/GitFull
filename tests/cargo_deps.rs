//! gitfull's own dependency posture, enforced:
//!
//! * exactly `serde` + `toml` (both MIT OR Apache-2.0) — no copyleft, no
//!   Red Hat-associated system software linked into gitfull itself;
//! * proprietary project: never publishable, LICENSE file required.

use std::fs;

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
