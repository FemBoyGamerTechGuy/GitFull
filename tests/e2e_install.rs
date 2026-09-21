//! End-to-end install test against a local source fixture with a faked
//! toolchain directory.
//!
//! This exercises the full pipeline WITHOUT any network and WITHOUT the
//! host compiler (the "gcc" used is a symlink seeded into the shared
//! toolchains directory, classified as ExecClass::Toolchain — the seed-GCC
//! bootstrap itself is out of scope in the dev environment by design):
//!
//!   spec (local path) -> sandbox -> auto-detect (Makefile) -> toolchain
//!   selection (shared) -> build in sandbox -> stage -> promote ->
//!   collect final binaries -> **install_binaries (the single
//!   sandbox-escape path)** -> meta.toml -> remove (hash-verified).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use gitfull::config::Config;
use gitfull::gitproc::ExecCtx;
use gitfull::planner::{self, InstallOpts};
use gitfull::sha256::sha256_file;
use gitfull::spec::PkgSpec;

fn have(prog: &str) -> bool {
    Path::new("/usr/bin").join(prog).is_file() || Path::new("/bin").join(prog).is_file()
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("gitfull-e2e-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("root/toolchains/gcc-14.2.0/bin")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();

        // ---- source fixture: a Makefile app -------------------------------
        fs::write(
            root.join("src/main.c"),
            r##"#include <stdio.h>
int main(void) {
    puts("hello from the gitfull sandbox");
    return 0;
}
"##,
        )
        .unwrap();
        fs::write(
            root.join("src/Makefile"),
            "PREFIX ?= /usr/local\n\
             \n\
             all: hello-gitfull\n\
             \n\
             hello-gitfull: main.c\n\
            \tcc -o hello-gitfull main.c\n\
             \n\
             install: hello-gitfull\n\
            \tmkdir -p $(DESTDIR)$(PREFIX)/bin\n\
            \tcp hello-gitfull $(DESTDIR)$(PREFIX)/bin\n",
        )
        .unwrap();

        // ---- fake shared toolchain: gcc/cc/g++ symlinks -------------------
        // (simulates an already-bootstrapped toolchains/gcc-<v>/)
        let tcbin = root.join("root/toolchains/gcc-14.2.0/bin");
        for prog in ["gcc", "cc", "g++"] {
            let host = ["/usr/bin", "/bin"]
                .iter()
                .map(|d| PathBuf::from(d).join(prog))
                .find(|p| p.is_file());
            if let Some(h) = host {
                std::os::unix::fs::symlink(&h, tcbin.join(prog)).unwrap();
            }
        }

        // ---- gitfull.conf --------------------------------------------------
        fs::write(
            root.join("gitfull.conf"),
            format!(
                "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\njobs = 2\n\n[toolchain.preferences]\ngcc = \"14.2.0\"\n",
                root.join("root").display(),
                root.join("bin").display()
            ),
        )
        .unwrap();

        Fixture { root }
    }

    fn cfg(&self) -> Config {
        let (cfg, warnings) = Config::load(&self.root.join("gitfull.conf")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        cfg
    }

    fn ctx(&self, cfg: &Config) -> ExecCtx {
        ExecCtx {
            audit_log: Some(cfg.root.join("audit.log")),
            extra_forbidden: Vec::new(),
            redactions: Vec::new(),
            resolve_path: cfg.host_tool_path.clone(),
        }
    }

    fn opts() -> InstallOpts {
        InstallOpts {
            dry_run: false,
            yes: true,
            verbose: false,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn install_build_stage_escape_remove() {
    if !have("gcc") || !have("cc") || !have("make") {
        eprintln!("skipping e2e: gcc/cc/make not present in this environment");
        return;
    }

    let fx = Fixture::new("escape");
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let opts = Fixture::opts();

    // install from a LOCAL path spec (no network)
    let spec = PkgSpec::parse(&fx.root.join("src").display().to_string()).unwrap();
    let rec = planner::install(&cfg, &ctx, &spec, &opts)
        .unwrap()
        .expect("install should produce a record");

    assert_eq!(rec.build_system, "make");
    assert_eq!(rec.bins.len(), 1, "expected exactly one final binary");
    let bin = PathBuf::from(&rec.bins[0].dest);
    assert_eq!(bin, fx.root.join("bin/hello-gitfull"));

    // THE sandbox escape produced a real, executable, correct binary
    assert!(bin.is_file());
    let mode = fs::metadata(&bin).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "binary must be executable");
    let out = Command::new(&bin).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello from the gitfull sandbox"));

    // provenance: recorded hash matches the installed file
    assert_eq!(rec.bins[0].sha256, sha256_file(&bin).unwrap());

    // sandbox containment: sources/build/stage all inside the app sandbox
    let sandbox_dir = cfg.apps_dir.join(&rec.name);
    assert!(sandbox_dir.join("src/Makefile").is_file());
    assert!(sandbox_dir.join("meta.toml").is_file());
    // nothing escaped besides the single binary: bin dir contains exactly it
    let bin_entries: Vec<_> = fs::read_dir(fx.root.join("bin"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(bin_entries.len(), 1);

    // audit log: build commands under toolchain class + install-binary event
    let audit = fs::read_to_string(cfg.root.join("audit.log")).unwrap();
    assert!(audit.contains("install-binary"), "{audit}");
    assert!(audit.contains("\ttoolchain\t"), "{audit}");
    assert!(audit.contains("\texec\ttoolchain\t"), "{audit}");
    assert!(audit.contains("make"), "{audit}");

    // shared toolchain was referenced, not duplicated
    assert!(rec
        .toolchains
        .iter()
        .any(|t| t.component == "gcc" && t.version == "14.2.0"));

    // ---- list / info ------------------------------------------------------
    let records = planner::list(&cfg).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, rec.name);

    // ---- remove: hash-verified --------------------------------------------
    planner::remove(&cfg, &ctx, &rec.name, false).unwrap();
    assert!(!bin.exists(), "binary must be removed");
    assert!(!sandbox_dir.exists(), "sandbox must be removed");
    let audit = fs::read_to_string(cfg.root.join("audit.log")).unwrap();
    assert!(audit.contains("remove-binary"), "{audit}");
}

#[test]
fn reinstall_wipes_sandbox() {
    if !have("gcc") || !have("cc") || !have("make") {
        eprintln!("skipping e2e reinstall: gcc/cc/make not present");
        return;
    }
    let fx = Fixture::new("reinstall");
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let opts = Fixture::opts();
    let spec = PkgSpec::parse(&fx.root.join("src").display().to_string()).unwrap();

    planner::install(&cfg, &ctx, &spec, &opts).unwrap().unwrap();
    // mutate the sandbox to prove the wipe happens
    let sandbox_dir = &cfg.apps_dir;
    let app_dir: PathBuf = fs::read_dir(sandbox_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .next()
        .unwrap();
    fs::write(app_dir.join("stale-marker"), b"x").unwrap();

    planner::install(&cfg, &ctx, &spec, &opts).unwrap().unwrap();
    assert!(
        !app_dir.join("stale-marker").exists(),
        "sandbox must be wiped on reinstall"
    );
    assert!(app_dir.join("meta.toml").is_file());
}

#[test]
fn dry_run_installs_nothing() {
    if !have("gcc") || !have("cc") || !have("make") {
        eprintln!("skipping e2e dry-run: gcc/cc/make not present");
        return;
    }
    let fx = Fixture::new("dryrun");
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let opts = InstallOpts {
        dry_run: true,
        yes: true,
        verbose: false,
    };
    let spec = PkgSpec::parse(&fx.root.join("src").display().to_string()).unwrap();

    let rec = planner::install(&cfg, &ctx, &spec, &opts).unwrap();
    assert!(rec.is_none(), "dry-run must not produce a record");
    assert!(fs::read_dir(fx.root.join("bin")).unwrap().flatten().count() == 0);
    assert!(!fx
        .root
        .join("root/apps")
        .join("local-src")
        .join("meta.toml")
        .exists());
}

#[test]
fn declared_bins_are_respected() {
    if !have("gcc") || !have("cc") || !have("make") {
        eprintln!("skipping e2e declared-bins: gcc/cc/make not present");
        return;
    }
    let fx = Fixture::new("declared");
    // repo gitfull.toml (optional file) declaring the binary explicitly
    fs::write(
        fx.root.join("src/gitfull.toml"),
        "[build]\nbins = [\"hello-gitfull\"]\n",
    )
    .unwrap();
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse(&fx.root.join("src").display().to_string()).unwrap();
    let rec = planner::install(&cfg, &ctx, &spec, &Fixture::opts())
        .unwrap()
        .unwrap();
    assert_eq!(rec.bins[0].name, "hello-gitfull");
}
