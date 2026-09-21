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
//! Per the project brief, these execution paths are **not exercised in the
//! development sandbox** (they target a full Linux machine); planning,
//! version auto-detection, and classification are covered by tests.

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{CloneSection, Config};
use crate::error::{GitfullError, Result};
use crate::gitproc::{self, authed_url, ExecClass, ExecCtx};
use crate::toolchain::{catalog_entry, source_url, ToolchainManager, CATALOG};

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
                    mgr.resolve_seed_gcc_version(&fctx, &host_tool_path, &git_home)?,
                    label,
                )
            }
        },
    };
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
    cfg.ensure_root()?;
    let source = source_url("gcc", &version, &cfg.toolchain.sources);
    let src = mgr.source_cache_dir("gcc", &version);
    let dest = mgr.component_dir("gcc", &version);
    let build_dir = cfg.toolchains_dir.join(format!(".build/gcc-{version}"));
    let tag = format!("releases/gcc-{version}");

    // 1. fetch (sealed FetchTool)
    if src.exists() {
        fs::remove_dir_all(&src)?;
    }
    println!("gitfull: fetching {source} (tag {tag})");
    let url = authed_url(&source, None);
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
        None,
    )?;
    let commit = gitproc::git_rev_parse_head(&fctx, &src, &host_tool_path, &git_home).ok();

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
            version: version.clone(),
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
pub fn build_component(cfg: &Config, base: &ExecCtx, component: &str, execute: bool) -> Result<()> {
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
            message: "requires the gcc toolchain — run `gitfull toolchain \
                      bootstrap-gcc --execute` first"
                .into(),
        });
    }

    let plan = component_plan(cfg, component, spec.source);
    for line in &plan {
        println!("{line}");
    }
    if !execute {
        println!("\ngitfull: plan only — pass --execute to run (full Linux machine required).");
        return Ok(());
    }
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

    let version = cfg
        .toolchain
        .preferences
        .get(component)
        .cloned()
        .unwrap_or_else(|| "latest".to_string());
    let source = source_url(component, &version, &cfg.toolchain.sources);
    let dest = mgr.component_dir(component, &version);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }

    if spec.source_is_git {
        // fetch via git (sealed)
        let src = mgr.source_cache_dir(component, &version);
        if src.exists() {
            fs::remove_dir_all(&src)?;
        }
        println!("gitfull: fetching {source}");
        let url = authed_url(&source, None);
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
            None,
        )?;
        let commit = gitproc::git_rev_parse_head(&fctx, &src, &host_tool_path, &git_home).ok();

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

fn component_plan(cfg: &Config, component: &str, source: &str) -> Vec<String> {
    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let version = cfg
        .toolchain
        .preferences
        .get(component)
        .cloned()
        .unwrap_or_else(|| "latest".to_string());
    let dest = mgr.component_dir(component, &version);
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
