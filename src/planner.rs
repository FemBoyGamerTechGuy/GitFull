//! Install pipeline orchestration.
//!
//! Flow of `gitfull install <spec>`:
//!
//! 1. resolve the spec against the forge registry (`[repo]` overrides
//!    can reroute the forge, pin a ref, or add requirements) — a bare
//!    `name` is first resolved through ranked forge search (see
//!    [`crate::search`]);
//! 2. create the app sandbox; clone with the **live progress UI**;
//! 3. auto-detect the build system (no extra files needed in the repo);
//! 4. **parse the build system's own dependency declarations**
//!    ([`crate::depgraph`]: meson `dependency()`/wraps, cmake
//!    `find_package()`/`find_library()`, Cargo.toml, configure.ac,
//!    Makefile pkg-config calls) and walk the full dependency graph
//!    cycle-safely, cloning and building every dependency inside *this*
//!    sandbox under `deps/` — each fetch shows the same live progress
//!    UI. Resolved dependencies are cached in the shared library cache
//!    `<root>/libs/` ([`crate::libcache`]) so a second app needing the
//!    same library reuses the build;
//! 5. select **shared** toolchains from `<root>/toolchains/` — anything
//!    missing is **auto-provisioned from source** by gitfull itself
//!    ([`crate::bootstrap::ensure_components`]): the seed GCC uses the
//!    host compiler exactly once, everything after that is built with
//!    toolchain-managed tools. A host package manager is never invoked
//!    (structurally impossible — see [`crate::gitproc`]);
//! 6. build dependencies leaves-first, then the app (all commands through
//!    the exec chokepoint, hermetic env);
//! 7. stage + collect final binaries;
//! 8. **the single sandbox-escape path** — [`install_binaries`] copies
//!    exactly those binaries into the bin dir, hashed + audited;
//! 9. write `meta.toml` (provenance for `list` / `update` / `remove`).
//!
//! Mutating operations enforce the root-privilege model
//! ([`crate::privilege`]): they write `<root>` and the system bin dir, so
//! they must run under `sudo` on a normal deployment.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::bootstrap;
use crate::config::Config;
use crate::depgraph::{self, DeclaredDep, DeclaredDeps, WrapDep};
use crate::error::{GitfullError, Result};
use crate::gitproc::{self, authed_url, ExecClass, ExecCtx, RemoteUrl};
use crate::libcache::{self, LibCache, LibMeta};
use crate::progress::ProgressUi;
use crate::resolver::{resolve_repo, ResolvedRepo};
use crate::sandbox::Sandbox;
use crate::search;
use crate::sha256::sha256_file;
use crate::spec::{PkgSpec, Source};
use crate::toolchain::{Selection, ToolchainManager};
use crate::util;

#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub dry_run: bool,
    pub yes: bool,
    pub verbose: bool,
}

// ---------------------------------------------------------------------------
// meta.toml — the per-app install record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaToolchain {
    pub component: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaBin {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub dest: String,
}

/// One shared-library-cache entry an install used (discovered via
/// [`crate::depgraph`] and built from source, or reused from a previous
/// install). Entries are shared between apps like toolchain components;
/// `gitfull remove` never deletes them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaLib {
    /// Cache entry key (`<slug>-<hash>`).
    pub key: String,
    /// Name the dependency was declared as in the manifest.
    pub name: String,
    /// Source the library was fetched from.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub name: String,
    pub forge: String,
    pub owner: String,
    pub repo: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub build_system: String,
    pub toolchains: Vec<MetaToolchain>,
    pub packages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libs: Vec<MetaLib>,
    pub bins: Vec<MetaBin>,
    pub installed_at: u64,
}

pub fn read_meta(path: &Path) -> Result<InstallRecord> {
    let text = fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

fn write_meta(path: &Path, rec: &InstallRecord) -> Result<()> {
    fs::write(path, toml::to_string(rec)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// source resolution
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ResolvedSource {
    Git {
        forge_name: String,
        owner: String,
        repo: String,
        url: RemoteUrl,
        git_ref: Option<String>,
    },
    Local(PathBuf),
}

fn forge_token(forge: &crate::forge::Forge) -> Option<String> {
    forge
        .token_env()
        .and_then(|v| std::env::var(v).ok())
        .filter(|t| !t.is_empty())
}

fn resolve_source(cfg: &Config, spec: &PkgSpec) -> Result<ResolvedSource> {
    match &spec.source {
        Source::Forge { forge: Some(f), .. } => {
            let forge = cfg.forges.get(f)?;
            let owner = spec_owner(spec);
            let repo = spec_repo(spec);
            let ov = cfg.repo_override_for(&owner, &repo);
            let url = forge.clone_url(&owner, &repo)?;
            let token = forge_token(forge);
            Ok(ResolvedSource::Git {
                forge_name: forge.name.clone(),
                owner,
                repo,
                url: authed_url(&url, token.as_deref()),
                git_ref: spec
                    .git_ref
                    .clone()
                    .or_else(|| ov.and_then(|o| o.git_ref.clone())),
            })
        }
        Source::Forge { forge: None, .. } => {
            let owner = spec_owner(spec);
            let repo = spec_repo(spec);
            let ov = cfg.repo_override_for(&owner, &repo);
            let forge = match ov.and_then(|o| o.forge.as_deref()) {
                Some(f) => cfg.forges.get(f)?,
                None => cfg.forges.default(),
            };
            let url = forge.clone_url(&owner, &repo)?;
            let token = forge_token(forge);
            Ok(ResolvedSource::Git {
                forge_name: forge.name.clone(),
                owner,
                repo,
                url: authed_url(&url, token.as_deref()),
                git_ref: spec
                    .git_ref
                    .clone()
                    .or_else(|| ov.and_then(|o| o.git_ref.clone())),
            })
        }
        Source::Url(u) => match cfg.forges.match_url(u) {
            Some(m) => {
                let forge = cfg.forges.get(&m.forge)?;
                let token = forge_token(forge);
                Ok(ResolvedSource::Git {
                    forge_name: forge.name.clone(),
                    owner: m.owner.clone(),
                    repo: m.repo.clone(),
                    url: authed_url(u, token.as_deref()),
                    git_ref: spec.git_ref.clone(),
                })
            }
            None => {
                // A git URL on a host no [forge] entry covers. Cloning an
                // anonymous https remote needs no forge machinery (search,
                // ranking and tokens are all forge-side), so proceed as a
                // generic source — with a visible hint that registering the
                // host enables token/search handling for it. This is what
                // lets curated-map entries (gitlab.gnome.org, …) and raw-URL
                // [dep] pins work without extra configuration.
                println!(
                    "gitfull: note: URL `{u}` matches no configured forge host — \
                     cloning it as an anonymous generic git remote (add a \
                     [forge.<name>] entry with that host if you need tokens \
                     or search for it)"
                );
                let (owner, repo) = parse_git_url_owner_repo(u)
                    .unwrap_or_else(|| (String::new(), "repo".to_string()));
                Ok(ResolvedSource::Git {
                    forge_name: "generic".to_string(),
                    owner,
                    repo,
                    url: authed_url(u, None),
                    git_ref: spec.git_ref.clone(),
                })
            }
        },
        Source::Search { .. } => Err(GitfullError::Unsupported(
            "internal: a search spec reached the planner unprocessed — \
             main.rs / info must resolve it via search::resolve first"
                .into(),
        )),
        Source::Local(p) => {
            let canon = fs::canonicalize(p)?;
            Ok(ResolvedSource::Local(canon))
        }
    }
}

fn spec_owner(spec: &PkgSpec) -> String {
    match &spec.source {
        Source::Forge { owner, .. } => owner.clone(),
        _ => String::new(),
    }
}

/// `(owner, repo)` from a git URL's path — `…/owner/repo(.git)` (owner
/// may be a nested group: `…/group/sub/repo.git`). Generic (non-forge)
/// URL sources use this for sandbox naming only.
fn parse_git_url_owner_repo(url: &str) -> Option<(String, String)> {
    let path = url.split("://").nth(1)?.splitn(2, '/').nth(1)?;
    let mut segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    // drop a trailing empty fragment such as "…" or query strings
    let last = segs.last_mut()?;
    if let Some(stripped) = last.strip_suffix(".git") {
        *last = stripped;
    }
    if segs.last()?.is_empty() {
        segs.pop();
    }
    let repo = segs.pop()?.to_string();
    if repo.is_empty() {
        return None;
    }
    Some((segs.join("/"), repo))
}

fn spec_repo(spec: &PkgSpec) -> String {
    match &spec.source {
        Source::Forge { repo, .. } => repo.clone(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// build adapters
// ---------------------------------------------------------------------------

struct BuildStep {
    label: String,
    argv: Vec<String>,
    cwd: PathBuf,
}

fn build_commands(bs: crate::manifest::BuildSystem, sb: &Sandbox, jobs: usize) -> Vec<BuildStep> {
    use crate::manifest::BuildSystem::*;
    let s = |label: &str, argv: Vec<String>, cwd: PathBuf| BuildStep {
        label: label.to_string(),
        argv,
        cwd,
    };
    let src = sb.src().display().to_string();
    let build = sb.build().display().to_string();
    let prefix = sb.prefix().display().to_string();
    match bs {
        Autotools => vec![
            s(
                "configure",
                vec![format!("{src}/configure"), format!("--prefix={prefix}")],
                sb.build(),
            ),
            s(
                "build",
                vec!["make".into(), format!("-j{jobs}")],
                sb.build(),
            ),
            s("install", vec!["make".into(), "install".into()], sb.build()),
        ],
        Make => vec![
            s("build", vec!["make".into(), format!("-j{jobs}")], sb.src()),
            s(
                "install",
                vec![
                    "make".into(),
                    "install".into(),
                    format!("PREFIX={prefix}"),
                    format!("DESTDIR={}", sb.stage().display()),
                ],
                sb.src(),
            ),
        ],
        Meson => vec![
            s(
                "setup",
                vec![
                    "meson".into(),
                    "setup".into(),
                    build.clone(),
                    "--prefix".into(),
                    prefix,
                ],
                sb.src(),
            ),
            s(
                "build",
                vec!["ninja".into(), "-C".into(), build.clone()],
                sb.build(),
            ),
            s(
                "install",
                vec!["ninja".into(), "-C".into(), build, "install".into()],
                sb.build(),
            ),
        ],
        Cmake => vec![
            s(
                "configure",
                vec![
                    "cmake".into(),
                    "-S".into(),
                    src,
                    "-B".into(),
                    build.clone(),
                    format!("-DCMAKE_INSTALL_PREFIX={prefix}"),
                    "-GNinja".into(),
                ],
                sb.build(),
            ),
            s(
                "build",
                vec![
                    "cmake".into(),
                    "--build".into(),
                    build.clone(),
                    "--parallel".into(),
                    jobs.to_string(),
                ],
                sb.build(),
            ),
            s(
                "install",
                vec!["cmake".into(), "--install".into(), build],
                sb.build(),
            ),
        ],
        Cargo => vec![s(
            "build",
            vec!["cargo".into(), "build".into(), "--release".into()],
            sb.src(),
        )],
    }
}

fn run_build(
    ctx: &ExecCtx,
    sb: &Sandbox,
    steps: &[BuildStep],
    env: &[(String, String)],
    verbose: bool,
) -> Result<()> {
    for (i, step) in steps.iter().enumerate() {
        let log = sb.logs().join(format!("{:02}-{}.log", i + 1, step.label));
        if verbose {
            println!("  $ {}", step.argv.join(" "));
        }
        gitproc::run(
            ctx,
            &step.argv,
            ExecClass::Toolchain,
            &step.cwd,
            env,
            Some(&log),
        )?;
    }
    Ok(())
}

/// The effective staging root for a sandbox.
///
/// `make install DESTDIR=<stage> PREFIX=<absolute prefix>` stages content
/// at `<stage><prefix>` (DESTDIR concatenation with absolute paths), so the
/// real staging root is the string concatenation; relative-PREFIX layouts
/// and direct installs land in `stage/` itself.
fn stage_root(sb: &Sandbox) -> PathBuf {
    let stage = sb.stage().display().to_string();
    let prefix = sb.prefix().display().to_string();
    let concat = PathBuf::from(format!("{stage}{prefix}"));
    if concat.is_dir() {
        return concat;
    }
    sb.stage()
}

/// Recursively move staged install trees into a prefix (used for deps so
/// later builds can find their artifacts via PKG_CONFIG_PATH/-I/-L).
fn promote_stage(stage: &Path, prefix: &Path) -> Result<()> {
    if !stage.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(stage)? {
        let e = entry?;
        let name = e.file_name();
        let dest = prefix.join(&name);
        if e.path().is_dir() {
            if dest.is_dir() {
                promote_stage(&e.path(), &dest)?;
            } else {
                fs::create_dir_all(prefix)?;
                fs::rename(e.path(), &dest)?;
            }
        } else if dest.exists() {
            fs::remove_file(&dest)?;
            fs::rename(e.path(), &dest)?;
        } else {
            fs::create_dir_all(prefix)?;
            fs::rename(e.path(), &dest)?;
        }
    }
    Ok(())
}

fn is_executable_file(p: &Path) -> bool {
    match fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

fn is_binary_artifact(name: &str) -> bool {
    !(name.starts_with('.')
        || name.ends_with(".so")
        || name.contains(".so.")
        || name.ends_with(".a")
        || name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".o")
        || name.ends_with(".la")
        || name.ends_with(".pc")
        || name.ends_with(".h"))
}

fn scan_dir_for_bins(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if recursive && name != "pkgconfig" && !name.starts_with('.') {
                    scan_dir_for_bins(&p, recursive, out);
                }
            } else if is_binary_artifact(&name) && is_executable_file(&p) {
                out.push(p);
            }
        }
    }
}

/// Collect the final binaries a build produced.
fn collect_bins(
    sb: &Sandbox,
    bs: crate::manifest::BuildSystem,
    explicit: &[String],
) -> Result<Vec<PathBuf>> {
    let staging = stage_root(sb);
    if !explicit.is_empty() {
        let mut found = Vec::new();
        for name in explicit {
            let mut all = Vec::new();
            scan_dir_for_bins(&staging, true, &mut all);
            scan_dir_for_bins(&sb.prefix().join("bin"), false, &mut all);
            scan_dir_for_bins(&sb.build().join("target/release"), false, &mut all);
            let hit = all
                .into_iter()
                .find(|p| p.file_name().map(|n| n == name.as_str()).unwrap_or(false));
            match hit {
                Some(p) => found.push(p),
                None => {
                    return Err(GitfullError::Exec {
                        program: "collect-bins".into(),
                        status: "declared binary not found".into(),
                        log: None,
                        tail: format!(
                            "binary `{name}` (declared via [repo] bins or \
                             gitfull.toml) was not produced by the build; checked \
                             staging, prefix/bin, build/target/release"
                        ),
                    })
                }
            }
        }
        return Ok(found);
    }
    // auto-scan: DESTDIR staging first (install rules), then prefix/bin,
    // then cargo's target/release
    let mut bins = Vec::new();
    scan_dir_for_bins(&staging, true, &mut bins);
    scan_dir_for_bins(&sb.prefix().join("bin"), false, &mut bins);
    if bs == crate::manifest::BuildSystem::Cargo {
        scan_dir_for_bins(&sb.build().join("target/release"), false, &mut bins);
    }
    // dedup by file name (first wins)
    let mut seen = BTreeSet::new();
    bins.retain(|p| {
        let n = p
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        !n.is_empty() && seen.insert(n)
    });
    Ok(bins)
}

// ---------------------------------------------------------------------------
// THE SINGLE SANDBOX-ESCAPE PATH
// ---------------------------------------------------------------------------

/// Copy final binaries from the sandbox into the bin directory.
///
/// This is the **only** sanctioned crossing point between a sandbox and the
/// host filesystem: each produced binary is copied (mode 0755) to the
/// configured bin dir, hashed (SHA-256), recorded in the audit log and in
/// the app's `meta.toml` so `gitfull remove` can later verify and undo it.
///
/// Nothing else in gitfull writes outside `<root>`.
pub fn install_binaries(
    cfg: &Config,
    ctx: &ExecCtx,
    bins: &[PathBuf],
    yes: bool,
) -> Result<Vec<MetaBin>> {
    fs::create_dir_all(&cfg.bin_dir)?;
    let mut out = Vec::new();
    for src in bins {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "binary".into());
        let dest = cfg.bin_dir.join(&name);
        if dest.exists() {
            if !yes {
                return Err(GitfullError::Sandbox(format!(
                    "refusing to overwrite existing {} (pass --yes to replace)",
                    dest.display()
                )));
            }
            println!("gitfull: warning: overwriting existing {}", dest.display());
        }
        let hash = sha256_file(src)?;
        let size = fs::metadata(src)?.len();
        fs::copy(src, &dest)?;
        let mut perms = fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms)?;
        gitproc::audit_event(
            ctx,
            "install-binary",
            &format!("{}\t{hash}\t{}", name, dest.display()),
        );
        out.push(MetaBin {
            name,
            sha256: hash,
            size,
            dest: dest.display().to_string(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// dependency-graph discovery + provisioning (depgraph.rs + libcache.rs)
// ---------------------------------------------------------------------------

/// The live progress UI used for **every** clone — the main repo,
/// each dependency source, and toolchain components. One mechanism,
/// one look (fill / rate / ETA / MB), so no fetch is ever invisible.
pub(crate) fn clone_progress(cfg: &Config) -> ProgressUi {
    ProgressUi::new(
        util::is_tty(2),
        util::term_width(),
        match cfg.color {
            crate::config::ColorChoice::Always => true,
            crate::config::ColorChoice::Never => false,
            crate::config::ColorChoice::Auto => util::colors_enabled(false, false),
        },
    )
}

/// How one dependency's source is fetched.
#[derive(Debug, Clone)]
enum Fetch {
    Git {
        url: RemoteUrl,
        git_ref: Option<String>,
    },
    Local {
        path: PathBuf,
    },
    /// meson `[wrap-file]`: source tarball (+ optional patch).
    Tarball {
        url: String,
        patch_url: Option<String>,
    },
}

impl Fetch {
    fn label(&self) -> String {
        match self {
            Fetch::Git { url, .. } => url.clean.clone(),
            Fetch::Local { path } => path.display().to_string(),
            Fetch::Tarball { url, .. } => url.clone(),
        }
    }
}

/// What the walk should do with one queued dependency source.
#[derive(Debug)]
enum DepSpec {
    /// A concrete package spec (`[repo] packages`, `[dep.<name>] source`,
    /// or a name resolved through ranked forge search).
    Package(PkgSpec),
    /// meson `[wrap-git]` subproject (clone the pinned URL directly).
    WrapGit(WrapDep),
    /// meson `[wrap-file]` subproject (source tarball).
    WrapFile(WrapDep),
}

/// One node of the per-install dependency graph. Node 0 is the app
/// itself; every other node is a dependency discovered from build
/// manifests (or declared via `[repo] packages`).
struct DepNode {
    key: String,
    /// Dedupe identity: clone URL + ref, local path, or tarball URL.
    identity: String,
    /// Deterministic `<root>/libs/` cache key for this source.
    cache_key: String,
    /// Name the dependency was declared as in a manifest, if any.
    declared: Option<String>,
    /// Normalized declared name (cache-provides matching).
    name_norm: Option<String>,
    sandbox: Sandbox,
    resolved: ResolvedRepo,
    parent: usize,
    /// Direct dependency nodes (children = things this node needs).
    children: Vec<usize>,
    /// Shared-cache entries reused by *name* while planning this node.
    lib_hits: Vec<PathBuf>,
    fetch: Fetch,
    /// Final install location: the lib-cache entry dir (deps) — set
    /// after the node is built or found cached.
    prefix: Option<PathBuf>,
    commit: Option<String>,
}

/// What planning one declared dependency concluded.
#[derive(Debug)]
enum DepPlan {
    /// Nothing to fetch (reason printed for the user).
    Satisfied,
    /// Reuse an existing shared-cache entry by provided name.
    CacheHit(PathBuf),
    /// Fetch + build from this source.
    Fetch(DepSpec),
    /// Only ranked-search candidates exist — **unconfirmed**. Never
    /// auto-built: the walk must either collect an interactive
    /// confirmation (TTY) or fail with a pin hint; `--dry-run` reports
    /// it as unresolved and skips the subtree.
    Unconfirmed(Vec<search::Candidate>),
}

/// Plan one manifest-declared dependency through the resolution layers
/// (highest authority first):
///
/// 1. cargo registry deps — the cargo resolver fetches them into the
///    app's own sandbox at build time (isolation-compliant by design);
/// 2. user `[dep.<name>]` override (`skip`, or a pinned `source`) —
///    user configuration always wins, including over the curated map;
/// 3. meson wraps (the manifest's own pin files — git/file);
/// 4. vendored subprojects (checked-in trees under `subprojects/`);
/// 5. the shared library cache, matched by provided names;
/// 6. the **curated upstream map** ([`crate::libmap`]) — well-known
///    pkg-config module names → their correct upstream repository.
///    This is the sound resolution strategy for module names: a
///    module→upstream mapping is *knowledge*, not something
///    star-ranking can infer (module names often live inside a parent
///    library's repo — `gio-unix-2.0` is a GLib module; generic names
///    string-match unrelated projects);
/// 7. ranked forge search — **flagged fallback only**: candidates are
///    returned unconfirmed ([`DepPlan::Unconfirmed`]) and must never be
///    silently auto-built.
fn plan_declared_dep(
    cfg: &Config,
    ctx: &ExecCtx,
    cache: &LibCache,
    declared: &DeclaredDeps,
    d: &DeclaredDep,
) -> Result<DepPlan> {
    if d.kind == crate::depgraph::DepKind::CrateRegistry {
        println!(
            "gitfull: dep `{}`: cargo registry dependency — cargo itself \
             fetches it into the app's sandbox at build time",
            d.name
        );
        return Ok(DepPlan::Satisfied);
    }
    if let Some(ov) = cfg.dep_override_for(&d.name) {
        if ov.skip {
            println!("gitfull: dep `{}`: skipped ([dep.{}] skip = true)", d.name, d.name);
            return Ok(DepPlan::Satisfied);
        }
        if let Some(src) = &ov.source {
            let mut spec = PkgSpec::parse(src)?;
            if spec.git_ref.is_none() {
                spec.git_ref = ov.git_ref.clone();
            }
            println!(
                "gitfull: dep `{}`: pinned via [dep.{}] -> {}",
                d.name,
                d.name,
                spec.key()
            );
            // identity-level cache check BEFORE fetching: this exact
            // pinned source may already have been built by an earlier
            // install — then nothing is cloned at all
            return identity_or_fetch(cfg, cache, &d.name_norm, DepSpec::Package(spec));
        }
    }
    // meson wraps: the manifest's own pin for this name wins over search
    if let Some(w) = declared
        .wraps
        .iter()
        .find(|w| w.name.to_ascii_lowercase() == d.name_norm || w.provides.iter().any(|p| p.to_ascii_lowercase() == d.name_norm))
    {
        if let Some(git) = &w.git {
            println!(
                "gitfull: dep `{}`: meson wrap (git) -> {} ({})",
                d.name, git.0, git.1
            );
            return identity_or_fetch(cfg, cache, &d.name_norm, DepSpec::WrapGit(w.clone()));
        }
        if let Some(file) = &w.file {
            println!(
                "gitfull: dep `{}`: meson wrap (file) -> {}",
                d.name, file.0
            );
            return identity_or_fetch(cfg, cache, &d.name_norm, DepSpec::WrapFile(w.clone()));
        }
    }
    // vendored subproject trees are used in-tree; fetching anything
    // for them would be wrong
    if declared
        .vendored
        .iter()
        .any(|v| v.to_ascii_lowercase() == d.name_norm)
    {
        println!(
            "gitfull: dep `{}`: vendored subproject in-tree — nothing to fetch",
            d.name
        );
        return Ok(DepPlan::Satisfied);
    }
    // shared library cache: reuse a build a previous install produced
    if let Some((key, dir)) = cache.find_providing(&d.name_norm) {
        println!(
            "gitfull: dep `{}`: shared lib cache hit (entry {}, built by an \
             earlier install — no rebuild)",
            d.name, key
        );
        return Ok(DepPlan::CacheHit(dir));
    }
    // curated upstream map: the sound strategy for module names — a
    // well-known module's correct upstream is knowledge, not something
    // popularity ranking can infer. Extensible/overridable via [dep].
    if let Some(entry) = crate::libmap::lookup(&d.name_norm) {
        let mut spec = PkgSpec::parse(entry.source)?;
        if spec.git_ref.is_none() {
            spec.git_ref = entry.git_ref.map(|s| s.to_string());
        }
        println!(
            "gitfull: dep `{}`: curated upstream map -> {}{} ({}) — well-known \
             module; sibling module names mapping to this same source \
             deduplicate to one fetch/build",
            d.name,
            entry.source,
            entry
                .git_ref
                .map(|r| format!(" (ref {r})"))
                .unwrap_or_default(),
            entry.label
        );
        return identity_or_fetch(cfg, cache, &d.name_norm, DepSpec::Package(spec));
    }
    // ranked forge search: FLAGGED FALLBACK — candidates only, never an
    // auto-selected build (see DepPlan::Unconfirmed)
    let candidates = search::rank_dep_candidates(cfg, ctx, &d.name)?;
    Ok(DepPlan::Unconfirmed(candidates))
}

/// Resolve a *pinned* dependency source (user `[dep.<name>]` override or
/// meson wrap) down to its fetch identity and check the shared cache at
/// the **identity** level: if this exact source (URL+ref / path /
/// tarball URL) was already built by an earlier install, reuse that
/// entry outright — no clone, no rebuild. Only a miss falls through to
/// a fetch. The cache key here matches the one the walk assigns when a
/// fetched node is registered (`identity_key(name_norm, identity)`).
fn identity_or_fetch(
    cfg: &Config,
    cache: &LibCache,
    name_norm: &str,
    spec: DepSpec,
) -> Result<DepPlan> {
    let planned = plan_dep_fetch(cfg, &spec)?;
    let key = LibCache::identity_key(name_norm, &planned.identity);
    if cache.entry_meta(&key).is_some() {
        println!(
            "gitfull: dep `{}`: this exact source is already built (shared lib \
             cache entry {key}) — reusing, nothing fetched",
            name_norm
        );
        return Ok(DepPlan::CacheHit(cache.entry_dir(&key)));
    }
    // the same physical source registered by an earlier install under
    // an alias name (identity reconstructs from the entry's provenance)
    if let Some((alias_key, dir)) = cache.find_by_identity(&planned.identity) {
        println!(
            "gitfull: dep `{}`: this exact source is already built (entry {alias_key}, \
             registered under another name) — reusing, nothing fetched",
            name_norm
        );
        return Ok(DepPlan::CacheHit(dir));
    }
    Ok(DepPlan::Fetch(spec))
}

/// Print the dependency-scan summary for one source tree.
fn print_dep_scan(declared: &DeclaredDeps) {
    let req = declared.required_names();
    if req.is_empty() && declared.wraps.is_empty() && declared.optional.is_empty() {
        println!("gitfull: no declared library dependencies found");
        return;
    }
    for d in req {
        println!(
            "gitfull:   dep {} {}({}; {})",
            d.name,
            d.version
                .as_deref()
                .map(|v| format!("{v} "))
                .unwrap_or_default(),
            d.kind.label(),
            d.origin
        );
    }
    for d in &declared.optional {
        println!(
            "gitfull:   dep {} ({}; optional per the manifest — reported, \
             not provisioned)",
            d.name, d.kind.label()
        );
    }
}

/// Fetch one dependency's source into its sandbox — with the same live
/// progress UI the main repo clone uses.
fn fetch_dep_source(
    cfg: &Config,
    fetch_ctx: &ExecCtx,
    node_sb: &Sandbox,
    fetch: &Fetch,
) -> Result<Option<String>> {
    match fetch {
        Fetch::Git { url, git_ref } => {
            println!("gitfull: cloning {}", url.clean);
            let mut ui = clone_progress(cfg);
            gitproc::git_clone(
                fetch_ctx,
                url,
                &node_sb.src(),
                git_ref.as_deref(),
                &cfg.clone,
                &cfg.host_tool_path,
                &node_sb.env(),
                Some(&mut ui),
            )?;
            let commit =
                gitproc::git_rev_parse_head(fetch_ctx, &node_sb.src(), &cfg.host_tool_path, &node_sb.env())
                    .ok();
            ui.finish(&format!(
                "gitfull: cloned {} ({})",
                url.clean,
                commit.as_deref().unwrap_or("unknown commit")
            ));
            Ok(commit)
        }
        Fetch::Local { path } => {
            println!("gitfull: local source {}", path.display());
            copy_tree(path, &node_sb.src())?;
            Ok(None)
        }
        Fetch::Tarball { url, patch_url } => {
            // [wrap-file]: download the pinned tarball, extract (lifting a
            // single top-level directory), optionally apply the wrap patch
            let digest = crate::sha256::sha256_hex(url.as_bytes());
            let name = util::sanitize_component(&digest[..16.min(digest.len())]);
            let tarball = cfg.cache_dir.join(format!("dep-{name}.tar"));
            println!("gitfull: downloading {url}");
            gitproc::curl_download(fetch_ctx, url, &tarball, &cfg.host_tool_path)?;
            let extract_dir = node_sb.dir.join(".extract");
            if extract_dir.exists() {
                fs::remove_dir_all(&extract_dir)?;
            }
            fs::create_dir_all(&extract_dir)?;
            gitproc::run(
                fetch_ctx,
                &[
                    "tar".into(),
                    "-xf".into(),
                    tarball.display().to_string(),
                    "-C".into(),
                    extract_dir.display().to_string(),
                ],
                ExecClass::HostUtility,
                Path::new("."),
                &[("PATH".into(), cfg.host_tool_path.clone()), ("LC_ALL".into(), "C".into())],
                None,
            )?;
            // lift a single top-level dir (libfoo-1.2/...) into src/
            let entries: Vec<_> = fs::read_dir(&extract_dir)?.flatten().collect();
            if entries.len() == 1 && entries[0].path().is_dir() {
                for e in fs::read_dir(entries[0].path())?.flatten() {
                    let dest = node_sb.src().join(e.file_name());
                    fs::rename(e.path(), &dest)?;
                }
            } else {
                for e in entries {
                    let dest = node_sb.src().join(e.file_name());
                    fs::rename(e.path(), &dest)?;
                }
            }
            let _ = fs::remove_dir_all(&extract_dir);
            if let Some(pu) = patch_url {
                let patch_file = cfg.cache_dir.join(format!("dep-{name}.patch"));
                println!("gitfull: downloading wrap patch {pu}");
                gitproc::curl_download(fetch_ctx, pu, &patch_file, &cfg.host_tool_path)?;
                gitproc::run(
                    fetch_ctx,
                    &[
                        "patch".into(),
                        "-p1".into(),
                        "-i".into(),
                        patch_file.display().to_string(),
                    ],
                    ExecClass::HostUtility,
                    &node_sb.src(),
                    &[("PATH".into(), cfg.host_tool_path.clone()), ("LC_ALL".into(), "C".into())],
                    None,
                )?;
            }
            Ok(None)
        }
    }
}

/// True if `ancestor_idx` is on the parent chain of `of_idx`.
fn is_ancestor(nodes: &[DepNode], ancestor_idx: usize, of_idx: usize) -> bool {
    let mut cur = of_idx;
    loop {
        if cur == ancestor_idx {
            return true;
        }
        let p = nodes[cur].parent;
        if p == cur {
            return false;
        }
        cur = p;
    }
}

/// A queued dependency edge (parent node index + how to fetch it).
struct QueuedDep {
    spec: DepSpec,
    parent: usize,
    /// Manifest-declared name (display) for this dependency, if any.
    declared: Option<String>,
    name_norm: Option<String>,
    /// Name component of the shared-cache key (declared name or repo).
    cache_name: String,
}

/// A `DepSpec` resolved down to a concrete fetch + identity.
struct PlannedFetch {
    fetch: Fetch,
    key: String,
    identity: String,
    sandbox_name: String,
    /// `(owner, repo)` when forge-resolved (for `[repo]` overrides).
    owner_repo: Option<(String, String)>,
}

fn plan_dep_fetch(cfg: &Config, spec: &DepSpec) -> Result<PlannedFetch> {
    match spec {
        DepSpec::Package(p) => match resolve_source(cfg, p)? {
            ResolvedSource::Git {
                forge_name,
                owner,
                repo,
                url,
                git_ref,
            } => {
                let identity =
                    format!("{}|{}", url.clean, git_ref.clone().unwrap_or_default());
                Ok(PlannedFetch {
                    fetch: Fetch::Git { url, git_ref },
                    key: p.key(),
                    identity,
                    sandbox_name: Sandbox::name_for(&forge_name, &owner, &repo),
                    owner_repo: Some((owner, repo)),
                })
            }
            ResolvedSource::Local(path) => {
                let fname = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "local".into());
                let identity = path.display().to_string();
                Ok(PlannedFetch {
                    fetch: Fetch::Local { path },
                    key: p.key(),
                    identity,
                    sandbox_name: util::sanitize_component(&fname),
                    owner_repo: None,
                })
            }
        },
        DepSpec::WrapGit(w) => {
            let (url_s, rev) = w
                .git
                .clone()
                .expect("internal: WrapGit queued without a url");
            let url = authed_url(&url_s, None);
            let identity = format!("{url_s}|{rev}");
            Ok(PlannedFetch {
                fetch: Fetch::Git {
                    url,
                    git_ref: Some(rev),
                },
                key: format!("wrap:{}", w.name),
                identity,
                sandbox_name: format!("wrap-{}", util::sanitize_component(&w.name)),
                owner_repo: None,
            })
        }
        DepSpec::WrapFile(w) => {
            let (url_s, patch) = w
                .file
                .clone()
                .expect("internal: WrapFile queued without a url");
            Ok(PlannedFetch {
                fetch: Fetch::Tarball {
                    url: url_s.clone(),
                    patch_url: patch,
                },
                key: format!("wrap:{}", w.name),
                identity: url_s,
                sandbox_name: format!("wrap-{}", util::sanitize_component(&w.name)),
                owner_repo: None,
            })
        }
    }
}

/// Confirm (or refuse) an **unconfirmed** ranked-search candidate for
/// a dependency name. This is the gate that makes the search fallback
/// safe: no search result is ever built without either
///
/// * an explicit interactive confirmation (TTY `y`), or
/// * a `[dep.<name>]` pin in gitfull.conf (the non-interactive path —
///   which is also what `--dry-run`/scripts/CI should use).
///
/// `--yes` deliberately does NOT bypass this gate: it suppresses
/// *prompts*, not the missing confirmation itself.
///
/// `tty` decides WHICH path runs — and when it is false the hard error
/// returns immediately, with **no blocking stdin read attempted at all**.
/// Callers must derive it from a both-ends-of-the-prompt check
/// ([`ExecCtx::interactive`] — stdin AND stdout real terminals), never
/// from stdin alone: an inherited-but-unserviced terminal on fd 0
/// (makepkg/CI/packaging builds) must not be mistaken for a human who
/// can answer. `tty` and `read_line` are parameters so tests can drive
/// both sides without a real terminal — and without depending on the
/// ambient fd state the test runner happened to inherit.
fn confirm_unconfirmed_search_dep(
    name: &str,
    cands: &[search::Candidate],
    tty: bool,
    read_line: &dyn Fn() -> Option<String>,
) -> Result<PkgSpec> {
    let top = &cands[0];
    let stars = if top.stars >= 10.0 {
        format!("{} stars", top.stars as u64)
    } else {
        format!(
            "{} stars — NEAR-ZERO signal, likely a wrong repo",
            top.stars as u64
        )
    };
    let pin_hint = format!(
        "pin the correct source in gitfull.conf:\n    \
         [dep.\"{name}\"]\n    source = \"forge:owner/repo\"   # or any git URL"
    );
    if !tty {
        return Err(GitfullError::Unsupported(format!(
            "dep `{name}` has NO curated mapping, [dep] pin, wrap or cached \
             build — only ranked-search candidates exist, and the top one \
             ({}:{} — {stars}) is UNCONFIRMED: popularity ranking cannot \
             establish upstream identity for a library module name. Refusing \
             to auto-build an unconfirmed match. Either {pin_hint} or run \
             interactively on a TTY to confirm once",
            top.forge,
            top.full_name()
        )));
    }
    print!(
        "gitfull: dep `{name}`: build the UNCONFIRMED search match {}:{} \
         (rank 1 of {}, {stars})? [y/N] ",
        top.forge,
        top.full_name(),
        cands.len()
    );
    use std::io::Write;
    std::io::stdout().flush().ok();
    match read_line() {
        Some(line) if line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes") => {
            println!(
                "gitfull: confirmed — for reproducible installs, also pin it: \
                 [dep.\"{name}\"] source = \"{}:{}\"",
                top.forge,
                top.full_name()
            );
            Ok(PkgSpec {
                source: Source::Forge {
                    forge: Some(top.forge.clone()),
                    owner: top.owner.clone(),
                    repo: top.repo.clone(),
                },
                git_ref: None,
            })
        }
        _ => Err(GitfullError::Unsupported(format!(
            "aborted at unconfirmed dependency `{name}` — {pin_hint}"
        ))),
    }
}

/// Read one line from the real stdin (the interactive confirmation
/// path; tests inject their own closure instead).
///
/// ONLY reachable behind a both-ends TTY check ([`ExecCtx::interactive`]):
/// a blocking read anywhere else is the packaging-pipeline hang — a
/// non-interactive session must fail fast instead of waiting forever.
fn read_stdin_line() -> Option<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok().map(|_| line)
}

/// Plan every declared dependency of one scanned tree: adds fetches to
/// the walk queue, returns shared-cache hits as `(name, dir)` pairs.
///
/// Wraps that no `dependency()` call references are still fetched — the
/// same policy `meson subprojects download` uses: a wrap file is the
/// project's own pin, and honoring it can only over-provide, never
/// under-provide. (Vendored in-tree subprojects are excluded here — they
/// need no fetch.)
///
/// `dry_run` only changes how an **unconfirmed** search fallback is
/// handled: a real install must confirm (TTY) or fail (pin hint), while
/// a dry run reports the dep as unresolved and skips its subtree.
fn plan_scan(
    cfg: &Config,
    ctx: &ExecCtx,
    cache: &LibCache,
    scan: &depgraph::DeclaredDeps,
    parent: usize,
    queue: &mut VecDeque<QueuedDep>,
    dry_run: bool,
) -> Result<Vec<(String, PathBuf)>> {
    let mut hits = Vec::new();
    for d in scan
        .required
        .iter()
        .filter(|d| d.kind != crate::depgraph::DepKind::CrateRegistry)
    {
        match plan_declared_dep(cfg, ctx, cache, scan, d)? {
            DepPlan::Satisfied => {}
            DepPlan::CacheHit(dir) => {
                // the hit entry AND its link closure (the entries it was
                // built against) must all be linkable by the dependent
                let key = dir
                    .file_name()
                    .map(|k| k.to_string_lossy().to_string())
                    .unwrap_or_default();
                for cdir in cache.closure_dirs(&key) {
                    if hits.iter().any(|(_, d)| d == &cdir) {
                        continue;
                    }
                    let name = cache
                        .entry_meta(
                            &cdir
                                .file_name()
                                .map(|k| k.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        )
                        .map(|m| m.name)
                        .unwrap_or_else(|| d.name.clone());
                    hits.push((name, cdir));
                }
            }
            DepPlan::Fetch(spec) => queue.push_back(QueuedDep {
                spec,
                parent,
                declared: Some(d.name.clone()),
                name_norm: Some(d.name_norm.clone()),
                cache_name: d.name_norm.clone(),
            }),
            DepPlan::Unconfirmed(cands) => {
                if dry_run {
                    println!(
                        "gitfull: dep `{}`: UNRESOLVED — only unconfirmed search \
                         candidates exist; a real install stops here for a \
                         [dep] pin or an interactive confirmation. Nothing \
                         would be auto-built.",
                        d.name
                    );
                } else {
                    // non-interactive sessions (no real terminal on BOTH
                    // ends of the prompt) hard-error HERE — the read is
                    // only ever attempted behind a both-fds TTY check,
                    // never on an inherited fd nobody is servicing
                    let spec = confirm_unconfirmed_search_dep(
                        &d.name,
                        &cands,
                        ctx.interactive(),
                        &read_stdin_line,
                    )?;
                    queue.push_back(QueuedDep {
                        spec: DepSpec::Package(spec),
                        parent,
                        declared: Some(d.name.clone()),
                        name_norm: Some(d.name_norm.clone()),
                        cache_name: d.name_norm.clone(),
                    });
                }
            }
        }
    }
    for w in &scan.wraps {
        let spec = if w.git.is_some() {
            DepSpec::WrapGit(w.clone())
        } else {
            DepSpec::WrapFile(w.clone())
        };
        println!(
            "gitfull: dep `{}`: meson wrap pin {} — honoring the project's own \
             subproject file",
            w.name,
            w.origin
        );
        queue.push_back(QueuedDep {
            spec,
            parent,
            declared: Some(w.name.clone()),
            name_norm: Some(w.name.to_ascii_lowercase()),
            cache_name: w.name.to_ascii_lowercase(),
        });
    }
    Ok(hits)
}

/// The link-time prefix list for building node `idx`: for every built
/// child (each ends up as a shared-cache entry) the FULL closure of
/// that entry — the entry itself plus every entry it was built against
/// — plus this node's own plan-time cache hits. This is what lets a
/// *reused* static library link: `libfoo.a` members referencing
/// `libbar.a` symbols need `libbar` on the line even though this
/// install never fetched it, and the build system's own `.pc`
/// `Requires:` chain only resolves when every closure member's
/// pkgconfig dir is on `PKG_CONFIG_PATH`.
fn link_prefixes(cache: &LibCache, nodes: &[DepNode], idx: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let push = |dir: PathBuf, out: &mut Vec<PathBuf>| {
        if !out.contains(&dir) {
            out.push(dir);
        }
    };
    for &c in &nodes[idx].children {
        if nodes[c].prefix.is_some() {
            for dir in cache.closure_dirs(&nodes[c].cache_key) {
                push(dir, &mut out);
            }
        }
    }
    for dir in nodes[idx].lib_hits.iter().cloned() {
        push(dir, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub fn install(
    cfg: &Config,
    base_ctx: &ExecCtx,
    spec: &PkgSpec,
    opts: &InstallOpts,
) -> Result<Option<InstallRecord>> {
    crate::privilege::require_root(cfg, "install")?;
    cfg.ensure_root()?;
    let source = resolve_source(cfg, spec)?;

    let (forge_name, owner, repo, sandbox_name) = match &source {
        ResolvedSource::Git {
            forge_name,
            owner,
            repo,
            ..
        } => {
            let n = Sandbox::name_for(forge_name, owner, repo);
            (forge_name.clone(), owner.clone(), repo.clone(), n)
        }
        ResolvedSource::Local(p) => {
            let n = util::sanitize_component(
                &p.file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "local".into()),
            );
            ("local".to_string(), String::new(), String::new(), n)
        }
    };

    let sb = Sandbox::for_app(&cfg.apps_dir, &sandbox_name);

    // Reinstall handling — the prompt is only ever attempted when the
    // session is interactive on BOTH ends (prompt visible on stdout,
    // answerable from stdin); non-interactive runs proceed straight
    // to the reinstall (opts.yes) without any blocking read
    if sb.meta_path().exists() {
        if !opts.yes && base_ctx.interactive() {
            print!("gitfull: `{sandbox_name}` is already installed — reinstall? [y/N] ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("gitfull: aborted");
                return Ok(None);
            }
        }
        println!("gitfull: reinstalling `{sandbox_name}` (wiping sandbox)");
        if sb.dir.exists() {
            fs::remove_dir_all(&sb.dir)?;
        }
    }
    sb.create()?;

    // ---- fetch source -----------------------------------------------------
    let mut fetch_ctx = base_ctx.clone();
    fetch_ctx.resolve_path = cfg.host_tool_path.clone();
    let git_home = sb.env();

    match &source {
        ResolvedSource::Git { url, git_ref, .. } => {
            println!("gitfull: cloning {}", url.clean);
            let mut ui = ProgressUi::new(
                util::is_tty(2),
                util::term_width(),
                match cfg.color {
                    crate::config::ColorChoice::Always => true,
                    crate::config::ColorChoice::Never => false,
                    crate::config::ColorChoice::Auto => util::colors_enabled(false, false),
                },
            );
            gitproc::git_clone(
                &fetch_ctx,
                url,
                &sb.src(),
                git_ref.as_deref(),
                &cfg.clone,
                &cfg.host_tool_path,
                &git_home,
                Some(&mut ui),
            )?;
            let commit =
                gitproc::git_rev_parse_head(&fetch_ctx, &sb.src(), &cfg.host_tool_path, &git_home)
                    .ok();
            ui.finish(&format!(
                "gitfull: cloned {} ({})",
                url.clean,
                commit.as_deref().unwrap_or("unknown commit")
            ));
        }
        ResolvedSource::Local(p) => {
            println!("gitfull: local source {}", p.display());
            copy_tree(p, &sb.src())?;
        }
    }

    // ---- resolve repo ------------------------------------------------------
    let key = spec.key();
    let ov = cfg.repo_override_for(&owner, &repo).cloned();
    let resolved = resolve_repo(&sb.src(), &key, ov.as_ref())?;
    println!(
        "gitfull: detected build system: {} (auto-detected from repo files)",
        resolved.build.label()
    );

    // ---- dependency-graph discovery (the repo's own manifests) -------------
    let libcache = LibCache::new(cfg.libs_dir.clone());
    println!(
        "gitfull: scanning {} manifests for declared dependencies \
         (discovery parses the project's own files — generic across repos)",
        resolved.build.label()
    );
    let declared = depgraph::scan(&sb.src(), resolved.build)?;
    print_dep_scan(&declared);

    // ---- dependency graph walk (BFS, cycle-safe) ---------------------------
    let app_identity = match &source {
        ResolvedSource::Git { url, git_ref, .. } => {
            format!("{}|{}", url.clean, git_ref.clone().unwrap_or_default())
        }
        ResolvedSource::Local(p) => p.display().to_string(),
    };
    // node 0 = the app itself (never registered into the lib cache)
    let mut nodes: Vec<DepNode> = vec![DepNode {
        key: key.clone(),
        identity: app_identity.clone(),
        cache_key: String::new(),
        declared: None,
        name_norm: None,
        sandbox: Sandbox { dir: sb.dir.clone() },
        resolved: resolved.clone(),
        parent: 0,
        children: Vec::new(),
        lib_hits: Vec::new(),
        fetch: Fetch::Local { path: PathBuf::new() },
        prefix: None,
        commit: None,
    }];
    let mut visited: BTreeMap<String, usize> = BTreeMap::from([(app_identity, 0usize)]);
    let mut queue: VecDeque<QueuedDep> = resolved
        .packages
        .iter()
        .map(|p| QueuedDep {
            spec: DepSpec::Package(p.clone()),
            parent: 0,
            declared: None,
            name_norm: None,
            cache_name: spec_repo(p),
        })
        .collect();
    let app_hits = plan_scan(cfg, &fetch_ctx, &libcache, &declared, 0, &mut queue, opts.dry_run)?;
    nodes[0].lib_hits = app_hits.iter().map(|(_, d)| d.clone()).collect();

    while let Some(q) = queue.pop_front() {
        let planned = plan_dep_fetch(cfg, &q.spec)?;
        if planned.identity == nodes[q.parent].identity {
            println!(
                "gitfull: note: `{}` resolves to the same source as its parent — \
                 skipping (no self-dependency)",
                planned.key
            );
            continue;
        }
        if let Some(&existing) = visited.get(&planned.identity) {
            // a back-edge onto an ancestor is a cycle; an edge onto an
            // already-visited non-ancestor is a diamond (dedup)
            if is_ancestor(&nodes, existing, q.parent) {
                let mut path = vec![nodes[existing].key.clone()];
                let mut cur = q.parent;
                while cur != existing {
                    path.push(nodes[cur].key.clone());
                    cur = nodes[cur].parent;
                }
                path.push(nodes[existing].key.clone());
                return Err(GitfullError::Unsupported(format!(
                    "dependency cycle: {}",
                    path.join(" -> ")
                )));
            }
            nodes[q.parent].children.push(existing);
            continue;
        }
        println!(
            "gitfull: dependency {} (required by {})",
            planned.key, nodes[q.parent].key
        );

        // sandbox name is unique per identity: base name + key digest
        let digest = LibCache::identity_key(&q.cache_name, &planned.identity);
        let short = digest.rsplit('-').next().unwrap_or("x").to_string();
        let sandbox_name = format!("{}-{short}", planned.sandbox_name);
        let depsb = Sandbox::for_app(&sb.deps(), &sandbox_name);
        if depsb.dir.exists() {
            fs::remove_dir_all(&depsb.dir)?;
        }
        depsb.create()?;
        let commit = fetch_dep_source(cfg, &fetch_ctx, &depsb, &planned.fetch)?;
        let dep_ov = planned
            .owner_repo
            .as_ref()
            .and_then(|(o, r)| cfg.repo_override_for(o, r).cloned());
        let dep_resolved = resolve_repo(&depsb.src(), &planned.key, dep_ov.as_ref())?;

        // discover THIS dependency's own declared dependencies (the walk
        // is transitive — same generic parsers, same resolution layers)
        let dep_scan = depgraph::scan(&depsb.src(), dep_resolved.build)?;
        let node_idx = nodes.len();
        let dep_hits = plan_scan(cfg, &fetch_ctx, &libcache, &dep_scan, node_idx, &mut queue, opts.dry_run)?;
        for p in &dep_resolved.packages {
            queue.push_back(QueuedDep {
                spec: DepSpec::Package(p.clone()),
                parent: node_idx,
                declared: None,
                name_norm: None,
                cache_name: spec_repo(p),
            });
        }
        nodes.push(DepNode {
            key: planned.key.clone(),
            identity: planned.identity.clone(),
            cache_key: digest,
            declared: q.declared.clone(),
            name_norm: q.name_norm.clone(),
            sandbox: depsb,
            resolved: dep_resolved,
            parent: q.parent,
            children: Vec::new(),
            lib_hits: dep_hits.into_iter().map(|(_, d)| d).collect(),
            fetch: planned.fetch.clone(),
            prefix: None,
            commit,
        });
        visited.insert(planned.identity.clone(), node_idx);
        nodes[q.parent].children.push(node_idx);
    }

    // ---- merged toolchain needs ---------------------------------------------
    let mut needs: BTreeMap<String, crate::resolver::Constraint> = resolved.toolchains.clone();
    for n in nodes.iter().skip(1) {
        for (comp, c) in &n.resolved.toolchains {
            let merged = needs
                .get(comp)
                .map(|e| e.merge(c))
                .unwrap_or_else(|| c.clone());
            needs.insert(comp.clone(), merged);
        }
    }
    let need_list: Vec<String> = needs
        .iter()
        .map(|(c, k)| {
            if k.op == crate::resolver::CmpOp::Any {
                c.clone()
            } else {
                format!("{c}{}{}", op_text(k.op), k.version.as_deref().unwrap_or(""))
            }
        })
        .collect();
    println!("gitfull: toolchain requirements: {}", need_list.join(", "));

    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let mut selections: BTreeMap<String, Selection> = BTreeMap::new();
    let mut missing: Vec<(String, crate::resolver::Constraint)> = Vec::new();
    for (comp, c) in &needs {
        match mgr.find(comp, c) {
            Some(sel) => {
                println!(
                    "gitfull: toolchain {}-{} (shared: {})",
                    sel.component,
                    sel.version,
                    sel.dir.display()
                );
                selections.insert(comp.clone(), sel);
            }
            None => missing.push((comp.clone(), c.clone())),
        }
    }

    if !missing.is_empty() {
        if opts.dry_run {
            // provisioning plan only — nothing is fetched or built
            bootstrap::ensure_components(cfg, base_ctx, &missing, true)?;
        } else {
            // Automatic toolchain provisioning: the seed GCC build is the
            // single host-touching step; everything afterwards is built
            // with toolchain-managed tools. The user never runs a manual
            // bootstrap on the normal path.
            bootstrap::ensure_components(cfg, base_ctx, &missing, false)?;
            for (comp, c) in &missing {
                match mgr.find(comp, c) {
                    Some(sel) => {
                        println!(
                            "gitfull: toolchain {}-{} (shared: {})",
                            sel.component,
                            sel.version,
                            sel.dir.display()
                        );
                        selections.insert(comp.clone(), sel);
                    }
                    None => {
                        return Err(GitfullError::Toolchain {
                            component: comp.clone(),
                            message: format!(
                                "auto-provisioning completed but `{comp}` still \
                                 does not satisfy the requirement — please report \
                                 this as a bug"
                            ),
                        })
                    }
                }
            }
        }
    }

    if opts.dry_run {
        println!("gitfull (dry-run): plan");
        println!("  sandbox: {}", sb.dir.display());
        println!(
            "  build:   {} ({} steps)",
            resolved.build.label(),
            build_commands(resolved.build, &sb, cfg.jobs).len()
        );
        for n in nodes.iter().skip(1) {
            println!(
                "  dep:     {} [{}] from {}{}",
                n.key,
                n.resolved.build.label(),
                n.fetch.label(),
                if libcache.entry_meta(&n.cache_key).is_some()
                    || libcache.find_by_identity(&n.identity).is_some()
                {
                    " (shared-cache hit — would NOT rebuild)"
                } else {
                    ""
                }
            );
        }
        for dir in &nodes[0].lib_hits {
            println!(
                "  lib:     shared-cache entry {} (already built — reuse)",
                dir.display()
            );
        }
        println!("  install: final binaries -> {}", cfg.bin_dir.display());
        println!("gitfull (dry-run): stopping before any build; no changes installed.");
        return Ok(None);
    }

    // ---- topological build order (dependencies before dependents) ----------
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &c in &n.children {
            dependents[c].push(i);
        }
    }
    let mut pending: Vec<usize> = nodes.iter().map(|n| n.children.len()).collect();
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|&i| pending[i] == 0).collect();
    let mut order: Vec<usize> = Vec::new();
    while let Some(i) = ready.pop() {
        order.push(i);
        for &d in &dependents[i] {
            pending[d] -= 1;
            if pending[d] == 0 {
                ready.push(d);
            }
        }
    }
    if order.len() != nodes.len() {
        let stuck: Vec<String> = (0..nodes.len())
            .filter(|i| !order.contains(i))
            .map(|i| nodes[i].key.clone())
            .collect();
        return Err(GitfullError::Unsupported(format!(
            "dependency cycle among: {}",
            stuck.join(" -> ")
        )));
    }

    // ---- build dependencies (leaves first), then the app -------------------
    let tc_bins: Vec<PathBuf> = selections.values().map(|s| s.bin.clone()).collect();
    let tc_libs: Vec<PathBuf> = selections
        .values()
        .flat_map(|s| {
            let mut v = vec![s.dir.join("lib"), s.dir.join("lib64")];
            v.retain(|p| p.is_dir());
            v
        })
        .collect();
    let mut extra: Vec<(String, String)> = Vec::new();
    if let Some(gcc) = selections.get("gcc") {
        extra.push(("CC".into(), gcc.bin.join("gcc").display().to_string()));
        extra.push(("CXX".into(), gcc.bin.join("g++").display().to_string()));
    }
    if let Some(py) = selections.get("python") {
        extra.push((
            "PYTHON".into(),
            py.bin.join("python3").display().to_string(),
        ));
    }

    for &idx in &order {
        if idx == 0 {
            continue; // the app itself builds after all dependencies
        }
        // identity hit: this exact source is already built and cached —
        // the core of "a second app never rebuilds the same library".
        // Looked up under this node's key AND by identity (an earlier
        // install may have registered it under an alias name).
        let identity_hit = if libcache.entry_meta(&nodes[idx].cache_key).is_some() {
            Some(nodes[idx].cache_key.clone())
        } else {
            libcache
                .find_by_identity(&nodes[idx].identity)
                .map(|(k, _)| k)
        };
        if let Some(key) = identity_hit {
            println!(
                "gitfull: dependency {} satisfied by shared lib cache entry {} \
                 (this exact source was built by an earlier install — no rebuild)",
                nodes[idx].key, key
            );
            nodes[idx].cache_key = key;
            nodes[idx].prefix = Some(libcache.entry_dir(&nodes[idx].cache_key));
            continue;
        }
        let child_prefixes: Vec<PathBuf> = link_prefixes(&libcache, &nodes, idx);
        let mut node_extra = extra.clone();
        if nodes[idx].resolved.build == crate::manifest::BuildSystem::Cargo {
            // keep cargo's target dir inside the sandbox build area —
            // `build/target`, exactly where the binary collector looks
            // (and never inside src/, which walk_files prunes)
            node_extra.push((
                "CARGO_TARGET_DIR".into(),
                nodes[idx].sandbox.build().join("target").display().to_string(),
            ));
        }
        let dep_env = nodes[idx].sandbox.build_env(
            &tc_bins,
            &tc_libs,
            &child_prefixes,
            &cfg.host_tool_path,
            &node_extra,
        );
        let jobs = nodes[idx].resolved.jobs.unwrap_or(cfg.jobs);
        let steps = build_commands(nodes[idx].resolved.build, &nodes[idx].sandbox, jobs);
        let dep_ctx = ExecCtx {
            resolve_path: dep_env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| cfg.host_tool_path.clone()),
            ..base_ctx.clone()
        };
        println!(
            "gitfull: building dependency {} [{}]",
            nodes[idx].key,
            nodes[idx].resolved.build.label()
        );
        run_build(&dep_ctx, &nodes[idx].sandbox, &steps, &dep_env, opts.verbose)?;
        promote_stage(
            &stage_root(&nodes[idx].sandbox),
            &nodes[idx].sandbox.prefix(),
        )?;

        // register the built library in the shared cache: the install
        // prefix moves to <root>/libs/<key>/ so every later app (and every
        // later install) reuses it instead of rebuilding
        let mut provides = libcache::provided_names(&nodes[idx].sandbox.prefix());
        let display_name = nodes[idx]
            .declared
            .clone()
            .unwrap_or_else(|| nodes[idx].key.clone());
        let name_norm = nodes[idx]
            .name_norm
            .clone()
            .unwrap_or_else(|| display_name.to_ascii_lowercase());
        if !provides.contains(&name_norm) {
            provides.push(name_norm);
        }
        let git_ref = match &nodes[idx].fetch {
            Fetch::Git { git_ref, .. } => git_ref.clone(),
            _ => None,
        };
        // the link closure this library was built against: children
        // (built or reused entries) + plan-time cache hits — recorded so
        // a later app reusing THIS entry links its closure too
        let mut requires: Vec<String> =
            nodes[idx].children.iter().map(|&c| nodes[c].cache_key.clone()).collect();
        for dir in &nodes[idx].lib_hits {
            if let Some(k) = dir.file_name().map(|k| k.to_string_lossy().to_string()) {
                if !requires.contains(&k) {
                    requires.push(k);
                }
            }
        }
        let meta = LibMeta {
            name: display_name.clone(),
            provides,
            source: nodes[idx].fetch.label(),
            git_ref,
            commit: nodes[idx].commit.clone(),
            requires,
            built_by: "gitfull toolchain (toolchain-managed gcc)".into(),
            date_epoch: util::epoch(),
        };
        let dir = libcache.register(
            &nodes[idx].cache_key,
            &nodes[idx].sandbox.prefix(),
            &meta,
        )?;
        println!(
            "gitfull: dependency {} built and cached: {} (provides: {})",
            nodes[idx].key,
            dir.display(),
            meta.provides.join(", ")
        );
        nodes[idx].prefix = Some(dir);
    }

    println!(
        "gitfull: building {} [{}]",
        sandbox_name,
        resolved.build.label()
    );
    let dep_prefixes: Vec<PathBuf> = link_prefixes(&libcache, &nodes, 0);
    let mut app_extra = extra.clone();
    if resolved.build == crate::manifest::BuildSystem::Cargo {
        // cargo's target dir: build/target — where the collector looks
        app_extra.push((
            "CARGO_TARGET_DIR".into(),
            sb.build().join("target").display().to_string(),
        ));
    }
    let app_env = sb.build_env(
        &tc_bins,
        &tc_libs,
        &dep_prefixes,
        &cfg.host_tool_path,
        &app_extra,
    );
    let jobs = resolved.jobs.unwrap_or(cfg.jobs);
    let steps = build_commands(resolved.build, &sb, jobs);
    let app_ctx = ExecCtx {
        resolve_path: app_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| cfg.host_tool_path.clone()),
        ..base_ctx.clone()
    };
    run_build(&app_ctx, &sb, &steps, &app_env, opts.verbose)?;

    // collect final binaries from the staging area BEFORE promotion
    let bins = collect_bins(&sb, resolved.build, &resolved.bins)?;
    if bins.is_empty() {
        return Err(GitfullError::Sandbox(format!(
            "build finished but no final binaries were found in {} \
             (stage/, prefix/bin, build/target/release)",
            sb.dir.display()
        )));
    }
    for b in &bins {
        println!(
            "gitfull: built binary {} ({})",
            b.display(),
            util::format_bytes(fs::metadata(b).map(|m| m.len()).unwrap_or(0) as f64)
        );
    }

    // ---- THE sandbox escape: copy final binaries out ---------------------------
    let metas = install_binaries(cfg, &app_ctx, &bins, opts.yes)?;
    // after the copies, fold the staged install into the in-sandbox prefix
    promote_stage(&stage_root(&sb), &sb.prefix())?;
    for m in &metas {
        println!(
            "gitfull: installed {} (sha256 {}, {})",
            m.dest,
            &m.sha256[..12.min(m.sha256.len())],
            util::format_bytes(m.size as f64)
        );
    }

    let record = InstallRecord {
        name: sandbox_name.clone(),
        forge: forge_name,
        owner,
        repo,
        url: match &source {
            ResolvedSource::Git { url, .. } => url.clean.clone(),
            ResolvedSource::Local(p) => p.display().to_string(),
        },
        git_ref: match &source {
            ResolvedSource::Git { git_ref, .. } => git_ref.clone(),
            ResolvedSource::Local(_) => None,
        },
        commit: match &source {
            ResolvedSource::Git { .. } => {
                gitproc::git_rev_parse_head(&fetch_ctx, &sb.src(), &cfg.host_tool_path, &git_home)
                    .ok()
            }
            ResolvedSource::Local(_) => None,
        },
        build_system: resolved.build.label().to_string(),
        toolchains: selections
            .values()
            .map(|s| MetaToolchain {
                component: s.component.clone(),
                version: s.version.clone(),
            })
            .collect(),
        packages: nodes.iter().skip(1).map(|n| n.key.clone()).collect(),
        libs: lib_record(&libcache, &nodes, &app_hits),
        bins: metas,
        installed_at: util::epoch(),
    };
    write_meta(&sb.meta_path(), &record)?;
    println!("gitfull: install record: {}", sb.meta_path().display());
    println!("gitfull: sandbox: {}", sb.dir.display());
    Ok(Some(record))
}

fn op_text(op: crate::resolver::CmpOp) -> &'static str {
    use crate::resolver::CmpOp::*;
    match op {
        Any => "",
        Eq => "=",
        Gt => ">",
        Gte => ">=",
        Lt => "<",
        Lte => "<=",
    }
}

/// The `libs` section of the install record: one entry per shared-cache
/// library this install built or reused.
fn lib_record(
    cache: &LibCache,
    nodes: &[DepNode],
    app_hits: &[(String, PathBuf)],
) -> Vec<MetaLib> {
    let mut out: Vec<MetaLib> = Vec::new();
    for n in nodes.iter().skip(1) {
        if let Some(meta) = cache.entry_meta(&n.cache_key) {
            out.push(MetaLib {
                key: n.cache_key.clone(),
                name: meta.name,
                source: meta.source,
                commit: meta.commit,
            });
        }
    }
    for (name, dir) in app_hits {
        let key = dir
            .file_name()
            .map(|k| k.to_string_lossy().to_string())
            .unwrap_or_default();
        let meta = cache.entry_meta(&key);
        out.push(MetaLib {
            key,
            name: name.clone(),
            source: meta.as_ref().map(|m| m.source.clone()).unwrap_or_default(),
            commit: meta.as_ref().and_then(|m| m.commit.clone()),
        });
    }
    out
}

/// Simple recursive copy (used for local-path sources).
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let e = entry?;
        let p = e.path();
        let d = dest.join(e.file_name());
        if p.is_dir() {
            copy_tree(&p, &d)?;
        } else if p.is_symlink() {
            let target = fs::read_link(&p)?;
            let _ = std::os::unix::fs::symlink(target, &d);
        } else {
            fs::copy(&p, &d)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list / remove / update / info
// ---------------------------------------------------------------------------

pub fn list(cfg: &Config) -> Result<Vec<InstallRecord>> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&cfg.apps_dir) {
        for e in rd.flatten() {
            let meta = e.path().join("meta.toml");
            if meta.is_file() {
                if let Ok(r) = read_meta(&meta) {
                    out.push(r);
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Find an installed app by sandbox name or `owner/repo` key.
pub fn find_installed(cfg: &Config, query: &str) -> Result<Option<(PathBuf, InstallRecord)>> {
    for r in list(cfg)? {
        if r.name == query || format!("{}/{}", r.owner, r.repo) == query {
            return Ok(Some((cfg.apps_dir.join(&r.name).join("meta.toml"), r)));
        }
    }
    Ok(None)
}

pub fn remove(cfg: &Config, ctx: &ExecCtx, query: &str, yes: bool) -> Result<()> {
    crate::privilege::require_root(cfg, "remove")?;
    let (meta_path, rec) = find_installed(cfg, query)?.ok_or_else(|| {
        GitfullError::NotInstalled(format!(
            "{query} (installed: {})",
            list(cfg)
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let sandbox_dir = meta_path.parent().unwrap_or(Path::new("."));

    println!("gitfull: removing {} ({})", rec.name, rec.url);
    for b in &rec.bins {
        let dest = PathBuf::from(&b.dest);
        match sha256_file(&dest) {
            Ok(h) if h == b.sha256 => {
                fs::remove_file(&dest)?;
                gitproc::audit_event(
                    ctx,
                    "remove-binary",
                    &format!("{}\t{}\t{}", b.name, b.sha256, b.dest),
                );
                println!("gitfull: removed {}", dest.display());
            }
            Ok(_) => {
                if yes {
                    fs::remove_file(&dest)?;
                    println!(
                        "gitfull: removed {} (hash differed — forced with --yes)",
                        dest.display()
                    );
                } else {
                    println!(
                        "gitfull: warning: {} changed since install — keeping it \
                         (use --yes to force removal)",
                        dest.display()
                    );
                }
            }
            Err(_) => {
                println!("gitfull: note: {} already gone", dest.display());
            }
        }
    }
    if sandbox_dir.exists() {
        fs::remove_dir_all(sandbox_dir)?;
    }
    println!("gitfull: removed sandbox {}", sandbox_dir.display());
    Ok(())
}

pub fn update(
    cfg: &Config,
    ctx: &ExecCtx,
    query: &str,
    opts: &InstallOpts,
) -> Result<Option<InstallRecord>> {
    crate::privilege::require_root(cfg, "update")?;
    let (_meta_path, rec) = find_installed(cfg, query)?
        .ok_or_else(|| GitfullError::NotInstalled(format!("`{query}` is not installed")))?;
    let spec = PkgSpec::parse(&rec.url)?;
    let mut opts = opts.clone();
    opts.yes = true; // reinstall path already confirmed
    install(cfg, ctx, &spec, &opts)
}

pub fn info(cfg: &Config, query: &str) -> Result<()> {
    if let Some((_m, rec)) = find_installed(cfg, query)? {
        println!("name:         {}", rec.name);
        println!("source:       {}", rec.url);
        if let Some(c) = &rec.commit {
            println!("commit:       {c}");
        }
        if let Some(r) = &rec.git_ref {
            println!("ref:          {r}");
        }
        println!("build system: {}", rec.build_system);
        println!(
            "toolchains:   {}",
            rec.toolchains
                .iter()
                .map(|t| format!("{}-{}", t.component, t.version))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !rec.packages.is_empty() {
            println!("dependencies: {}", rec.packages.join(", "));
        }
        if !rec.libs.is_empty() {
            println!(
                "libraries:   {} (shared <root>/libs cache; never removed \
                 with the app)",
                rec.libs
                    .iter()
                    .map(|l| format!("{} [{}]", l.name, l.key))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for b in &rec.bins {
            println!(
                "binary:       {} (sha256 {}, {})",
                b.dest,
                b.sha256,
                util::format_bytes(b.size as f64)
            );
        }
        return Ok(());
    }
    // not installed: show forge resolution only
    let spec = PkgSpec::parse(query)?;
    // bare names go through the same ranked forge search as install —
    // read-only, no clone, resolution printed so the choice is visible
    let spec = match &spec.source {
        Source::Search { .. } => {
            let ctx = ExecCtx {
                audit_log: None,
                extra_forbidden: cfg.policy.extra_forbidden_programs.clone(),
                redactions: Vec::new(),
                resolve_path: cfg.host_tool_path.clone(),
                interactive_override: None,
            };
            search::resolve(cfg, &ctx, &spec)?
        }
        _ => spec,
    };
    match resolve_source(cfg, &spec)? {
        ResolvedSource::Git {
            forge_name,
            owner,
            repo,
            url,
            git_ref,
        } => {
            println!("not installed");
            println!("forge:   {forge_name}");
            println!("package: {owner}/{repo}");
            println!("clone:   {}", url.clean);
            if let Some(r) = git_ref {
                println!("ref:     {r}");
            }
        }
        ResolvedSource::Local(p) => {
            println!("not installed");
            println!("local:   {}", p.display());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests: resolution-layer ordering + the unconfirmed-search gate
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::gitproc::ExecCtx;

    /// A config with fake-invalid forges (search is unreachable —
    /// exactly the setup that must NEVER leak into an auto-build) and
    /// an optional extra TOML stanza.
    fn test_cfg(extra: Option<&str>) -> Config {
        let dir = std::env::temp_dir().join(format!(
            "gitfull-planner-test-{}-{}",
            std::process::id(),
            util::epoch()
        ));
        std::fs::create_dir_all(dir.join("root")).unwrap();
        let mut conf = format!(
            "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\n\n\
             [forge.github]\nkind = \"github\"\nhost = \"fake.invalid\"\napi_base = \"https://fake.invalid\"\n\n\
             [forge.gitlab]\nkind = \"gitlab\"\nhost = \"fake.invalid\"\n\n\
             [forge.codeberg]\nkind = \"forgejo\"\nhost = \"fake.invalid\"\n",
            dir.join("root").display(),
            dir.join("bin").display()
        );
        if let Some(x) = extra {
            conf.push_str(x);
        }
        let path = dir.join("gitfull.conf");
        std::fs::write(&path, conf).unwrap();
        let (cfg, warns) = Config::load(&path).unwrap();
        assert!(warns.is_empty(), "{warns:?}");
        cfg
    }

    fn ctx(cfg: &Config) -> ExecCtx {
        ExecCtx {
            audit_log: Some(cfg.root.join("audit.log")),
            extra_forbidden: Vec::new(),
            redactions: Vec::new(),
            resolve_path: cfg.host_tool_path.clone(),
            // deterministic non-interactive session: unit tests never
            // prompt regardless of the ambient fds the runner inherited
            interactive_override: Some(false),
        }
    }

    fn dep(name: &str) -> DeclaredDep {
        DeclaredDep {
            name: name.to_string(),
            name_norm: name.to_ascii_lowercase(),
            kind: crate::depgraph::DepKind::PkgConfig,
            required: true,
            version: None,
            origin: "test".to_string(),
            git_url: None,
        }
    }

    fn empty_cache(cfg: &Config) -> LibCache {
        LibCache::new(cfg.libs_dir.clone())
    }

    /// The strategy change, distilled: a well-known module name resolves
    /// through the CURATED MAP, never through search. This also proves
    /// the map is consulted without touching the network (the fake
    /// forges are unreachable; a search would fail the test).
    #[test]
    fn curated_map_beats_search_for_well_known_modules() {
        let cfg = test_cfg(None);
        let cache = empty_cache(&cfg);
        let declared = crate::depgraph::DeclaredDeps::default();
        for (name, expect_url) in [
            ("glib-2.0", "https://gitlab.gnome.org/GNOME/glib"),
            ("gio-unix-2.0", "https://gitlab.gnome.org/GNOME/glib"),
            ("cairo", "https://gitlab.freedesktop.org/cairo/cairo"),
            ("gee-0.8", "https://gitlab.gnome.org/GNOME/libgee"),
            ("sdl3", "https://github.com/libsdl-org/SDL"),
        ] {
            match plan_declared_dep(&cfg, &ctx(&cfg), &cache, &declared, &dep(name)).unwrap() {
                DepPlan::Fetch(DepSpec::Package(p)) => {
                    assert_eq!(
                        p.key(),
                        expect_url,
                        "`{name}` must resolve through the curated map"
                    );
                }
                other => panic!("`{name}`: expected a curated Fetch, got {other:?}"),
            }
        }
    }

    /// Curated entries with maintenance refs keep them (sdl2 means the
    /// SDL repo's SDL2 branch, not the default SDL3 branch).
    #[test]
    fn curated_refs_are_carried_into_the_spec() {
        let cfg = test_cfg(None);
        let cache = empty_cache(&cfg);
        let declared = crate::depgraph::DeclaredDeps::default();
        match plan_declared_dep(&cfg, &ctx(&cfg), &cache, &declared, &dep("sdl2")).unwrap() {
            DepPlan::Fetch(DepSpec::Package(p)) => {
                assert_eq!(p.key(), "https://github.com/libsdl-org/SDL");
                assert_eq!(p.git_ref.as_deref(), Some("SDL2"));
            }
            other => panic!("sdl2: expected Fetch, got {other:?}"),
        }
    }

    /// A user `[dep.<name>]` pin overrides the curated map — extension
    /// and override happen in config, never in code.
    #[test]
    fn user_dep_pin_overrides_the_curated_map() {
        let extra = "\n[dep.cairo]\nsource = \"github:myorg/my-cairo-fork\"\nref = \"stable\"\n";
        let cfg = test_cfg(Some(extra));
        let cache = empty_cache(&cfg);
        let declared = crate::depgraph::DeclaredDeps::default();
        match plan_declared_dep(&cfg, &ctx(&cfg), &cache, &declared, &dep("cairo")).unwrap() {
            DepPlan::Fetch(DepSpec::Package(p)) => {
                assert_eq!(p.key(), "myorg/my-cairo-fork");
                assert_eq!(p.git_ref.as_deref(), Some("stable"));
            }
            other => panic!("cairo: expected the user pin, got {other:?}"),
        }
    }

    /// Multiple modules of one parent (glib-2.0 + gio-unix-2.0) plan to
    /// the SAME fetch identity — the precondition for the walk's
    /// single-fetch/build deduplication.
    #[test]
    fn same_parent_modules_plan_to_one_identity() {
        let cfg = test_cfg(None);
        let cache = empty_cache(&cfg);
        let declared = crate::depgraph::DeclaredDeps::default();
        let mut identities = BTreeSet::new();
        for name in ["glib-2.0", "gio-unix-2.0", "gobject-2.0", "gio-2.0"] {
            match plan_declared_dep(&cfg, &ctx(&cfg), &cache, &declared, &dep(name)).unwrap() {
                DepPlan::Fetch(DepSpec::Package(p)) => {
                    let planned = plan_dep_fetch(&cfg, &DepSpec::Package(p)).unwrap();
                    identities.insert(planned.identity);
                }
                other => panic!("`{name}`: expected Fetch, got {other:?}"),
            }
        }
        assert_eq!(identities.len(), 1, "{identities:?}");
    }

    /// Token scoping, the incident-report arm: a GitHub PAT sitting in the
    /// environment (its forge even configured, with `token_env`) must be
    /// attached ONLY to clones of that forge's own host. A URL on an
    /// unconfigured host resolves to a strictly credential-free generic
    /// remote — `authed == clean`, token `None` — so nothing about the
    /// PAT can be presented to a host it has no relationship with.
    #[test]
    fn generic_remote_resolution_attaches_no_token_even_with_one_in_env() {
        let dir = std::env::temp_dir().join(format!(
            "gitfull-planner-scope-{}-{}",
            std::process::id(),
            util::epoch()
        ));
        std::fs::create_dir_all(dir.join("root")).unwrap();
        let conf = format!(
            "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\n\n\
             [forge.github]\nkind = \"github\"\nhost = \"fake.invalid\"\n\
             api_base = \"https://fake.invalid\"\ntoken_env = \"GITFULL_TEST_SCOPE_PAT\"\n",
            dir.join("root").display(),
            dir.join("bin").display()
        );
        let path = dir.join("gitfull.conf");
        std::fs::write(&path, conf).unwrap();
        let (cfg, warns) = Config::load(&path).unwrap();
        assert!(warns.is_empty(), "{warns:?}");

        std::env::set_var("GITFULL_TEST_SCOPE_PAT", "pat-scope-sentinel-xyz");

        // the PAT IS resolvable for the configured forge…
        let gh = cfg.forges.get("github").unwrap();
        assert_eq!(
            forge_token(gh).as_deref(),
            Some("pat-scope-sentinel-xyz"),
            "precondition: the token is visible to the configured forge"
        );

        // …but a URL on a host with NO [forge] entry resolves credential-free
        let spec = crate::spec::PkgSpec::parse("https://gitlab.freedesktop.org/x/y")
            .expect("url spec parses");
        let resolved = resolve_source(&cfg, &spec).expect("generic resolution");
        match resolved {
            ResolvedSource::Git {
                forge_name,
                url,
                git_ref,
                ..
            } => {
                assert_eq!(forge_name, "generic");
                assert!(git_ref.is_none());
                assert!(
                    url.token.is_none(),
                    "no token may exist on a generic remote"
                );
                assert_eq!(url.authed, url.clean, "authed must equal clean");
                assert_eq!(
                    url.clean, "https://gitlab.freedesktop.org/x/y",
                    "the URL passes through untouched"
                );
            }
            other => panic!("expected a generic Git source, got {other:?}"),
        }

        // contrast arm: the configured forge's OWN host does get the token —
        // scoping means "exactly where it belongs", not "nowhere"
        let gh_spec =
            crate::spec::PkgSpec::parse("https://fake.invalid/acme/widgets.git").unwrap();
        match resolve_source(&cfg, &gh_spec).expect("forge resolution") {
            ResolvedSource::Git { url, .. } => {
                assert_eq!(
                    url.authed,
                    "https://x-access-token:pat-scope-sentinel-xyz@fake.invalid/acme/widgets.git",
                    "the forge's own host receives the token in the authed URL"
                );
                assert_eq!(url.clean, "https://fake.invalid/acme/widgets.git");
            }
            other => panic!("expected a forge Git source, got {other:?}"),
        }
        std::env::remove_var("GITFULL_TEST_SCOPE_PAT");
    }

    /// The gate itself: a non-TTY context may NEVER build a search
    /// fallback match, however highly ranked it is.
    #[test]
    fn unconfirmed_search_never_builds_without_confirmation() {
        let cands = vec![search::Candidate {
            forge: "github".into(),
            owner: "someone".into(),
            repo: "coursework-gee".into(),
            stars: 2.0,
            contributors: Some(1.0),
            commits: Some(3.0),
            last_activity: Some(util::epoch() - 90 * 86400),
            score: 0.12,
        }];
        // non-TTY: hard error naming the dep, the unconfirmed match and
        // the [dep] pin syntax — nothing about "auto-selected"
        let err = confirm_unconfirmed_search_dep("gee-0.8", &cands, false, &|| None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("gee-0.8"), "{msg}");
        assert!(msg.contains("UNCONFIRMED"), "{msg}");
        assert!(msg.contains("[dep."), "{msg}");
        assert!(!msg.contains("auto-selected"), "{msg}");
        // TTY + "n"/EOF: refused
        let refused = confirm_unconfirmed_search_dep("gee-0.8", &cands, true, &|| None).unwrap_err();
        assert!(format!("{refused}").contains("aborted"));
        // TTY + "y": proceeds with the TOP candidate only
        let ok = confirm_unconfirmed_search_dep("gee-0.8", &cands, true, &|| Some("y\n".into()))
            .unwrap();
        assert_eq!(ok.key(), "someone/coursework-gee");
        // even a 796-star match is unconfirmed without consent
        let popular = vec![search::Candidate {
            forge: "github".into(),
            owner: "TryGhost".into(),
            repo: "docker-library-ghost".into(),
            stars: 796.0,
            contributors: Some(40.0),
            commits: Some(500.0),
            last_activity: Some(util::epoch() - 86400),
            score: 0.58,
        }];
        assert!(confirm_unconfirmed_search_dep("cairo", &popular, false, &|| None).is_err());
    }

    /// The interactivity seam that makes the gate hang-proof:
    ///
    /// * `None` (production CLI) auto-detects with the BOTH-ends prompt
    ///   check — stdin AND stdout must be real terminals, never stdin
    ///   alone (an inherited-but-unserviced terminal on fd 0 is exactly
    ///   the makepkg/CI packaging hang);
    /// * `Some(false)` (tests, embedders) forces the non-interactive
    ///   hard-error path deterministically, whatever fds the test runner
    ///   inherited;
    /// * `Some(true)` forces the prompt path.
    #[test]
    fn interactive_sessions_are_decided_by_both_prompt_ends() {
        assert_eq!(
            util::is_interactive(),
            util::is_tty(0) && util::is_tty(1),
            "is_interactive is the classic isatty(0) && isatty(1) prompt \
             check — a single-ended check mistakes an inherited terminal \
             for a human and hangs packaging builds"
        );
        let auto = ExecCtx {
            interactive_override: None,
            ..ExecCtx::default()
        };
        assert_eq!(auto.interactive(), util::is_interactive());
        let forced_off = ExecCtx {
            interactive_override: Some(false),
            ..ExecCtx::default()
        };
        assert!(!forced_off.interactive());
        let forced_on = ExecCtx {
            interactive_override: Some(true),
            ..ExecCtx::default()
        };
        assert!(forced_on.interactive());

        // the shipped gate: a forced non-interactive session hard-errors
        // IMMEDIATELY — the read closure must never even be consulted
        // (this is the deterministic simulation of "no TTY present")
        let cands = vec![search::Candidate {
            forge: "github".into(),
            owner: "someone".into(),
            repo: "coursework-gee".into(),
            stars: 2.0,
            contributors: Some(1.0),
            commits: Some(3.0),
            last_activity: Some(util::epoch() - 90 * 86400),
            score: 0.12,
        }];
        let read_attempted = std::cell::Cell::new(false);
        let err = confirm_unconfirmed_search_dep(
            "gee-0.8",
            &cands,
            forced_off.interactive(),
            &|| {
                read_attempted.set(true);
                Some("y\n".into()) // would BUILD if the gate were wrong
            },
        )
        .unwrap_err();
        assert!(
            !read_attempted.get(),
            "no blocking read may be attempted at all in a non-interactive session"
        );
        assert!(format!("{err}").contains("UNCONFIRMED"), "{err}");
    }

    /// Generic (non-forge) git URLs plan to a plain anonymous fetch —
    /// no forge registration required, owner/repo parsed for the
    /// sandbox name only.
    #[test]
    fn generic_git_urls_parse_owner_repo() {
        assert_eq!(
            parse_git_url_owner_repo("https://gitlab.gnome.org/GNOME/glib"),
            Some(("GNOME".to_string(), "glib".to_string()))
        );
        assert_eq!(
            parse_git_url_owner_repo("https://gitlab.freedesktop.org/cairo/cairo.git"),
            Some(("cairo".to_string(), "cairo".to_string()))
        );
        assert_eq!(
            parse_git_url_owner_repo("https://gitlab.com/group/sub/libtiff.git"),
            Some(("group/sub".to_string(), "libtiff".to_string()))
        );
        assert_eq!(parse_git_url_owner_repo("https://example.com"), None);
    }
}
