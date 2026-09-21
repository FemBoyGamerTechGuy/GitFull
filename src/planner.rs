//! Install pipeline orchestration.
//!
//! Flow of `gitfull install <spec>`:
//!
//! 1. resolve the spec against the forge registry (`[repo]` overrides
//!    can reroute the forge, pin a ref, or add requirements);
//! 2. create the app sandbox; clone with the **live progress UI**;
//! 3. auto-detect the build system (no extra files needed in the repo);
//! 4. walk package dependencies recursively (cycle-safe), cloning and
//!    building each inside *this* sandbox under `deps/`;
//! 5. select **shared** toolchains from `<root>/toolchains/` (missing ones
//!    are reported with the exact bootstrap command — never installed via
//!    a host package manager);
//! 6. build (all commands through the exec chokepoint, hermetic env);
//! 7. stage + collect final binaries;
//! 8. **the single sandbox-escape path** — [`install_binaries`] copies
//!    exactly those binaries into the bin dir, hashed + audited;
//! 9. write `meta.toml` (provenance for `list` / `update` / `remove`).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{GitfullError, Result};
use crate::gitproc::{self, authed_url, ExecClass, ExecCtx, RemoteUrl};
use crate::progress::ProgressUi;
use crate::resolver::{detect_cycle, resolve_repo, ResolvedRepo};
use crate::sandbox::Sandbox;
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
            None => Err(GitfullError::Unsupported(format!(
                "URL `{u}` does not match any configured forge host. Add a \
                 [forge.<name>] entry with that host to gitfull.conf — no code \
                 changes needed (see docs/CONFIG.md)"
            ))),
        },
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
// install
// ---------------------------------------------------------------------------

pub fn install(
    cfg: &Config,
    base_ctx: &ExecCtx,
    spec: &PkgSpec,
    opts: &InstallOpts,
) -> Result<Option<InstallRecord>> {
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

    // Reinstall handling
    if sb.meta_path().exists() {
        if !opts.yes && util::is_tty(0) {
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

    // ---- dependency walk (inside this sandbox) ------------------------------
    struct Dep {
        sandbox: Sandbox,
        resolved: ResolvedRepo,
        url: String,
    }
    let mut deps: Vec<Dep> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::from([key.clone()]);
    let mut chain: Vec<String> = vec![key.clone()];
    let mut queue: VecDeque<(PkgSpec, String)> = resolved
        .packages
        .iter()
        .map(|p| (p.clone(), key.clone()))
        .collect();

    while let Some((dep_spec, parent)) = queue.pop_front() {
        let dep_source = resolve_source(cfg, &dep_spec)?;
        let (dep_forge, dep_owner, dep_repo, dep_name, dep_url) = match &dep_source {
            ResolvedSource::Git {
                forge_name,
                owner,
                repo,
                url,
                ..
            } => (
                forge_name.clone(),
                owner.clone(),
                repo.clone(),
                Sandbox::name_for(forge_name, owner, repo),
                url.clean.clone(),
            ),
            ResolvedSource::Local(p) => (
                "local".to_string(),
                String::new(),
                p.file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default(),
                util::sanitize_component(
                    &p.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| "local".into()),
                ),
                p.display().to_string(),
            ),
        };
        let dep_key = dep_spec.key();
        if visited.contains(&dep_key) {
            continue;
        }
        if let Some(cycle) = detect_cycle(&chain, &dep_key) {
            return Err(GitfullError::Unsupported(format!(
                "dependency cycle: {cycle}"
            )));
        }
        println!("gitfull: dependency {dep_key} (required by {parent})");

        let depsb = Sandbox::for_app(&sb.deps(), &dep_name);
        if depsb.dir.exists() {
            fs::remove_dir_all(&depsb.dir)?;
        }
        depsb.create()?;
        match &dep_source {
            ResolvedSource::Git { url, git_ref, .. } => {
                gitproc::git_clone(
                    &fetch_ctx,
                    url,
                    &depsb.src(),
                    git_ref.as_deref(),
                    &cfg.clone,
                    &cfg.host_tool_path,
                    &depsb.env(),
                    None,
                )?;
            }
            ResolvedSource::Local(p) => {
                copy_tree(p, &depsb.src())?;
            }
        }
        let dep_ov = cfg.repo_override_for(&dep_owner, &dep_repo).cloned();
        let dep_resolved = resolve_repo(&depsb.src(), &dep_key, dep_ov.as_ref())?;
        queue.extend(
            dep_resolved
                .packages
                .iter()
                .map(|p| (p.clone(), dep_key.clone()))
                .collect::<Vec<_>>(),
        );
        visited.insert(dep_key.clone());
        chain.push(dep_key.clone());
        let _ = (dep_forge, dep_repo);
        deps.push(Dep {
            sandbox: depsb,
            resolved: dep_resolved,
            url: dep_url,
        });
    }

    // ---- merged toolchain needs ---------------------------------------------
    let mut needs: BTreeMap<String, crate::resolver::Constraint> = resolved.toolchains.clone();
    for d in &deps {
        for (comp, c) in &d.resolved.toolchains {
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
        let mut lines = Vec::new();
        for (comp, c) in &missing {
            if comp == "gcc" {
                lines.push(format!(
                    "gcc: not installed. Bootstrap the SEED GCC toolchain first:\n  \
                     gitfull toolchain bootstrap-gcc --execute\n  \
                     (uses the host system compiler exactly ONCE; see docs/AUDIT.md)\n  \
                     requirement: gcc{}{}",
                    op_text(c.op),
                    c.version.as_deref().unwrap_or("")
                ));
            } else {
                lines.push(format!(
                    "{comp}: not installed. Build it from source with gitfull:\n  \
                     gitfull toolchain build {comp} --execute\n  \
                     (built with the toolchain-managed compiler — never the host one)",
                ));
            }
        }
        if opts.dry_run {
            println!("gitfull (dry-run): would need to obtain missing toolchains:");
            for l in &lines {
                println!("  {l}");
            }
            println!("gitfull (dry-run): stopping before any build; no changes installed.");
            return Ok(None);
        }
        return Err(GitfullError::Toolchain {
            component: "resolver".into(),
            message: lines.join("\n"),
        });
    }

    if opts.dry_run {
        println!("gitfull (dry-run): plan");
        println!("  sandbox: {}", sb.dir.display());
        println!(
            "  build:   {} ({} steps)",
            resolved.build.label(),
            build_commands(resolved.build, &sb, cfg.jobs).len()
        );
        for d in &deps {
            println!(
                "  dep:     {} [{}] from {}",
                d.resolved.key,
                d.resolved.build.label(),
                d.url
            );
        }
        println!("  install: final binaries -> {}", cfg.bin_dir.display());
        println!("gitfull (dry-run): stopping before any build; no changes installed.");
        return Ok(None);
    }

    // ---- build dependencies, then the app ------------------------------------
    let dep_prefixes: Vec<PathBuf> = deps.iter().map(|d| d.sandbox.prefix()).collect();

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

    for d in &deps {
        println!(
            "gitfull: building dependency {} [{}]",
            d.resolved.key,
            d.resolved.build.label()
        );
        let dep_env = d
            .sandbox
            .build_env(&tc_bins, &tc_libs, &[], &cfg.host_tool_path, &extra);
        let jobs = d.resolved.jobs.unwrap_or(cfg.jobs);
        let steps = build_commands(d.resolved.build, &d.sandbox, jobs);
        let dep_ctx = ExecCtx {
            resolve_path: dep_env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| cfg.host_tool_path.clone()),
            ..base_ctx.clone()
        };
        run_build(&dep_ctx, &d.sandbox, &steps, &dep_env, opts.verbose)?;
        promote_stage(&stage_root(&d.sandbox), &d.sandbox.prefix())?;
        println!("gitfull: dependency {} built and staged", d.resolved.key);
    }

    println!(
        "gitfull: building {} [{}]",
        sandbox_name,
        resolved.build.label()
    );
    let app_env = sb.build_env(
        &tc_bins,
        &tc_libs,
        &dep_prefixes,
        &cfg.host_tool_path,
        &extra,
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
    if resolved.build == crate::manifest::BuildSystem::Cargo {
        // cargo needs a writable target dir + registry under HOME
        let _ = fs::create_dir_all(sb.build().join("target"));
    }
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
        packages: deps.iter().map(|d| d.resolved.key.clone()).collect(),
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
