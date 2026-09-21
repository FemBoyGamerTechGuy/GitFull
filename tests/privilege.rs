//! Root-privilege model integration tests.
//!
//! Mutating operations (install / update / remove / toolchain builds) must
//! refuse to run as a normal user whenever gitfull operates on its system
//! paths (`/var/lib/gitfull` + a system-wide bin dir). The test suite runs
//! unprivileged, so the default-path configuration must be refused with a
//! sudo hint; explicitly overriding BOTH paths (dev mode) keeps it allowed
//! — which is exactly what the e2e install suite relies on.

use std::fs;
use std::path::PathBuf;

use gitfull::config::Config;
use gitfull::gitproc::ExecCtx;
use gitfull::planner;
use gitfull::spec::PkgSpec;

fn tmpdir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gitfull-priv-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn ctx(cfg: &Config) -> ExecCtx {
    ExecCtx {
        audit_log: Some(cfg.root.join("audit.log")),
        extra_forbidden: Vec::new(),
        redactions: Vec::new(),
        resolve_path: cfg.host_tool_path.clone(),
        interactive_override: None,
    }
}

/// A config with the DEFAULT system paths (no [core] section).
fn system_cfg() -> Config {
    let (cfg, _) = Config::load(std::path::Path::new("/nonexistent-gitfull.conf")).unwrap();
    cfg
}

/// Both paths overridden → dev mode, no root required.
fn dev_cfg(label: &str) -> Config {
    let d = tmpdir(label);
    let conf = d.join("gitfull.conf");
    fs::write(
        &conf,
        format!(
            "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\njobs = 1\n",
            d.join("root").display(),
            d.join("bin").display()
        ),
    )
    .unwrap();
    let (cfg, _) = Config::load(&conf).unwrap();
    cfg
}

fn local_spec(label: &str) -> PkgSpec {
    // a real local directory so parsing/canonicalization would work if
    // the privilege gate let it through
    let d = tmpdir(label);
    fs::write(d.join("Makefile"), "all:\n").unwrap();
    PkgSpec::parse(&d.display().to_string()).unwrap()
}

#[test]
fn install_requires_root_on_system_paths() {
    // The suite runs unprivileged; with default paths the very first thing
    // install does is refuse and tell the user to use sudo.
    let cfg = system_cfg();
    let spec = local_spec("sys-install");
    let opts = planner::InstallOpts {
        dry_run: true,
        yes: true,
        verbose: false,
    };
    let err = planner::install(&cfg, &ctx(&cfg), &spec, &opts).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("root required"), "{msg}");
    assert!(msg.contains("sudo gitfull install"), "{msg}");
    assert!(msg.contains("/var/lib/gitfull"), "{msg}");
}

#[test]
fn remove_requires_root_even_before_lookup() {
    // root check fires before the "not installed" error
    let cfg = system_cfg();
    let err = planner::remove(&cfg, &ctx(&cfg), "definitely-not-installed", false).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("sudo gitfull remove"), "{msg}");
}

#[test]
fn update_requires_root_on_system_paths() {
    let cfg = system_cfg();
    let opts = planner::InstallOpts {
        dry_run: true,
        yes: true,
        verbose: false,
    };
    let err = planner::update(&cfg, &ctx(&cfg), "anything", &opts).unwrap_err();
    assert!(format!("{err}").contains("sudo gitfull update"), "{err}");
}

#[test]
fn dev_mode_override_paths_skip_the_root_requirement() {
    // BOTH root and bin_dir overridden: allowed without root (this is what
    // the e2e suite and throwaway sandboxes rely on).
    let cfg = dev_cfg("devmode");
    assert!(!gitfull::privilege::requires_root_for(&cfg));
    assert!(gitfull::privilege::check(false, &cfg, "install").is_ok());
}

#[test]
fn one_system_path_still_requires_root() {
    // root overridden but bin_dir left at /usr/local/bin → still root
    let d = tmpdir("half");
    let conf = d.join("gitfull.conf");
    fs::write(
        &conf,
        format!("[core]\nroot = \"{}\"\njobs = 1\n", d.join("root").display()),
    )
    .unwrap();
    let (cfg, _) = Config::load(&conf).unwrap();
    assert!(gitfull::privilege::requires_root_for(&cfg));
    let err = gitfull::privilege::check(false, &cfg, "install").unwrap_err();
    assert!(format!("{err}").contains("/usr/local/bin"), "{err}");
}

#[test]
fn read_only_commands_have_no_gate() {
    // list must never hit the privilege gate (empty install state, custom
    // root not even required — list just reads)
    let cfg = system_cfg();
    assert!(planner::list(&cfg).is_ok());
}
