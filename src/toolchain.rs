//! Toolchain manager — versioned, shared, self-built.
//!
//! `<root>/toolchains/<component>-<version>/` holds every toolchain
//! component (gcc, python, meson, ninja, cmake, vala, rust). Apps and
//! dependency builds *reference* these shared versions; nothing is
//! duplicated per-app.
//!
//! # The seed GCC bootstrap (the single host-touching path)
//!
//! The host system compiler is used **exactly once**, to build the seed GCC
//! toolchain; after that every build — dependencies and target apps alike —
//! uses the toolchain-managed compiler. The sequence (see
//! `seed_gcc_plan()` / `bootstrap_seed_gcc()`):
//!
//! 1. Resolve the version: `toolchain.seed_gcc_version` if pinned, else
//!    **latest**, auto-detected from the GCC git tags (`git ls-remote`,
//!    sealed FetchTool — no forge API, no hardcoded version constant).
//! 2. `git clone --branch releases/gcc-<v> --depth 1` the GCC source into
//!    the cache (sealed).
//! 3. `./contrib/download_prerequisites` (sealed FetchTool; GMP/MPFR/MPC
//!    into the source tree).
//! 4. `configure --disable-bootstrap ... && make && make install` into
//!    `toolchains/gcc-<v>/` — **this is the one and only invocation of the
//!    host compiler** (ExecClass::SeedHostCompiler; a single-stage build,
//!    `--disable-bootstrap`, so host cc compiles the seed exactly once).
//! 5. Record provenance in `toolchains/gcc-<v>/meta.toml`.
//!
//! After step 4, host cc is never invoked again: every later build runs
//! with a PATH that starts with `toolchains/*/bin`, and the exec guard
//! only ever classifies host-cc usage as `SeedHostCompiler` during this
//! sequence (see docs/AUDIT.md).
//!
//! Per the project brief, the seed-GCC build path is NOT exercised in the
//! development sandbox — it targets a full Linux machine. Everything up to
//! and including plan generation and version auto-detection is covered by
//! tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ToolchainSection;
use crate::error::{GitfullError, Result};
use crate::gitproc::{self, ExecCtx};
use crate::resolver::Constraint;
use crate::util::version_cmp;

/// One toolchain component in the catalog.
pub struct ComponentSpec {
    pub name: &'static str,
    /// Default source (git URL unless noted). `{version}` is substituted
    /// for tarball sources.
    pub source: &'static str,
    pub source_is_git: bool,
    /// Other components required to build this one.
    pub needs: &'static [&'static str],
}

/// The toolchain catalog. Sources are overridable via
/// `[toolchain.sources]` — no code changes needed to point at mirrors.
pub const CATALOG: &[ComponentSpec] = &[
    ComponentSpec {
        name: "gcc",
        source: "https://gcc.gnu.org/git/gcc.git",
        source_is_git: true,
        needs: &[],
        // SEED component: built once with the host compiler.
    },
    ComponentSpec {
        name: "python",
        source: "https://github.com/python/cpython.git",
        source_is_git: true,
        needs: &["gcc"],
    },
    ComponentSpec {
        name: "meson",
        source: "https://github.com/mesonbuild/meson.git",
        source_is_git: true,
        needs: &["python"],
    },
    ComponentSpec {
        name: "ninja",
        source: "https://github.com/ninja-build/ninja.git",
        source_is_git: true,
        needs: &["python", "gcc"],
    },
    ComponentSpec {
        name: "cmake",
        source: "https://gitlab.com/cmake/cmake.git",
        source_is_git: true,
        needs: &["gcc"],
    },
    ComponentSpec {
        name: "vala",
        source: "https://gitlab.gnome.org/GNOME/vala.git",
        source_is_git: true,
        needs: &["gcc"], // + glib at build time (see docs/ARCHITECTURE.md)
    },
    ComponentSpec {
        name: "rust",
        source: "https://static.rust-lang.org/dist/rust-{version}-x86_64-unknown-linux-gnu.tar.xz",
        source_is_git: false,
        needs: &[],
    },
];

pub fn catalog_entry(name: &str) -> Option<&'static ComponentSpec> {
    CATALOG.iter().find(|c| c.name == name)
}

/// Effective source URL for a component (config override applied).
pub fn source_url(component: &str, version: &str, sources: &BTreeMap<String, String>) -> String {
    if let Some(u) = sources.get(component) {
        return u.replace("{version}", version);
    }
    catalog_entry(component)
        .map(|c| c.source.replace("{version}", version))
        .unwrap_or_default()
}

/// A resolved toolchain component: version + directories.
#[derive(Debug, Clone)]
pub struct Selection {
    pub component: String,
    pub version: String,
    pub dir: PathBuf,
    pub bin: PathBuf,
}

pub struct ToolchainManager {
    pub dir: PathBuf,
    pub cfg: ToolchainSection,
}

impl ToolchainManager {
    pub fn new(toolchains_dir: PathBuf, cfg: ToolchainSection) -> ToolchainManager {
        ToolchainManager {
            dir: toolchains_dir,
            cfg,
        }
    }

    pub fn component_dir(&self, component: &str, version: &str) -> PathBuf {
        self.dir.join(format!("{component}-{version}"))
    }

    /// Versions installed for `component` (scanned from directory names).
    pub fn installed_versions(&self, component: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(v) = name.strip_prefix(&format!("{component}-")) {
                    // only dotted-numeric version dirs count as installed
                    let numeric = !v.is_empty()
                        && v.chars()
                            .next()
                            .map(|c| c.is_ascii_digit())
                            .unwrap_or(false)
                        && v.split('.')
                            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
                    if numeric && e.path().join("bin").is_dir() {
                        out.push(v.to_string());
                    }
                }
            }
        }
        out.sort_by(|a, b| version_cmp(a, b));
        out
    }

    /// Newest installed version satisfying `constraint`.
    pub fn find(&self, component: &str, constraint: &Constraint) -> Option<Selection> {
        let versions = self.installed_versions(component);
        let best = versions
            .into_iter()
            .filter(|v| constraint.satisfies(v))
            .max_by(|a, b| version_cmp(a, b))?;
        Some(Selection {
            dir: self.component_dir(component, &best),
            bin: self.component_dir(component, &best).join("bin"),
            component: component.to_string(),
            version: best,
        })
    }

    /// Resolve the seed GCC version: explicit pin (`seed_gcc_version`, or
    /// `preferences.gcc`), otherwise **latest** via git tag query. There is
    /// deliberately no hardcoded default.
    pub fn resolve_seed_gcc_version(
        &self,
        ctx: &ExecCtx,
        host_tool_path: &str,
        git_home: &Path,
    ) -> Result<String> {
        if let Some(v) = &self.cfg.seed_gcc_version {
            return Ok(v.clone());
        }
        if let Some(v) = self.cfg.preferences.get("gcc") {
            return Ok(v.clone());
        }
        let source = source_url("gcc", "latest", &self.cfg.sources);
        let refs = gitproc::git_ls_remote_tags(
            ctx,
            &source,
            "refs/tags/releases/gcc-*",
            host_tool_path,
            git_home,
        )?;
        let mut versions: Vec<String> = refs
            .into_iter()
            .filter_map(|r| {
                r.strip_prefix("refs/tags/releases/gcc-")
                    .map(|s| s.to_string())
            })
            // stable releases only: X.Y.Z (no suffixes like 13.1.0-rc1)
            .filter(|v| {
                !v.contains('-')
                    && v.split('.').count() == 3
                    && v.split('.')
                        .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
            })
            .collect();
        versions.sort_by(|a, b| version_cmp(a, b));
        versions.pop().ok_or_else(|| GitfullError::Toolchain {
            component: "gcc".into(),
            message: format!(
                "could not auto-detect the latest GCC release from {source} \
                     (no `refs/tags/releases/gcc-X.Y.Z` tags matched). Pin one \
                     explicitly with toolchain.seed_gcc_version in gitfull.conf"
            ),
        })
    }

    /// The audited seed-GCC bootstrap sequence, as human-readable steps.
    pub fn seed_gcc_plan(&self, version: &str) -> Vec<String> {
        let src_cache = self.source_cache_dir("gcc", version);
        let dest = self.component_dir("gcc", version);
        let src_cache_s = src_cache.display().to_string();
        let dest_s = dest.display().to_string();
        let source = source_url("gcc", version, &self.cfg.sources);
        let tag = format!("releases/gcc-{version}");
        vec![
            format!(
                "1. resolve version      : {version} (pin via toolchain.seed_gcc_version, \
                 or auto-detect latest from {source} refs/tags/releases/gcc-*)"
            ),
            format!(
                "2. fetch source (sealed): git clone --branch {tag} --depth 1 {source} -> {src_cache_s}"
            ),
            format!(
                "3. prerequisites        : ./contrib/download_prerequisites  (sealed FetchTool: \
                 GMP/MPFR/MPC land inside the source tree)"
            ),
            format!(
                "4. build [THE ONE HOST TOUCH]: mkdir -p {build} && cd {build} && \
                 {src_cache_s}/configure --disable-bootstrap --disable-nls --disable-multilib \
                 --enable-languages=c,c++ --prefix={dest_s} && make -j && make install",
                build = dest_s.to_string() + "-build",
            ),
            format!(
                "   NOTE: --disable-bootstrap = single-stage build; the HOST system \
                 compiler is invoked exactly ONCE, in this step, and never again"
            ),
            format!(
                "5. record provenance    : {meta} (component, version, source, commit, \
                 built_by=\"host-cc (seed)\", date)",
                meta = dest.join("meta.toml").display()
            ),
            format!(
                "6. from here on         : every build uses {bin} (ExecClass::Toolchain)",
                bin = dest.join("bin").display()
            ),
        ]
    }

    pub fn source_cache_dir(&self, component: &str, version: &str) -> PathBuf {
        // cache lives next to toolchains/ by default; manager only knows
        // its own dir, so use a sibling "cache" dir.
        let cache = self
            .dir
            .parent()
            .map(|p| p.join("cache"))
            .unwrap_or_else(|| self.dir.join("_cache"));
        cache.join(format!("{component}-{version}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_and_sources() {
        assert!(catalog_entry("gcc").is_some());
        assert!(catalog_entry("nope").is_none());
        let mut sources = BTreeMap::new();
        sources.insert("gcc".to_string(), "https://example.com/gcc.git".to_string());
        assert_eq!(
            source_url("gcc", "13", &sources),
            "https://example.com/gcc.git"
        );
        assert_eq!(
            source_url("rust", "1.80.0", &BTreeMap::new()),
            "https://static.rust-lang.org/dist/rust-1.80.0-x86_64-unknown-linux-gnu.tar.xz"
        );
    }

    #[test]
    fn installed_scan_and_find() {
        let tmp = std::env::temp_dir().join(format!("gitfull-tc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let mgr = ToolchainManager::new(tmp.join("toolchains"), ToolchainSection::default());
        // fake two gcc installs
        for v in ["13.3.0", "14.2.0"] {
            let d = mgr.component_dir("gcc", v).join("bin");
            fs::create_dir_all(&d).unwrap();
        }
        fs::create_dir_all(mgr.component_dir("gcc", "not-a-version").join("bin")).unwrap();
        // decoy: python dir should not appear under gcc
        fs::create_dir_all(mgr.dir.join("python-3.12.4/bin")).unwrap();

        let versions = mgr.installed_versions("gcc");
        assert_eq!(versions, vec!["13.3.0".to_string(), "14.2.0".to_string()]);

        let any = Constraint::parse_any();
        let sel = mgr.find("gcc", &any).unwrap();
        assert_eq!(sel.version, "14.2.0");

        let pinned = Constraint::parse_op(crate::resolver::CmpOp::Gte, "14.0".into());
        assert_eq!(mgr.find("gcc", &pinned).unwrap().version, "14.2.0");
        let old = Constraint::parse_op(crate::resolver::CmpOp::Lt, "14.0".into());
        assert_eq!(mgr.find("gcc", &old).unwrap().version, "13.3.0");
        // decoy python-3.12.4 was created above: it counts as an installed
        // python, but nothing was created for cmake.
        assert!(mgr.find("python", &any).is_some());
        assert!(mgr.find("cmake", &any).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }
}
