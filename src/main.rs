//! gitfull CLI — verb commands.
//!
//! ```text
//! gitfull [global options] <command> [command args]
//!
//! commands:
//!   install <spec>...     install packages (`owner/repo`, `forge:o/r`,
//!                          `o/r@ref`, URL, or local path)
//!   update [name...]      re-clone + rebuild installed packages
//!   remove <name>...      remove installed packages (verifies hashes)
//!   list                  list installed packages
//!   info <query>          show details for a package
//!   toolchain ...         list | bootstrap-gcc | build
//!   doctor                environment checks
//!   config ...            show | validate | path
//!   audit [N]             show the last N audit events
//!
//! global options:
//!   --config <path>   config file (default /etc/gitfull.conf)
//!   --root <path>     override sandbox root
//!   --dry-run         plan only — no build, no install
//!   --yes, -y         assume yes (overwrite binaries, force removal)
//!   --no-color        disable ANSI colors
//!   --color <when>    auto | always | never
//!   --verbose, -v     echo build commands
//!   --version, -V     print version
//!   --help, -h        this help
//! ```
//!
//! Exit codes: `0` ok · `2` usage · `3` policy violation · `4` build/exec
//! failure · `5` not installed.

use std::path::PathBuf;
use std::process::ExitCode;

use gitfull::bootstrap;
use gitfull::config::{Config, DEFAULT_CONFIG_PATH};
use gitfull::error::{GitfullError, Result};
use gitfull::gitproc::{ExecClass, ExecCtx};
use gitfull::planner::{self, InstallOpts};
use gitfull::spec::PkgSpec;
use gitfull::toolchain::{catalog_entry, ToolchainManager, CATALOG};
use gitfull::{util, VERSION};

struct Globals {
    config: Option<PathBuf>,
    root: Option<PathBuf>,
    dry_run: bool,
    yes: bool,
    no_color: bool,
    color_always: bool,
    verbose: bool,
}

fn usage() -> String {
    format!(
        "gitfull {VERSION} — forge-agnostic package manager for Git-hosted repositories

usage: gitfull [options] <command> [args]

commands:
  install <spec>...    install: owner/repo | forge:owner/repo | o/r@ref | URL | /local/path
  update [name...]     re-clone and rebuild installed packages
  remove <name>...     remove installed packages (hash-verified)
  list                 list installed packages
  info <query>         show install record or forge resolution
  toolchain list           catalog + installed versions
  toolchain bootstrap-gcc [--execute] [--version <v>]
                         the single host-touching step (seed GCC; see docs/AUDIT.md)
  toolchain build <comp> [--execute]
                         build a component from source with the toolchain gcc
  doctor               environment checks
  config show|validate|path
  audit [N]            tail the audit log (default 20)

options:
  --config <path>      config file (default {DEFAULT_CONFIG_PATH})
  --root <path>        override sandbox root
  --dry-run            resolve and plan only
  --yes, -y            assume yes
  --no-color           disable colors
  --color <when>       auto | always | never
  --verbose, -v        echo build commands
  --version, -V        print version
  --help, -h           this help

exit codes: 0 ok, 2 usage, 3 policy violation, 4 build failure, 5 not installed"
    )
}

/// Parse global options. Global flags must PRECEDE the command; flags
/// after the command belong to that command (e.g.
/// `gitfull toolchain bootstrap-gcc --version 14.2.0`).
fn parse_globals(args: &[String]) -> Result<(Globals, String, Vec<String>)> {
    let mut g = Globals {
        config: None,
        root: None,
        dry_run: false,
        yes: false,
        no_color: false,
        color_always: false,
        verbose: false,
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') || a == "-" {
            break; // first positional = command; the rest belongs to it
        }
        let value_of = |i: &mut usize, flag: &str| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| GitfullError::Usage(format!("{flag} requires a value")))
        };
        match a.as_str() {
            "--config" => g.config = Some(PathBuf::from(value_of(&mut i, a)?)),
            "--root" => g.root = Some(PathBuf::from(value_of(&mut i, a)?)),
            "--dry-run" => g.dry_run = true,
            "--yes" | "-y" => g.yes = true,
            "--no-color" => g.no_color = true,
            "--color" => {
                let v = value_of(&mut i, a)?;
                match v.as_str() {
                    "always" => g.color_always = true,
                    "never" => g.no_color = true,
                    "auto" => {}
                    other => {
                        return Err(GitfullError::Usage(format!(
                            "--color: expected auto|always|never, got `{other}`"
                        )))
                    }
                }
            }
            "--verbose" | "-v" => g.verbose = true,
            "--version" | "-V" => {
                println!("gitfull {VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => {
                return Err(GitfullError::Usage(format!(
                    "unknown option `{other}`\n\n{}",
                    usage()
                )))
            }
        }
        i += 1;
    }
    let cmd = args
        .get(i)
        .cloned()
        .ok_or_else(|| GitfullError::Usage(format!("missing command\n\n{}", usage())))?;
    Ok((g, cmd, args[i + 1..].to_vec()))
}

/// Per-command flags shared by install/update/remove (may appear after the
/// command, interleaved with positionals).
fn parse_op_flags(rest: &[String], cmd: &str) -> Result<(bool, bool, bool, Vec<String>)> {
    // returns (yes, dry_run, verbose, positionals)
    let mut yes = false;
    let mut dry = false;
    let mut verbose = false;
    let mut pos = Vec::new();
    for a in rest {
        match a.as_str() {
            "--yes" | "-y" => yes = true,
            "--dry-run" => dry = true,
            "--verbose" | "-v" => verbose = true,
            other if other.starts_with('-') => {
                return Err(GitfullError::Usage(format!(
                    "unknown option `{other}` for `{cmd}` (global options go \
                     before the command)"
                )))
            }
            other => pos.push(other.to_string()),
        }
    }
    Ok((yes, dry, verbose, pos))
}

fn exit_code(e: &GitfullError) -> ExitCode {
    match e {
        GitfullError::Usage(_) => ExitCode::from(2),
        GitfullError::Policy { .. } => ExitCode::from(3),
        GitfullError::Exec { .. } | GitfullError::Sandbox(_) | GitfullError::Toolchain { .. } => {
            ExitCode::from(4)
        }
        GitfullError::NotInstalled(_) => ExitCode::from(5),
        _ => ExitCode::from(1),
    }
}

fn load_config(g: &Globals) -> Result<(Config, Vec<String>)> {
    let path = g
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let (mut cfg, mut warnings) = Config::load(&path)?;
    if let Some(root) = &g.root {
        cfg.root = root.clone();
        cfg.apps_dir = root.join("apps");
        cfg.toolchains_dir = root.join("toolchains");
        cfg.cache_dir = root.join("cache");
        cfg.logs_dir = root.join("logs");
    }
    if g.no_color {
        cfg.color = gitfull::config::ColorChoice::Never;
    }
    if g.color_always {
        cfg.color = gitfull::config::ColorChoice::Always;
    }
    for w in &warnings {
        eprintln!("gitfull: warning: {w}");
    }
    // (--root/--config overrides may invalidate cached warnings)
    warnings.clear();
    Ok((cfg, warnings))
}

fn base_ctx(cfg: &Config, mutating: bool) -> ExecCtx {
    ExecCtx {
        audit_log: if mutating {
            Some(cfg.root.join("audit.log"))
        } else {
            None
        },
        extra_forbidden: cfg.policy.extra_forbidden_programs.clone(),
        redactions: Vec::new(),
        resolve_path: cfg.host_tool_path.clone(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (globals, cmd, rest) = match parse_globals(&args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("gitfull: {e}");
            return ExitCode::from(2);
        }
    };
    match execute(&globals, &cmd, &rest) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gitfull: {e}");
            exit_code(&e)
        }
    }
}

fn execute(g: &Globals, cmd: &str, rest: &[String]) -> Result<ExitCode> {
    match cmd {
        "install" => cmd_install(g, rest),
        "update" => cmd_update(g, rest),
        "remove" => cmd_remove(g, rest),
        "list" => cmd_list(g),
        "info" => cmd_info(g, rest),
        "toolchain" => cmd_toolchain(g, rest),
        "doctor" => cmd_doctor(g),
        "config" => cmd_config(g, rest),
        "audit" => cmd_audit(g, rest),
        other => Err(GitfullError::Usage(format!(
            "unknown command `{other}`\n\n{}",
            usage()
        ))),
    }
}

fn cmd_install(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let (c_yes, c_dry, c_verbose, specs) = parse_op_flags(rest, "install")?;
    if specs.is_empty() {
        return Err(GitfullError::Usage(
            "install requires at least one package spec".into(),
        ));
    }
    let (cfg, _) = load_config(g)?;
    let ctx = base_ctx(&cfg, true);
    let opts = InstallOpts {
        dry_run: g.dry_run || c_dry,
        yes: g.yes || c_yes,
        verbose: g.verbose || c_verbose,
    };
    let mut failures = 0;
    for spec_s in &specs {
        let spec = match PkgSpec::parse(spec_s) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gitfull: {e}");
                failures += 1;
                continue;
            }
        };
        match planner::install(&cfg, &ctx, &spec, &opts) {
            Ok(Some(rec)) => {
                println!(
                    "gitfull: {} installed ({} binaries)",
                    rec.name,
                    rec.bins.len()
                );
            }
            Ok(None) => {} // dry-run / user aborted
            Err(e) => {
                eprintln!("gitfull: {e}");
                failures += 1;
            }
        }
    }
    Ok(if failures > 0 {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_update(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let (c_yes, c_dry, c_verbose, names) = parse_op_flags(rest, "update")?;
    let (cfg, _) = load_config(g)?;
    let ctx = base_ctx(&cfg, true);
    let opts = InstallOpts {
        dry_run: g.dry_run || c_dry,
        yes: g.yes || c_yes,
        verbose: g.verbose || c_verbose,
    };
    let names: Vec<String> = if names.is_empty() {
        planner::list(&cfg)?.into_iter().map(|r| r.name).collect()
    } else {
        names
    };
    if names.is_empty() {
        println!("gitfull: nothing installed");
        return Ok(ExitCode::SUCCESS);
    }
    let mut failures = 0;
    for n in &names {
        println!("gitfull: updating {n}");
        if let Err(e) = planner::update(&cfg, &ctx, n, &opts) {
            eprintln!("gitfull: {e}");
            failures += 1;
        }
    }
    Ok(if failures > 0 {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_remove(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let (c_yes, _dry, _verbose, names) = parse_op_flags(rest, "remove")?;
    if names.is_empty() {
        return Err(GitfullError::Usage(
            "remove requires at least one package name".into(),
        ));
    }
    let (cfg, _) = load_config(g)?;
    let ctx = base_ctx(&cfg, true);
    let yes = g.yes || c_yes;
    let mut failures = 0;
    for n in &names {
        if let Err(e) = planner::remove(&cfg, &ctx, n, yes) {
            eprintln!("gitfull: {e}");
            failures += 1;
        }
    }
    Ok(if failures > 0 {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_list(_g: &Globals) -> Result<ExitCode> {
    let (cfg, _) = load_config(_g)?;
    let records = planner::list(&cfg)?;
    if records.is_empty() {
        println!(
            "gitfull: no packages installed under {}",
            cfg.apps_dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<40} {:<10} {:<10} {}",
        "NAME", "FORGE", "BUILD", "BINARIES"
    );
    for r in &records {
        println!(
            "{:<40} {:<10} {:<10} {}",
            r.name,
            r.forge,
            r.build_system,
            r.bins
                .iter()
                .map(|b| b.name.clone())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_info(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let q = rest
        .first()
        .ok_or_else(|| GitfullError::Usage("info requires a package name or spec".into()))?;
    let (cfg, _) = load_config(g)?;
    planner::info(&cfg, q)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_toolchain(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let (cfg, _) = load_config(g)?;
    let sub = rest.first().map(|s| s.as_str()).ok_or_else(|| {
        GitfullError::Usage("toolchain requires a subcommand (list|bootstrap-gcc|build)".into())
    })?;
    match sub {
        "list" => {
            let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
            println!("{:<10} {:<12} {}", "COMPONENT", "INSTALLED", "SOURCE");
            for c in CATALOG {
                let versions = mgr.installed_versions(c.name);
                println!(
                    "{:<10} {:<12} {}",
                    c.name,
                    if versions.is_empty() {
                        "-".to_string()
                    } else {
                        versions.join(",")
                    },
                    c.source
                );
            }
            println!(
                "\nseed gcc version: {}",
                cfg.toolchain
                    .seed_gcc_version
                    .as_deref()
                    .unwrap_or("(not pinned — latest is auto-detected at bootstrap)")
            );
            Ok(ExitCode::SUCCESS)
        }
        "bootstrap-gcc" => {
            let mut execute = false;
            let mut version: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--execute" => execute = true,
                    "--version" => {
                        i += 1;
                        version = rest.get(i).cloned().map(Some).ok_or_else(|| {
                            GitfullError::Usage("--version requires a value".into())
                        })?;
                    }
                    other => {
                        return Err(GitfullError::Usage(format!(
                            "unknown option `{other}` for toolchain bootstrap-gcc"
                        )))
                    }
                }
                i += 1;
            }
            // version resolution (CLI flag > config pin > latest tag)
            // happens inside bootstrap::bootstrap_seed_gcc
            let ctx = base_ctx(&cfg, execute);
            bootstrap::bootstrap_seed_gcc(&cfg, &ctx, version.as_deref(), execute)?;
            Ok(ExitCode::SUCCESS)
        }
        "build" => {
            let comp = rest.get(1).ok_or_else(|| {
                GitfullError::Usage("toolchain build requires a component name".into())
            })?;
            catalog_entry(comp).ok_or_else(|| {
                GitfullError::Usage(format!(
                    "unknown component `{comp}` (catalog: {})",
                    CATALOG
                        .iter()
                        .map(|c| c.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let execute = rest.iter().any(|a| a == "--execute");
            let ctx = base_ctx(&cfg, execute);
            bootstrap::build_component(&cfg, &ctx, comp, execute)?;
            Ok(ExitCode::SUCCESS)
        }
        other => Err(GitfullError::Usage(format!(
            "unknown toolchain subcommand `{other}` (list|bootstrap-gcc|build)"
        ))),
    }
}

fn cmd_doctor(g: &Globals) -> Result<ExitCode> {
    let (cfg, warnings) = load_config(g)?;
    let mut ok = true;
    let check = |cond: bool, label: &str, detail: &str| {
        println!(
            "  [{}] {:<28} {}",
            if cond { "ok" } else { "!!" },
            label,
            detail
        );
    };

    println!("gitfull {VERSION} doctor");
    for w in &warnings {
        println!("  warning: {w}");
    }
    match &cfg.config_path {
        Some(p) => check(true, "config", &format!("loaded from {}", p.display())),
        None => check(true, "config", "built-in defaults (no config file)"),
    }
    println!(
        "  paths: root={} apps={} toolchains={} bin={}",
        cfg.root.display(),
        cfg.apps_dir.display(),
        cfg.toolchains_dir.display(),
        cfg.bin_dir.display()
    );

    let ctx = base_ctx(&cfg, false);

    // git present (sealed FetchTool probe)
    let git_ok = gitfull::gitproc::run(
        &ExecCtx {
            resolve_path: cfg.host_tool_path.clone(),
            ..ctx.clone()
        },
        &["git".into(), "--version".into()],
        ExecClass::FetchTool,
        std::path::Path::new("."),
        &[
            ("PATH".into(), cfg.host_tool_path.clone()),
            ("LC_ALL".into(), "C".into()),
        ],
        None,
    )
    .map(|o| o.lines().next().unwrap_or("git").trim().to_string());
    match &git_ok {
        Ok(v) => check(true, "git (sealed fetch tool)", v),
        Err(e) => {
            ok = false;
            check(
                false,
                "git (sealed fetch tool)",
                &format!("NOT FOUND — {e}"),
            );
        }
    }

    // host cc present — probed ONLY as readiness for the sanctioned seed
    // bootstrap; classified as SeedHostCompiler to keep the audit story
    // exact (host cc is only ever touched for the seed).
    let host_cc = ["cc", "gcc"]
        .iter()
        .find_map(|p| {
            gitfull::gitproc::run(
                &ExecCtx {
                    resolve_path: cfg.host_tool_path.clone(),
                    ..ctx.clone()
                },
                &[p.to_string(), "--version".into()],
                ExecClass::SeedHostCompiler,
                std::path::Path::new("."),
                &[
                    ("PATH".into(), cfg.host_tool_path.clone()),
                    ("LC_ALL".into(), "C".into()),
                ],
                None,
            )
            .ok()
        })
        .map(|o| o.lines().next().unwrap_or("cc").trim().to_string());
    match &host_cc {
        Some(v) => check(
            true,
            "host compiler (seed only)",
            &format!("{v} — used exactly once, for the seed GCC build"),
        ),
        None => check(
            false,
            "host compiler (seed only)",
            "not found — seed GCC bootstrap will not be possible",
        ),
    }

    // curl (tarball fetches)
    let curl_ok = util::find_in_path("curl", &cfg.host_tool_path).is_some();
    check(
        curl_ok,
        "curl (tarball fetches)",
        "needed only for tarball toolchain sources",
    );

    // root writable?
    if cfg.root.exists() {
        let probe = cfg.root.join(".gitfull-doctor-probe");
        match std::fs::write(&probe, b"") {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                check(true, "root writable", "");
            }
            Err(e) => {
                ok = false;
                check(false, "root writable", &e.to_string());
            }
        }
    } else {
        check(
            true,
            "root",
            &format!("{} will be created on first install", cfg.root.display()),
        );
    }

    if cfg.bin_dir.exists() {
        let probe = cfg.bin_dir.join(".gitfull-doctor-probe");
        match std::fs::write(&probe, b"") {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                check(
                    true,
                    "bin dir writable",
                    &format!("{}", cfg.bin_dir.display()),
                );
            }
            Err(_) => {
                check(
                    false,
                    "bin dir writable",
                    &format!("{} — installs will fail until fixed", cfg.bin_dir.display()),
                );
            }
        }
    } else {
        check(
            true,
            "bin dir",
            &format!("{} created on first install", cfg.bin_dir.display()),
        );
    }

    let mgr = ToolchainManager::new(cfg.toolchains_dir.clone(), cfg.toolchain.clone());
    let gcc_versions = mgr.installed_versions("gcc");
    if gcc_versions.is_empty() {
        println!(
            "  [..] gcc toolchain           not bootstrapped — run `gitfull \
             toolchain bootstrap-gcc --execute` (host compiler used once)"
        );
    } else {
        check(true, "gcc toolchain", &gcc_versions.join(", "));
    }

    println!(
        "  policy: {} forbidden programs enforced at the exec chokepoint \
         (+{} user extensions); no package manager can be invoked",
        gitfull::gitproc::BUILTIN_FORBIDDEN.len(),
        cfg.policy.extra_forbidden_programs.len()
    );

    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn cmd_config(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let sub = rest.first().map(|s| s.as_str()).ok_or_else(|| {
        GitfullError::Usage("config requires a subcommand (show|validate|path)".into())
    })?;
    let (cfg, warnings) = load_config(g)?;
    match sub {
        "path" => {
            println!("{}", DEFAULT_CONFIG_PATH);
            Ok(ExitCode::SUCCESS)
        }
        "validate" => {
            println!("ok: configuration is valid");
            for w in &warnings {
                println!("warning: {w}");
            }
            Ok(ExitCode::SUCCESS)
        }
        "show" => {
            println!(
                "config:  {}",
                cfg.config_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(defaults)".into())
            );
            println!("root:    {}", cfg.root.display());
            println!("apps:    {}", cfg.apps_dir.display());
            println!("tchains: {}", cfg.toolchains_dir.display());
            println!("cache:   {}", cfg.cache_dir.display());
            println!("logs:    {}", cfg.logs_dir.display());
            println!("bin_dir: {}", cfg.bin_dir.display());
            println!("jobs:    {}", cfg.jobs);
            println!("host_tool_path: {}", cfg.host_tool_path);
            println!("default forge:  {}", cfg.forges.default_name());
            println!("forges:");
            for f in cfg.forges.all() {
                println!(
                    "  {}{} kind={} host={}",
                    f.name,
                    if f.builtin { " (builtin)" } else { "" },
                    f.kind(),
                    f.host()
                );
            }
            println!("repos configured: {}", cfg.repos.len());
            println!(
                "seed_gcc_version: {}",
                cfg.toolchain
                    .seed_gcc_version
                    .as_deref()
                    .unwrap_or("(auto: latest)")
            );
            println!(
                "extra forbidden programs: {:?}",
                cfg.policy.extra_forbidden_programs
            );
            Ok(ExitCode::SUCCESS)
        }
        other => Err(GitfullError::Usage(format!(
            "unknown config subcommand `{other}` (show|validate|path)"
        ))),
    }
}

fn cmd_audit(g: &Globals, rest: &[String]) -> Result<ExitCode> {
    let (cfg, _) = load_config(g)?;
    let n: usize = rest.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let log = cfg.root.join("audit.log");
    match util::read_file_if_exists(&log)? {
        Some(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n);
            for l in &lines[start..] {
                println!("{l}");
            }
            println!("({} total events)", lines.len());
        }
        None => println!("gitfull: no audit log yet at {}", log.display()),
    }
    Ok(ExitCode::SUCCESS)
}
