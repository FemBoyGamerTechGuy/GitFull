//! Root-privilege model.
//!
//! gitfull writes to `<root>` (default `/var/lib/gitfull`) and copies final
//! binaries into a system-wide bin dir (default `/usr/local/bin`). Both are
//! root-owned on a normal Linux install, so **every state-changing
//! operation — install, update, remove, toolchain builds — must run as
//! root** (`sudo gitfull ...`).
//!
//! Read-only commands (`list`, `info`, `doctor`, `config`, `audit`) never
//! need root: they only read state that already exists (and filesystem
//! permissions remain the final arbiter of what they can see).
//!
//! # Dev/test carve-out (explicit, not silent)
//!
//! When *both* `core.root` **and** `core.bin_dir` are explicitly overridden
//! away from the system defaults, gitfull is deliberately operating on
//! user-writable paths — the e2e test suite and throwaway sandboxes rely on
//! this. The carve-out requires BOTH overrides: keeping either system path
//! keeps the root requirement (writing `/var/lib/gitfull` OR copying into
//! `/usr/local/bin` each need root on their own).
//!
//! The enforcement lives in the library entry points
//! (`planner::install` / `planner::update` / `planner::remove`,
//! `bootstrap::ensure_components` / `bootstrap_seed_gcc` / `build_component`
//! with `--execute`), not just in the CLI, so it cannot be bypassed by
//! calling the API directly.

use std::path::PathBuf;

use crate::config::{Config, DEFAULT_BIN_DIR, DEFAULT_ROOT};
use crate::error::{GitfullError, Result};

/// Effective user ID, read from `/proc/self/status` (`Uid:` line, second
/// field = euid). Hand-rolled to avoid a libc dependency; gitfull is
/// Linux/Unix-only anyway.
pub fn euid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
            let mut it = rest.split_whitespace();
            let _real = it.next()?;
            let eff = it.next()?;
            return eff.parse().ok();
        }
    }
    None
}

pub fn is_root() -> bool {
    euid() == Some(0)
}

/// Pure decision core (testable without actually being root):
/// does `cfg` describe a root-requiring deployment for a mutating command?
pub fn requires_root_for(cfg: &Config) -> bool {
    let system_root = cfg.root == PathBuf::from(DEFAULT_ROOT);
    let system_bin = cfg.bin_dir == PathBuf::from(DEFAULT_BIN_DIR);
    system_root || system_bin
}

/// The root check itself: `running_as_root` is injected so the decision is
/// unit-testable; [`require_root`] wires in the real euid.
pub fn check(running_as_root: bool, cfg: &Config, cmd: &str) -> Result<()> {
    if running_as_root || !requires_root_for(cfg) {
        return Ok(());
    }
    Err(GitfullError::Privilege {
        cmd: cmd.to_string(),
        root: cfg.root.clone(),
        bin_dir: cfg.bin_dir.clone(),
    })
}

/// Enforce root for a state-changing operation (see module docs).
///
/// Called by the mutating code paths — `install`, `update`, `remove`,
/// and toolchain builds with execution enabled.
pub fn require_root(cfg: &Config, cmd: &str) -> Result<()> {
    check(is_root(), cfg, cmd)
}

/// Human-readable mode line for `gitfull doctor`.
pub fn mode_line(cfg: &Config) -> String {
    let who = if is_root() { "root" } else { "regular user" };
    if requires_root_for(cfg) {
        format!(
            "{who} — mutating commands (install, update, remove, toolchain …--execute) \
             require sudo on this deployment (root={}, bin_dir={})",
            cfg.root.display(),
            cfg.bin_dir.display()
        )
    } else {
        format!(
            "{who} — non-system root/bin_dir in use ({} / {}): dev mode, \
             mutating commands allowed without sudo",
            cfg.root.display(),
            cfg.bin_dir.display()
        )
    }
}

/// Read-only commands that never require root (used in help text).
pub const READ_ONLY_COMMANDS: &[&str] = &["list", "info", "doctor", "config", "audit"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoreSection;
    use std::path::Path;

    fn cfg_with(root: &str, bin: &str) -> Config {
        let core = CoreSection {
            root: PathBuf::from(root),
            bin_dir: PathBuf::from(bin),
            jobs: 2,
            host_tool_path: "/usr/bin:/bin".into(),
            color: "auto".into(),
        };
        let (cfg, _) = Config::load(Path::new("/nonexistent/gitfull.conf")).unwrap();
        let mut cfg = cfg;
        cfg.root = core.root.clone();
        cfg.bin_dir = core.bin_dir.clone();
        cfg.apps_dir = cfg.root.join("apps");
        cfg.toolchains_dir = cfg.root.join("toolchains");
        cfg.cache_dir = cfg.root.join("cache");
        cfg.logs_dir = cfg.root.join("logs");
        cfg
    }

    #[test]
    fn decision_core() {
        let sys = cfg_with(DEFAULT_ROOT, DEFAULT_BIN_DIR);
        assert!(requires_root_for(&sys));
        // root passes always
        assert!(check(true, &sys, "install").is_ok());
        // non-root + system paths → refusal with a sudo hint
        let err = check(false, &sys, "install").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("sudo"), "{msg}");
        assert!(msg.contains("install"), "{msg}");
        assert!(msg.contains(DEFAULT_ROOT), "{msg}");
        // one system path is enough to keep the requirement
        let half = cfg_with(DEFAULT_ROOT, "/home/z/.local/bin");
        assert!(requires_root_for(&half));
        assert!(check(false, &half, "remove").is_err());
        // both overridden → dev mode, non-root allowed
        let dev = cfg_with("/tmp/gitfull-root", "/tmp/gitfull-bin");
        assert!(!requires_root_for(&dev));
        assert!(check(false, &dev, "install").is_ok());
    }

    #[test]
    fn euid_parses() {
        // We are a real process with a real /proc entry; must parse.
        let e = euid();
        assert!(e.is_some(), "failed to parse /proc/self/status");
        // The test suite normally runs unprivileged; either way is a valid
        // value, but the parse itself must succeed.
    }
}
