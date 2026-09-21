//! Toolchain bootstrap execution.
//!
//! This module contains the implementation of the **single host-touching
//! path** in gitfull: [`bootstrap_seed_gcc`] uses the host system compiler
//! exactly once to build the seed GCC toolchain, after which the host
//! compiler is never invoked again (all later builds run with
//! toolchain-managed compilers under `ExecClass::Toolchain`).
//!
//! It also implements building the remaining toolchain components from
//! source ([`build_component`]) — python, meson, ninja, cmake, vala, rust —
//! using the already-bootstrapped GCC, never the host compiler.
//!
//! [`ensure_components`] is the **automatic provisioning** entry point used
//! by `gitfull install`: missing toolchain components (and their own build
//! needs, transitively) are fetched and built silently-in-background — the
//! user never runs a manual bootstrap step on the happy path. The
//! `gitfull toolchain …` subcommands remain as optional manual/advanced
//! overrides.
//!
//! Per the project brief, these execution paths are **not exercised in the
//! development sandbox** (they target a full Linux machine); planning,
//! version auto-detection, ordering, and classification are covered by
//! tests.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{CloneSection, Config};
use crate::error::{GitfullError, Result};
use crate::gitproc::{self, authed_url, ExecClass, ExecCtx};
use crate::resolver::{Constraint, CmpOp};
use crate::toolchain::{
    catalog_entry, resolve_latest_git_version, resolve_latest_rust_version, source_url,
    ComponentSpec, ToolchainManager, CATALOG,
};

#[derive(Serialize)]
struct Provenance {
    component: String,
    version: String,
    source: String,
    built_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    date_epoch: u64,
}

fn write_provenance(dir: &Path, p: &Provenance) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("meta.toml"), toml::to_string(p)?)?;
    Ok(())
}

fn fetch_ctx(cfg: &Config, base: &ExecCtx) -> ExecCtx {
    let mut c = base.clone();
    c.resolve_path = cfg.host_tool_path.clone();
    c
}

/// Build the seed GCC toolchain — THE single host-touching path.
///
/// `version`: explicit pin; otherwise resolved automatically (config pin
/// or **latest** from the GCC git tags — no hardcoded default).
///
/// Without `--execute` this prints the audited plan and exits; with
/// `--execute` it runs the sequence (full Linux machine required).
pub fn bootstrap_seed_gcc(
    cfg: &Config,
    base: &ExecCtx,
    version: Option<&str>,
    execute: bool,
) -> Result<()> {
    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let git_home = cfg.cache_dir.join("git-home");
    let fctx = fetch_ctx(cfg, base);
    let host_tool_path = cfg.host_tool_path.clone();

    // version resolution order: --version flag > config pin > latest tag
    let (version, origin) = match version {
        Some(v) => (v.to_string(), "(pinned via --version)"),
        None => match &cfg.toolchain.seed_gcc_version {
            Some(v) => (v.clone(), "(pinned via toolchain.seed_gcc_version)"),
            None => {
                let label = if cfg.toolchain.preferences.get("gcc").is_some() {
                    "(pinned via toolchain.preferences.gcc)"
                } else {
                    "(auto-detected: latest release tag — no hardcoded default)"
                };
                (
                    mgr.resolve_seed_gcc_version(
                        &fctx,
                        &host_tool_path,
                        &git_home,
                        &Constraint::parse_any(),
                    )?,
                    label,
                )
            }
        },
    };
    seed_gcc_resolved(cfg, base, &version, origin, execute)
}

/// The seed-GCC build with the version already resolved (used by both the
/// manual `toolchain bootstrap-gcc` command and automatic provisioning,
/// which passes its own constraint-derived version + origin label).
fn seed_gcc_resolved(
    cfg: &Config,
    base: &ExecCtx,
    version: &str,
    origin: &str,
    execute: bool,
) -> Result<()> {
    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let git_home = cfg.cache_dir.join("git-home");
    let fctx = fetch_ctx(cfg, base);
    let host_tool_path = cfg.host_tool_path.clone();
    println!("gitfull: seed GCC version: {version} {origin}");

    if mgr.installed_versions("gcc").iter().any(|v| *v == version) {
        println!(
            "gitfull: gcc-{version} is already installed at {} — nothing to do \
             (the host compiler will NOT be used again)",
            mgr.component_dir("gcc", &version).display()
        );
        return Ok(());
    }

    for line in mgr.seed_gcc_plan(&version) {
        println!("{line}");
    }
    if !execute {
        println!(
            "\ngitfull: plan only — pass --execute to run. The build uses the \
             HOST system compiler exactly once (see docs/AUDIT.md) and requires \
             a full Linux machine."
        );
        return Ok(());
    }

    // ---- execute ------------------------------------------------------------
    crate::privilege::require_root(cfg, "toolchain bootstrap-gcc")?;
    cfg.ensure_root()?;
    let source = source_url("gcc", &version, &cfg.toolchain.sources);
    let src = mgr.source_cache_dir("gcc", &version);
    let dest = mgr.component_dir("gcc", &version);
    let build_dir = cfg.toolchains_dir.join(format!(".build/gcc-{version}"));
    let tag = format!("releases/gcc-{version}");

    // 1. fetch (sealed FetchTool) — with the same live progress UI every
    //    clone in gitfull uses (main repos, dependencies, toolchains)
    if src.exists() {
        fs::remove_dir_all(&src)?;
    }
    println!("gitfull: fetching {source} (tag {tag})");
    let url = authed_url(&source, None);
    let mut ui = crate::planner::clone_progress(cfg);
    gitproc::git_clone(
        &fctx,
        &url,
        &src,
        Some(&tag),
        &CloneSection {
            depth: Some(1),
            single_branch: Some(true),
            recurse_submodules: false,
        },
        &host_tool_path,
        &git_home,
        Some(&mut ui),
    )?;
    let commit = gitproc::git_rev_parse_head(&fctx, &src, &host_tool_path, &git_home).ok();
    ui.finish(&format!(
        "gitfull: fetched {source} ({})",
        commit.as_deref().unwrap_or("unknown commit")
    ));

    // 2. prerequisites (GMP/MPFR/MPC) — sealed FetchTool
    println!("gitfull: downloading prerequisites (GMP/MPFR/MPC)");
    let prereq_env = gitproc::git_env(&git_home, &host_tool_path);
    gitproc::run(
        &fctx,
        &["sh".into(), "contrib/download_prerequisites".into()],
        ExecClass::FetchTool,
        &src,
        &prereq_env,
        Some(&cfg.logs_dir.join(format!("gcc-{version}-prereqs.log"))),
    )?;

    // 3-4. configure + make + install — THE host-compiler window
    //    (ExecClass::SeedHostCompiler: the ONLY place this class is used)
    println!(
        "gitfull: building seed GCC with the HOST compiler (single sanctioned \
         host touch; --disable-bootstrap = exactly one stage)"
    );
    fs::create_dir_all(&build_dir)?;
    let host_env: Vec<(String, String)> = vec![
        ("PATH".into(), host_tool_path.clone()),
        ("HOME".into(), git_home.display().to_string()),
        ("LC_ALL".into(), "C".into()),
    ];
    let cc_log = |step: &str| cfg.logs_dir.join(format!("gcc-{version}-{step}.log"));
    let prefix = dest.display().to_string();
    gitproc::run(
        &fctx,
        &[
            format!("{}/configure", src.display()),
            "--disable-bootstrap".into(),
            "--disable-nls".into(),
            "--disable-multilib".into(),
            "--enable-languages=c,c++".into(),
            format!("--prefix={prefix}"),
        ],
        ExecClass::SeedHostCompiler,
        &build_dir,
        &host_env,
        Some(&cc_log("configure")),
    )?;
    gitproc::run(
        &fctx,
        &["make".into(), format!("-j{}", cfg.jobs)],
        ExecClass::SeedHostCompiler,
        &build_dir,
        &host_env,
        Some(&cc_log("build")),
    )?;
    gitproc::run(
        &fctx,
        &["make".into(), "install".into()],
        ExecClass::SeedHostCompiler,
        &build_dir,
        &host_env,
        Some(&cc_log("install")),
    )?;

    // 5. provenance
    write_provenance(
        &dest,
        &Provenance {
            component: "gcc".into(),
            version: version.to_string(),
            source: source.clone(),
            built_by: "host-cc (seed — the single host-touching step)".into(),
            commit,
            date_epoch: crate::util::epoch(),
        },
    )?;
    let _ = fs::remove_dir_all(&build_dir); // keep toolchains/ clean

    println!(
        "gitfull: seed GCC installed at {} — from now on, the host compiler \
         is never invoked again (ExecClass::Toolchain for all builds)",
        dest.display()
    );
    Ok(())
}

/// Build a non-seed toolchain component from source, using the
/// toolchain-managed GCC (never the host compiler).
///
/// * python / cmake / vala — autotools-style: configure && make && install
/// * meson — git checkout + wrapper script (runs `meson.py` with the
///   toolchain python; no compilation)
/// * ninja — `configure.py --bootstrap` (toolchain python + gcc)
/// * rust — official dist tarball via curl (FetchTool) + tar (HostUtility)
///
/// `version`: explicit pin (from `--version` or automatic provisioning);
/// otherwise resolved as `[toolchain.preferences]` pin → **latest stable
/// release tag** (git sources) / current stable channel (rust). Never a
/// hardcoded version constant.
pub fn build_component(
    cfg: &Config,
    base: &ExecCtx,
    component: &str,
    version: Option<&str>,
    execute: bool,
) -> Result<()> {
    let spec = catalog_entry(component).ok_or_else(|| GitfullError::Toolchain {
        component: component.to_string(),
        message: format!(
            "unknown component `{component}` (catalog: {})",
            CATALOG
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })?;

    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let installed = mgr.installed_versions(component);
    println!(
        "gitfull: {component}: installed versions: {}",
        if installed.is_empty() {
            "(none)".to_string()
        } else {
            installed.join(", ")
        }
    );

    // The seed must exist first (everything except rust builds with gcc).
    let gcc = mgr.find("gcc", &crate::resolver::Constraint::parse_any());
    if spec.needs.contains(&"gcc") && gcc.is_none() {
        return Err(GitfullError::Toolchain {
            component: component.to_string(),
            message: "requires the gcc toolchain — run `gitfull install <pkg>` \
                      (it auto-bootstraps the seed) or `gitfull toolchain \
                      bootstrap-gcc --execute` first"
                .into(),
        });
    }

    let version = match version {
        Some(v) => v.to_string(),
        None => resolve_component_version(cfg, base, component, &Constraint::parse_any())?,
    };

    let plan = component_plan(cfg, component, spec.source, &version);
    for line in &plan {
        println!("{line}");
    }
    if !execute {
        println!("\ngitfull: plan only — pass --execute to run (full Linux machine required).");
        return Ok(());
    }
    crate::privilege::require_root(cfg, "toolchain build")?;
    cfg.ensure_root()?;

    let git_home = cfg.cache_dir.join("git-home");
    let fctx = fetch_ctx(cfg, base);
    let host_tool_path = cfg.host_tool_path.clone();
    // toolchain build env: toolchain gcc first, then host utilities
    let gcc = mgr.find("gcc", &crate::resolver::Constraint::parse_any());
    let tc_bins: Vec<PathBuf> = gcc.iter().map(|g| g.bin.clone()).collect();
    let sb = SandboxForToolchain::new(cfg, component);
    sb.create()?;
    let mut tc_env = sb.build_env(&tc_bins, &[], &[], &host_tool_path, &[]);
    if let Some(g) = &gcc {
        tc_env.push(("CC".into(), g.bin.join("gcc").display().to_string()));
        tc_env.push(("CXX".into(), g.bin.join("g++").display().to_string()));
    }

    let source = source_url(component, &version, &cfg.toolchain.sources);
    let dest = mgr.component_dir(component, &version);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }

    if spec.source_is_git {
        // fetch via git (sealed) — live progress UI like every clone
        let src = mgr.source_cache_dir(component, &version);
        if src.exists() {
            fs::remove_dir_all(&src)?;
        }
        println!("gitfull: fetching {source}");
        let url = authed_url(&source, None);
        let mut ui = crate::planner::clone_progress(cfg);
        gitproc::git_clone(
            &fctx,
            &url,
            &src,
            None,
            &CloneSection {
                depth: None,
                single_branch: Some(true),
                recurse_submodules: false,
            },
            &host_tool_path,
            &git_home,
            Some(&mut ui),
        )?;
        let commit = gitproc::git_rev_parse_head(&fctx, &src, &host_tool_path, &git_home).ok();
        ui.finish(&format!(
            "gitfull: fetched {source} ({})",
            commit.as_deref().unwrap_or("unknown commit")
        ));

        match component {
            "meson" => {
                // wrapper script: no compilation, just python + checkout
                let py = mgr
                    .find("python", &crate::resolver::Constraint::parse_any())
                    .map(|p| p.bin.join("python3"))
                    .ok_or_else(|| GitfullError::Toolchain {
                        component: "meson".into(),
                        message: "requires the python toolchain — run \
                                      `gitfull toolchain build python --execute` first"
                            .into(),
                    })?;
                let bin = dest.join("bin");
                fs::create_dir_all(&bin)?;
                let wrapper = format!(
                    "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
                    py.display(),
                    src.join("meson.py").display()
                );
                fs::write(bin.join("meson"), wrapper)?;
                make_executable(&bin.join("meson"))?;
            }
            "ninja" => {
                let py = mgr
                    .find("python", &crate::resolver::Constraint::parse_any())
                    .map(|p| p.bin.join("python3"))
                    .ok_or_else(|| GitfullError::Toolchain {
                        component: "ninja".into(),
                        message: "requires the python toolchain".into(),
                    })?;
                gitproc::run(
                    &fctx,
                    &[
                        py.display().to_string(),
                        "configure.py".into(),
                        "--bootstrap".into(),
                    ],
                    ExecClass::Toolchain,
                    &src,
                    &tc_env,
                    Some(&cfg.logs_dir.join("ninja-bootstrap.log")),
                )?;
                let bin = dest.join("bin");
                fs::create_dir_all(&bin)?;
                fs::copy(src.join("ninja"), bin.join("ninja"))?;
                make_executable(&bin.join("ninja"))?;
            }
            _ => {
                // autotools-style: python / cmake / vala
                let build_dir = sb.dir.join("build");
                fs::create_dir_all(&build_dir)?;
                let log = |s: &str| cfg.logs_dir.join(format!("{component}-{s}.log"));
                gitproc::run(
                    &fctx,
                    &[
                        format!("{}/configure", src.display()),
                        format!("--prefix={}", dest.display()),
                        "--disable-nls".into(),
                    ],
                    ExecClass::Toolchain,
                    &build_dir,
                    &tc_env,
                    Some(&log("configure")),
                )?;
                gitproc::run(
                    &fctx,
                    &["make".into(), format!("-j{}", cfg.jobs)],
                    ExecClass::Toolchain,
                    &build_dir,
                    &tc_env,
                    Some(&log("build")),
                )?;
                gitproc::run(
                    &fctx,
                    &["make".into(), "install".into()],
                    ExecClass::Toolchain,
                    &build_dir,
                    &tc_env,
                    Some(&log("install")),
                )?;
            }
        }
        write_provenance(
            &dest,
            &Provenance {
                component: component.to_string(),
                version: version.clone(),
                source: source.clone(),
                built_by: "gitfull toolchain (toolchain-managed gcc)".into(),
                commit,
                date_epoch: crate::util::epoch(),
            },
        )?;
    } else {
        // tarball source (rust): curl + tar
        let tarball = cfg.cache_dir.join(format!("{component}-{version}.tar.xz"));
        println!("gitfull: downloading {source}");
        gitproc::curl_download(&fctx, &source, &tarball, &host_tool_path)?;
        gitproc::run(
            &fctx,
            &[
                "tar".into(),
                "-xJf".into(),
                tarball.display().to_string(),
                "-C".into(),
                cfg.toolchains_dir.display().to_string(),
            ],
            ExecClass::HostUtility,
            Path::new("."),
            &[
                ("PATH".into(), host_tool_path.clone()),
                ("LC_ALL".into(), "C".into()),
            ],
            None,
        )?;
        // The dist tarball extracts as `rust-<version>-<triple>`; relocate
        // it to the plain versioned component dir the manager scans for
        // (`<toolchains>/rust-<version>`), whatever the source named it.
        if let Some(stem) = Path::new(&source)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.trim_end_matches(".tar.xz").trim_end_matches(".tar.gz"))
        {
            let extracted = cfg.toolchains_dir.join(stem);
            if extracted.is_dir() && extracted != dest {
                if dest.exists() {
                    fs::remove_dir_all(&dest)?;
                }
                fs::rename(&extracted, &dest)?;
            }
        }
        write_provenance(
            &dest,
            &Provenance {
                component: component.to_string(),
                version: version.clone(),
                source: source.clone(),
                built_by: "gitfull toolchain (official dist tarball)".into(),
                commit: None,
                date_epoch: crate::util::epoch(),
            },
        )?;
    }

    println!(
        "gitfull: {component}-{version} installed at {}",
        dest.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Automatic provisioning (the install happy path)
// ---------------------------------------------------------------------------

/// Build order for a set of missing components: the transitive closure of
/// their catalog `needs`, topologically sorted (dependencies first; the
/// gcc seed always comes first because everything non-rust needs it).
/// Pure function — unit-tested without any network or filesystem.
pub fn provision_order(missing: &[String]) -> Result<Vec<&'static ComponentSpec>> {
    // closure
    let mut want: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<String> = missing.to_vec();
    while let Some(name) = queue.pop() {
        let spec = catalog_entry(&name).ok_or_else(|| GitfullError::Toolchain {
            component: name.clone(),
            message: format!(
                "unknown component `{name}` (catalog: {}). gitfull can only \
                 auto-provision catalog components; custom toolchains must be \
                 provided manually under <root>/toolchains/",
                CATALOG.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
            ),
        })?;
        if want.insert(spec.name) {
            queue.extend(spec.needs.iter().map(|s| s.to_string()));
        }
    }
    // topological sort, CATALOG order as the deterministic tie-break
    let mut out: Vec<&'static ComponentSpec> = Vec::new();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    loop {
        let mut progressed = false;
        for spec in CATALOG {
            if !want.contains(spec.name) || done.contains(spec.name) {
                continue;
            }
            let ready = spec.needs.iter().all(|n| done.contains(n));
            if ready {
                out.push(spec);
                done.insert(spec.name);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    if out.len() != want.len() {
        return Err(GitfullError::Toolchain {
            component: "provision-order".into(),
            message: "circular `needs` in the toolchain catalog".into(),
        });
    }
    Ok(out)
}

/// Resolve the version to build for `component` under `constraint`:
/// explicit `[toolchain.preferences]` pin (if it satisfies) → latest
/// release tag (git sources) / current stable channel (rust tarballs).
fn resolve_component_version(
    cfg: &Config,
    base: &ExecCtx,
    component: &str,
    constraint: &Constraint,
) -> Result<String> {
    let spec = catalog_entry(component)
        .ok_or_else(|| GitfullError::Toolchain {
            component: component.into(),
            message: "unknown component".into(),
        })?;
    if let Some(pin) = cfg.toolchain.preferences.get(component) {
        if constraint.satisfies(pin) {
            return Ok(pin.clone());
        }
    }
    let fctx = fetch_ctx(cfg, base);
    let git_home = cfg.cache_dir.join("git-home");
    if spec.source_is_git {
        let source = source_url(component, "latest", &cfg.toolchain.sources);
        resolve_latest_git_version(
            &fctx,
            &source,
            constraint,
            &cfg.host_tool_path,
            &git_home,
        )
    } else {
        let v = resolve_latest_rust_version(&fctx, &cfg.host_tool_path)?;
        if constraint.satisfies(&v) {
            Ok(v)
        } else {
            Err(GitfullError::Toolchain {
                component: component.into(),
                message: format!(
                    "the current rust stable channel is {v}, which does not \
                     satisfy the requirement; pin the version you need with \
                     [toolchain.preferences] rust = \"...\" in gitfull.conf"
                ),
            })
        }
    }
}

/// Resolve the gcc version honoring pins AND the install-time constraint
/// (a pin that conflicts with the constraint is a hard error, not a
/// silent override).
fn resolve_gcc_version(
    cfg: &Config,
    base: &ExecCtx,
    mgr: &ToolchainManager,
    constraint: &Constraint,
) -> Result<String> {
    let pin = cfg
        .toolchain
        .seed_gcc_version
        .as_deref()
        .or_else(|| cfg.toolchain.preferences.get("gcc").map(|s| s.as_str()));
    if let Some(pin) = pin {
        if constraint.satisfies(pin) {
            return Ok(pin.to_string());
        }
        return Err(GitfullError::Toolchain {
            component: "gcc".into(),
            message: format!(
                "pinned gcc {pin} does not satisfy the requirement (gcc {}{}); \
                 adjust toolchain.seed_gcc_version / [toolchain.preferences] \
                 or the repo's [repo] toolchains constraint",
                constraint_text(constraint),
                constraint.version.as_deref().unwrap_or("")
            ),
        });
    }
    let fctx = fetch_ctx(cfg, base);
    let git_home = cfg.cache_dir.join("git-home");
    mgr.resolve_seed_gcc_version(&fctx, &cfg.host_tool_path, &git_home, constraint)
}

/// **Automatic toolchain provisioning** — the heart of the corrected
/// install flow. Given the components an install found missing (with their
/// constraints), gitfull:
///
/// 1. computes the transitive build closure and the correct build order
///    (seed gcc first — it is the only step that ever touches the host
///    compiler, exactly once);
/// 2. skips anything already installed that satisfies its constraint;
/// 3. resolves a version for each remaining component (pin → latest
///    release tag, filtered by the constraint);
/// 4. fetches and builds each one with toolchain-managed tools.
///
/// `dry_run` prints the provisioning plan without touching the network or
/// the filesystem. The user never runs `gitfull toolchain build ...`
/// manually on the normal path — those subcommands are overrides.
pub fn ensure_components(
    cfg: &Config,
    base: &ExecCtx,
    missing: &[(String, Constraint)],
    dry_run: bool,
) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let names: Vec<String> = missing.iter().map(|(c, _)| c.clone()).collect();
    let order = provision_order(&names)?;

    // what actually still needs building (constraint-aware)?
    let constraints: BTreeMap<String, Constraint> = missing
        .iter()
        .map(|(c, k)| (c.clone(), k.clone()))
        .collect();
    let mut to_build: Vec<(&'static ComponentSpec, Constraint)> = Vec::new();
    for spec in &order {
        let constraint = constraints
            .get(spec.name)
            .cloned()
            .unwrap_or_else(Constraint::parse_any);
        if mgr.find(spec.name, &constraint).is_some() {
            continue; // already installed (or built earlier in this pass)
        }
        to_build.push((spec, constraint));
    }

    if to_build.is_empty() {
        // everything appeared between selection and now — fine
        return Ok(());
    }

    if dry_run {
        println!("gitfull (dry-run): would auto-provision missing toolchains:");
        for (spec, constraint) in &to_build {
            let role = if spec.name == "gcc" {
                "SEED — uses the host compiler exactly ONCE (--disable-bootstrap)"
            } else if spec.source_is_git {
                "built with toolchain-managed tools"
            } else {
                "official dist tarball"
            };
            let need = match constraint.op {
                CmpOp::Any => "".to_string(),
                _ => format!(
                    "{}{}",
                    op_text(constraint.op),
                    constraint.version.as_deref().unwrap_or("")
                ),
            };
            println!(
                "  {:<8} {:<10} ({role}; source: {})",
                spec.name, need, spec.source
            );
        }
        return Ok(());
    }

    println!(
        "gitfull: auto-provisioning {} missing toolchain component(s) — the \
         seed GCC build is the ONLY step that uses the host compiler (once); \
         everything else is built with toolchain-managed tools:",
        to_build.len()
    );
    crate::privilege::require_root(cfg, "install (toolchain auto-provision)")?;
    cfg.ensure_root()?;

    for (spec, constraint) in &to_build {
        // re-check: an earlier step may have installed it as a side effect
        if mgr.find(spec.name, constraint).is_some() {
            println!(
                "gitfull: {name} satisfied by an earlier step — skipping",
                name = spec.name
            );
            continue;
        }
        println!();
        if spec.name == "gcc" {
            let version = resolve_gcc_version(cfg, base, &mgr, constraint)?;
            seed_gcc_resolved(
                cfg,
                base,
                &version,
                "(auto-selected during install: constraint + latest release tag)",
                true,
            )?;
        } else {
            let version = resolve_component_version(cfg, base, spec.name, constraint)?;
            build_component(cfg, base, spec.name, Some(&version), true)?;
        }
    }
    Ok(())
}

fn op_text(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Any => "",
        CmpOp::Eq => "=",
        CmpOp::Gt => ">",
        CmpOp::Gte => ">=",
        CmpOp::Lt => "<",
        CmpOp::Lte => "<=",
    }
}

fn constraint_text(c: &Constraint) -> String {
    match c.op {
        CmpOp::Any => "any".to_string(),
        _ => op_text(c.op).to_string(),
    }
}

fn component_plan(cfg: &Config, component: &str, source: &str, version: &str) -> Vec<String> {
    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let dest = mgr.component_dir(component, version);
    vec![
        format!("1. source     : {source} (version {version}, overridable via toolchain.sources)"),
        format!("2. toolchain  : built with the toolchain-managed gcc — never the host compiler"),
        format!("3. install to : {}", dest.display()),
        format!("4. provenance : {}", dest.join("meta.toml").display()),
    ]
}

fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(p)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(p, perms)?;
    Ok(())
}

/// Minimal sandbox for toolchain builds (build env only).
struct SandboxForToolchain {
    dir: PathBuf,
}

impl SandboxForToolchain {
    fn new(cfg: &Config, component: &str) -> Self {
        SandboxForToolchain {
            dir: cfg.toolchains_dir.join(format!(".build-{component}")),
        }
    }
    fn create(&self) -> Result<()> {
        for d in ["env", "tmp", "build"] {
            fs::create_dir_all(self.dir.join(d))?;
        }
        Ok(())
    }
    fn build_env(
        &self,
        tc_bins: &[PathBuf],
        _a: &[PathBuf],
        _b: &[PathBuf],
        host_tool_path: &str,
        _c: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut path: Vec<String> = tc_bins.iter().map(|b| b.display().to_string()).collect();
        path.push(host_tool_path.to_string());
        vec![
            ("PATH".into(), crate::util::dedup_path(&path)),
            ("HOME".into(), self.dir.join("env").display().to_string()),
            ("TMPDIR".into(), self.dir.join("tmp").display().to_string()),
            ("LC_ALL".into(), "C".into()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<'a>(order: &[&'a ComponentSpec]) -> Vec<&'a str> {
        order.iter().map(|s| s.name).collect()
    }

    #[test]
    fn order_is_topological() {
        // meson needs python, python needs gcc; ninja needs python + gcc
        let order =
            provision_order(&["meson".to_string(), "ninja".to_string()]).unwrap();
        let n = names(&order);
        assert_eq!(n, vec!["gcc", "python", "meson", "ninja"]);
        // gcc must precede everything that needs it
        let pos = |x: &str| n.iter().position(|c| *c == x).unwrap();
        assert!(pos("gcc") < pos("python"));
        assert!(pos("python") < pos("meson"));
        assert!(pos("gcc") < pos("ninja"));
    }

    #[test]
    fn seed_comes_first_for_bare_gcc() {
        let order = provision_order(&["gcc".to_string()]).unwrap();
        assert_eq!(names(&order), vec!["gcc"]);
    }

    #[test]
    fn rust_is_independent() {
        let order = provision_order(&["rust".to_string()]).unwrap();
        assert_eq!(names(&order), vec!["rust"]);
    }

    #[test]
    fn unknown_component_is_a_clear_error() {
        let err = provision_order(&["clang".to_string()]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("clang"), "{msg}");
        assert!(msg.contains("unknown component"), "{msg}");
    }

    #[test]
    fn duplicates_are_fine() {
        let order = provision_order(&["python".to_string(), "python".to_string()]).unwrap();
        assert_eq!(names(&order), vec!["gcc", "python"]);
    }
}
