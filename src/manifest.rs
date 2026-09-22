//! Build-system auto-detection.
//!
//! gitfull needs **no extra files** in a target repository: it detects the
//! build system from the files that are already there. An optional
//! `gitfull.toml` in a repo (or a `[repo."owner/name"]` entry in
//! gitfull.conf) can override detection, but nothing is ever *required*.
//!
//! Detection priority (most-specific first):
//!
//! | file             | build system | implicit toolchain needs |
//! |------------------|--------------|---------------------------|
//! | `meson.build`    | meson        | gcc, python, meson, ninja |
//! | `CMakeLists.txt` | cmake        | gcc, cmake, ninja         |
//! | `Cargo.toml`     | cargo        | rust                      |
//! | `configure`      | autotools    | gcc                       |
//! | `Makefile`       | make         | gcc                       |

use serde::Deserialize;
use std::path::Path;

use crate::error::{GitfullError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSystem {
    Autotools,
    Make,
    Meson,
    Cmake,
    Cargo,
}

impl BuildSystem {
    pub fn label(self) -> &'static str {
        match self {
            BuildSystem::Autotools => "autotools",
            BuildSystem::Make => "make",
            BuildSystem::Meson => "meson",
            BuildSystem::Cmake => "cmake",
            BuildSystem::Cargo => "cargo",
        }
    }

    pub fn parse(s: &str) -> Result<BuildSystem> {
        match s.trim().to_ascii_lowercase().as_str() {
            "autotools" | "autoconf" => Ok(BuildSystem::Autotools),
            "make" | "makefile" => Ok(BuildSystem::Make),
            "meson" => Ok(BuildSystem::Meson),
            "cmake" => Ok(BuildSystem::Cmake),
            "cargo" | "rust" => Ok(BuildSystem::Cargo),
            other => Err(GitfullError::Config(format!(
                "unknown build system `{other}` \
                 (expected autotools|make|meson|cmake|cargo)"
            ))),
        }
    }
}

/// The optional `gitfull.toml` repo manifest — never required, purely an
/// override for detection.
///
/// ```toml
/// [build]
/// system = "meson"     # override detection
/// bins  = ["myapp"]    # explicit final binaries (overrides stage scanning)
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManifestFile {
    #[serde(default)]
    pub build: ManifestBuild,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ManifestBuild {
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub bins: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoManifest {
    pub build: BuildSystem,
    /// Explicit binaries to install (empty = scan the staging area).
    pub bins: Vec<String>,
}

/// Detect the build system from a source checkout.
pub fn detect_build_system(src: &Path) -> Result<BuildSystem> {
    let has = |f: &str| src.join(f).is_file();
    if has("meson.build") {
        Ok(BuildSystem::Meson)
    } else if has("CMakeLists.txt") {
        Ok(BuildSystem::Cmake)
    } else if has("Cargo.toml") {
        Ok(BuildSystem::Cargo)
    } else if has("configure") {
        Ok(BuildSystem::Autotools)
    } else if has("configure.ac") || has("configure.in") {
        // a git checkout of an autotools project ships the input, not
        // the generated script (release tarballs carry `configure`);
        // the build chain bootstraps it with autoreconf
        Ok(BuildSystem::Autotools)
    } else if has("Makefile") {
        Ok(BuildSystem::Make)
    } else {
        Err(GitfullError::Unsupported(format!(
            "no recognized build system in {} (looked for meson.build, \
             CMakeLists.txt, Cargo.toml, configure, configure.ac, Makefile). \
             Auto-detection needs no extra files; you can force one with \
             [repo.\"owner/name\"] build_system = \"...\" in gitfull.conf",
            src.display()
        )))
    }
}

/// Load the effective manifest for a checkout: `gitfull.toml` overrides if
/// present, auto-detection otherwise.
pub fn load_manifest(src: &Path) -> Result<RepoManifest> {
    let manifest_path = src.join("gitfull.toml");
    let text = crate::util::read_file_if_exists(&manifest_path)?;
    match text {
        Some(t) => {
            let mf: ManifestFile = toml::from_str(&t)
                .map_err(|e| GitfullError::Toml(format!("in {}: {e}", manifest_path.display())))?;
            let build = match &mf.build.system {
                Some(s) => BuildSystem::parse(s)?,
                None => detect_build_system(src)?,
            };
            Ok(RepoManifest {
                build,
                bins: mf.build.bins,
            })
        }
        None => Ok(RepoManifest {
            build: detect_build_system(src)?,
            bins: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gitfull-mf-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detection() {
        let d = tmpdir("detect");
        assert!(detect_build_system(&d).is_err());
        // lowest priority: plain Makefile
        fs::write(d.join("Makefile"), "all:\n").unwrap();
        assert_eq!(detect_build_system(&d).unwrap(), BuildSystem::Make);
        // configure beats Makefile
        fs::write(d.join("configure"), "#!/bin/sh\n").unwrap();
        assert_eq!(detect_build_system(&d).unwrap(), BuildSystem::Autotools);
        // meson.build beats all of the above
        fs::write(d.join("meson.build"), "project('x')\n").unwrap();
        assert_eq!(detect_build_system(&d).unwrap(), BuildSystem::Meson);
        fs::remove_file(d.join("meson.build")).unwrap();
        // CMakeLists.txt beats configure/Makefile
        fs::write(d.join("CMakeLists.txt"), "").unwrap();
        assert_eq!(detect_build_system(&d).unwrap(), BuildSystem::Cmake);
        fs::remove_file(d.join("CMakeLists.txt")).unwrap();
        // Cargo.toml beats configure/Makefile
        fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(detect_build_system(&d).unwrap(), BuildSystem::Cargo);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn optional_manifest_overrides() {
        let d = tmpdir("manifest");
        fs::write(d.join("Makefile"), "all:\n").unwrap();
        fs::write(
            d.join("gitfull.toml"),
            "[build]\nsystem = \"cmake\"\nbins = [\"tool\"]\n",
        )
        .unwrap();
        let m = load_manifest(&d).unwrap();
        assert_eq!(m.build, BuildSystem::Cmake);
        assert_eq!(m.bins, vec!["tool".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_manifest_needed() {
        let d = tmpdir("nomanifest");
        fs::write(d.join("meson.build"), "").unwrap();
        let m = load_manifest(&d).unwrap();
        assert_eq!(m.build, BuildSystem::Meson);
        assert!(m.bins.is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
