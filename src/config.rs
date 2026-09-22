//! `/etc/gitfull.conf` — gitfull's system-wide configuration.
//!
//! A custom TOML-based schema, parsed with the only two crates gitfull
//! depends on (`serde` + `toml`). The schema is deliberately *extensible*:
//!
//! * unknown keys inside `[forge.<name>]` entries are accepted, so new forge
//!   features can be configured before code support lands;
//! * arbitrary new `[forge.<name>]` entries define new forges with zero code
//!   changes (see [`crate::forge`]);
//! * `[repo."owner/name"]` entries override the forge, pin a ref, add
//!   package/toolchain requirements, or override build-system detection;
//! * unknown top-level sections produce a warning, not an error.
//!
//! Fixed sections (`core`, `paths`, `toolchain`, `policy`, `clone`, `repo`
//! entries) reject unknown keys so typos fail loudly.
//!
//! See docs/CONFIG.md for the full reference and the annotated example at
//! `config/gitfull.conf.example`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{GitfullError, Result};
use crate::forge::Registry;
use crate::util;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/gitfull.conf";
pub const DEFAULT_ROOT: &str = "/var/lib/gitfull";
pub const DEFAULT_BIN_DIR: &str = "/usr/local/bin";
/// POSIX utilities available to in-sandbox builds (`sh`, coreutils, ...).
/// These are *tools* invoked on sandbox paths only — see docs/AUDIT.md for
/// why this exists and how to reach full strictness (busybox toolchain).
pub const DEFAULT_HOST_TOOL_PATH: &str = "/usr/bin:/bin";

fn d_root() -> PathBuf {
    PathBuf::from(DEFAULT_ROOT)
}
fn d_bin() -> PathBuf {
    PathBuf::from(DEFAULT_BIN_DIR)
}
fn d_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}
fn d_host_tool_path() -> String {
    DEFAULT_HOST_TOOL_PATH.to_string()
}
fn d_color() -> String {
    "auto".to_string()
}
fn d_true() -> bool {
    true
}

/// `[core]` — global settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreSection {
    /// Sandbox/toolchain root. Everything gitfull writes lives under here
    /// (plus the single bin-dir copy-out path).
    #[serde(default = "d_root")]
    pub root: PathBuf,
    /// Where final binaries are copied (the single sandbox-escape path).
    #[serde(default = "d_bin")]
    pub bin_dir: PathBuf,
    /// Build parallelism.
    #[serde(default = "d_jobs")]
    pub jobs: usize,
    /// PATH entries for POSIX utilities usable by in-sandbox builds.
    #[serde(default = "d_host_tool_path")]
    pub host_tool_path: String,
    /// `auto` | `always` | `never`.
    #[serde(default = "d_color")]
    pub color: String,
}

// Keep derived-default and per-field serde defaults consistent: when the
// whole [core] section is absent, serde uses Default::default(), which
// must equal the field-level defaults (jobs=0 would be invalid).
impl Default for CoreSection {
    fn default() -> Self {
        CoreSection {
            root: d_root(),
            bin_dir: d_bin(),
            jobs: d_jobs(),
            host_tool_path: d_host_tool_path(),
            color: d_color(),
        }
    }
}

/// `[paths]` — optional per-directory overrides; default to `<root>/<name>`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PathsSection {
    #[serde(default)]
    pub apps: Option<PathBuf>,
    #[serde(default)]
    pub toolchains: Option<PathBuf>,
    #[serde(default)]
    pub cache: Option<PathBuf>,
    #[serde(default)]
    pub logs: Option<PathBuf>,
    #[serde(default)]
    pub libs: Option<PathBuf>,
}

/// `[forge]` — the forge registry.
///
/// `default = "github"` selects the default forge; every other key under
/// `[forge]` is a forge entry: `[forge.<name>]` (see [`crate::forge::ForgeDef`]).
/// Sub-tables are captured via `#[serde(flatten)]`, so *any* forge name is
/// valid — adding a forge is a config edit, not a code change.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ForgeSection {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(flatten)]
    pub entries: BTreeMap<String, crate::forge::ForgeDef>,
}

/// `[repo."owner/name"]` — per-repository overrides.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepoOverride {
    /// Route this repository through a different forge (data-defined).
    #[serde(default)]
    pub forge: Option<String>,
    /// Pin a branch/tag/commit.
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
    /// Override auto-detected build system: `autotools|make|meson|cmake|cargo`.
    #[serde(default)]
    pub build_system: Option<String>,
    /// Per-repo build parallelism.
    #[serde(default)]
    pub jobs: Option<usize>,
    /// Additional package dependencies (`"owner/repo"` specs).
    #[serde(default)]
    pub packages: Vec<String>,
    /// Additional toolchain requirements (`"gcc>=13"` style constraints).
    #[serde(default)]
    pub toolchains: Vec<String>,
    /// Explicit final binaries to install (overrides stage scanning).
    #[serde(default)]
    pub bins: Vec<String>,
}

/// `[dep.<name>]` — per-dependency resolution override, keyed by the
/// name as declared in a build manifest (meson `dependency('name')`,
/// cmake `find_package(Name)`, autotools `AC_CHECK_LIB([name])`, ...).
///
/// This is *user configuration* (mirroring `[toolchain.sources]`), not a
/// code-side name table: it exists so users can pin a dependency to a
/// specific source, mirror, or local path, deterministically. Without
/// an override, dependency names are resolved generically through the
/// ranked forge search.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DepOverride {
    /// Any spec form: `forge:owner/repo`, `owner/repo`, a URL, or a
    /// local path.
    #[serde(default)]
    pub source: Option<String>,
    /// Pin a branch/tag/commit (requires `source`).
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
    /// Never provision this dependency (a deliberate opt-out, e.g. when
    /// a project declares a dependency it does not actually need).
    #[serde(default)]
    pub skip: bool,
    /// **Component-scoped meson build** (multi-component monorepos —
    /// meson dependencies only): compile ONLY these ninja targets
    /// (upstream's own component alias targets, e.g. systemd's
    /// `libsystemd`) instead of the whole suite. Overrides the curated
    /// entry's scoping for this name; empty means "compile everything"
    /// (useful with `install_tags` alone).
    #[serde(default)]
    pub build_targets: Vec<String>,
    /// Install-tag filter for `meson install --tags …` (meson
    /// dependencies only): copy only files upstream tagged for the
    /// component, e.g. `"libsystemd,devel"`. Overridden together with
    /// `build_targets`; when neither is set the curated entry's scoping
    /// applies.
    #[serde(default)]
    pub install_tags: Option<String>,
}

/// `[toolchain]` — toolchain management.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ToolchainSection {
    /// Seed GCC version. **There is no hardcoded default**: when unset,
    /// gitfull resolves "latest" by querying the GCC git tags at bootstrap
    /// time ([`crate::toolchain`]). Set an explicit older version here to
    /// pin one.
    #[serde(default)]
    pub seed_gcc_version: Option<String>,
    /// Preferred versions per component (`gcc = "13.3.0"`).
    #[serde(default)]
    pub preferences: BTreeMap<String, String>,
    /// Source URL overrides per component (`gcc = "https://..."`).
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
}

/// `[policy]` — enforcement policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    /// Extra forbidden program names, in addition to gitfull's built-in
    /// package-manager/privilege-escalator denylist.
    #[serde(default)]
    pub extra_forbidden_programs: Vec<String>,
    /// Target apps may build copyleft code inside their own sandboxes
    /// (gitfull itself never links it). Default `true`.
    #[serde(default = "d_true")]
    pub allow_copyleft_targets: bool,
}

impl Default for PolicySection {
    fn default() -> Self {
        PolicySection {
            extra_forbidden_programs: Vec::new(),
            allow_copyleft_targets: true,
        }
    }
}

/// `[clone]` — clone behavior.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloneSection {
    /// Shallow-clone depth (unset = full history).
    #[serde(default)]
    pub depth: Option<usize>,
    /// Clone only the target branch. Default `true`.
    #[serde(default)]
    pub single_branch: Option<bool>,
    /// Initialize git submodules after clone. Default `false`.
    #[serde(default)]
    pub recurse_submodules: bool,
}

impl CloneSection {
    pub fn single_branch_on(&self) -> bool {
        self.single_branch.unwrap_or(true)
    }
}

/// The raw file as written by the user.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)]
    pub core: CoreSection,
    #[serde(default)]
    pub paths: PathsSection,
    #[serde(default)]
    pub forge: ForgeSection,
    #[serde(default)]
    pub repo: BTreeMap<String, RepoOverride>,
    #[serde(default)]
    pub dep: BTreeMap<String, DepOverride>,
    #[serde(default)]
    pub toolchain: ToolchainSection,
    #[serde(default)]
    pub policy: PolicySection,
    #[serde(default)]
    pub clone: CloneSection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn parse(s: &str) -> Result<ColorChoice> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            other => Err(GitfullError::Config(format!(
                "core.color: invalid value `{other}` (expected auto|always|never)"
            ))),
        }
    }
}

/// Fully-resolved runtime configuration (defaults applied, paths derived,
/// forges registered).
#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: Option<PathBuf>,
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub jobs: usize,
    pub host_tool_path: String,
    pub color: ColorChoice,
    pub apps_dir: PathBuf,
    pub toolchains_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub libs_dir: PathBuf,
    pub forges: Registry,
    pub repos: BTreeMap<String, RepoOverride>,
    pub deps: BTreeMap<String, DepOverride>,
    pub toolchain: ToolchainSection,
    pub policy: PolicySection,
    pub clone: CloneSection,
}

impl Config {
    /// Load from `path`; a missing file yields built-in defaults plus a
    /// warning (gitfull is usable before `/etc/gitfull.conf` exists).
    pub fn load(path: &Path) -> Result<(Config, Vec<String>)> {
        let mut warnings = Vec::new();
        let (text, existed) = match util::read_file_if_exists(path) {
            Ok(Some(s)) => (s, true),
            Ok(None) => (String::new(), false),
            Err(e) => return Err(e.into()),
        };
        if !existed {
            warnings.push(format!(
                "config file {} does not exist — using built-in defaults \
                 (create it from config/gitfull.conf.example)",
                path.display()
            ));
        }

        // Parse to a generic Value first so unknown top-level sections warn
        // instead of being silently swallowed.
        let value: toml::Value = toml::from_str(&text)?;
        let known = [
            "core",
            "paths",
            "forge",
            "repo",
            "dep",
            "toolchain",
            "policy",
            "clone",
        ];
        if let toml::Value::Table(t) = &value {
            for key in t.keys() {
                if !known.contains(&key.as_str()) {
                    warnings.push(format!(
                        "unknown top-level section [{key}] ignored \
                         (recognized: {known:?})"
                    ));
                }
            }
        }

        let fc: FileConfig = value.try_into().map_err(|e: toml::de::Error| {
            GitfullError::Config(format!("in {}: {e}", path.display()))
        })?;

        if fc.core.jobs == 0 {
            return Err(GitfullError::Config("core.jobs must be >= 1".to_string()));
        }
        let color = ColorChoice::parse(&fc.core.color)?;

        let forges = Registry::new(&fc.forge)?;

        let root = fc.core.root;
        let apps_dir = fc.paths.apps.unwrap_or_else(|| root.join("apps"));
        let toolchains_dir = fc
            .paths
            .toolchains
            .unwrap_or_else(|| root.join("toolchains"));
        let cache_dir = fc.paths.cache.unwrap_or_else(|| root.join("cache"));
        let logs_dir = fc.paths.logs.unwrap_or_else(|| root.join("logs"));
        let libs_dir = fc.paths.libs.unwrap_or_else(|| root.join("libs"));

        // Validate per-repo overrides reference known forges.
        for (key, ov) in &fc.repo {
            if let Some(f) = &ov.forge {
                forges.get(f).map_err(|_| {
                    GitfullError::Config(format!(
                        "repo.{key}: unknown forge `{f}` \
                         (configured forges: {})",
                        forges.names().join(", ")
                    ))
                })?;
            }
            if let Some(bs) = &ov.build_system {
                crate::manifest::BuildSystem::parse(bs)?;
            }
        }

        // Validate per-dependency overrides: `source` must parse as a
        // spec; `ref` requires `source`.
        for (key, ov) in &fc.dep {
            if let Some(s) = &ov.source {
                crate::spec::PkgSpec::parse(s).map_err(|e| {
                    GitfullError::Config(format!("dep.{key}: invalid source `{s}` ({e})"))
                })?;
            }
            if ov.git_ref.is_some() && ov.source.is_none() {
                return Err(GitfullError::Config(format!(
                    "dep.{key}: `ref` requires `source`"
                )));
            }
        }

        Ok((
            Config {
                config_path: if existed {
                    Some(path.to_path_buf())
                } else {
                    None
                },
                root,
                bin_dir: fc.core.bin_dir,
                jobs: fc.core.jobs,
                host_tool_path: fc.core.host_tool_path,
                color,
                apps_dir,
                toolchains_dir,
                cache_dir,
                logs_dir,
                libs_dir,
                forges,
                repos: fc.repo,
                deps: fc.dep,
                toolchain: fc.toolchain,
                policy: fc.policy,
                clone: fc.clone,
            },
            warnings,
        ))
    }

    /// Per-repo override for `owner/repo` (exact key match).
    pub fn repo_override_for(&self, owner: &str, repo: &str) -> Option<&RepoOverride> {
        self.repos.get(&format!("{owner}/{repo}"))
    }

    /// Per-dependency override for a manifest-declared dependency name.
    /// Exact key first, then case-insensitive (manifest names arrive in
    /// their native case — `find_package(ZLIB)` vs `[dep.zlib]`).
    pub fn dep_override_for(&self, name: &str) -> Option<&DepOverride> {
        if let Some(ov) = self.deps.get(name) {
            return Some(ov);
        }
        let lower = name.to_ascii_lowercase();
        self.deps
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v)
    }

    /// Ensure the mutable state root exists (called by mutating commands
    /// only — read-only commands never create anything).
    pub fn ensure_root(&self) -> Result<()> {
        for d in [
            &self.root,
            &self.apps_dir,
            &self.toolchains_dir,
            &self.cache_dir,
            &self.logs_dir,
            &self.libs_dir,
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}
