//! End-to-end dependency-graph discovery + provisioning tests.
//!
//! These prove the fix end to end, offline: gitfull parses the build
//! system's OWN manifests (a Makefile's `pkg-config` invocations here —
//! the same generic mechanism the unit tests exercise for meson, cmake,
//! cargo and autotools shapes), resolves each declared dependency,
//! clones it into the app sandbox, builds it with the (faked shared)
//! toolchain, registers it in the shared `<root>/libs/` cache — and the
//! produced application binary actually LINKS against the provisioned
//! libraries through that cache.
//!
//! Two resolution layers are covered live:
//!
//! * `[dep.<name>] source = "<local path>"` overrides, and
//! * ranked forge search + dumb-HTTP git clone against a std-only fake
//!   forge API (the fully generic path — no name tables anywhere).
//!
//! Cross-app cache reuse is proven by installing a SECOND app that needs
//! the same library: it must not clone or rebuild anything.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use gitfull::config::Config;
use gitfull::gitproc::ExecCtx;
use gitfull::planner::{self, InstallOpts};
use gitfull::spec::PkgSpec;

fn have(prog: &str) -> bool {
    Path::new("/usr/bin").join(prog).is_file() || Path::new("/bin").join(prog).is_file()
}

fn tools_ready() -> bool {
    have("gcc") && have("cc") && have("make") && have("pkg-config") && have("git")
}

// ---------------------------------------------------------------------------
// fixture: app + two library repos wired through [dep.<name>] overrides
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
}

/// A tiny library repo: builds `lib<name>.a`, installs a header, a
/// **relocatable** `.pc` file (`${pcfiledir}`-based, like well-behaved
/// real libraries), and — when `uses` is set — declares another
/// pkg-config dependency in its own Makefile (transitive discovery) and
/// carries it as `Requires:` in the .pc (the pkg-config link closure,
/// exactly how real libraries propagate static link needs).
fn write_lib_repo(dir: &Path, name: &str, uses: Option<&str>, body: &str) {
    fs::create_dir_all(dir).unwrap();
    let underscore = name.replace('-', "_");
    let pc_decl = match uses {
        Some(u) => format!(
            "CFLAGS += $(shell pkg-config --cflags {u})\nLDLIBS += $(shell pkg-config --libs {u})\n"
        ),
        None => String::new(),
    };
    // the source actually includes the transitive dependency's header
    // (discovered through PKG_CONFIG_PATH, not vendored)
    let uses_include = match uses {
        Some(u) => format!("#include \"{}.h\"\n", u.replace('-', "_")),
        None => String::new(),
    };
    fs::write(
        dir.join("Makefile"),
        format!(
            "PREFIX ?= /usr/local\n\n{pc_decl}\
             lib{name}.a: {underscore}.o\n\
             \tar rcs lib{name}.a {underscore}.o\n\n\
             {underscore}.o: {underscore}.c {underscore}.h\n\
             \t$(CC) $(CFLAGS) -c {underscore}.c\n\n\
             install: lib{name}.a\n\
             \tmkdir -p $(DESTDIR)$(PREFIX)/lib/pkgconfig $(DESTDIR)$(PREFIX)/include\n\
             \tcp lib{name}.a $(DESTDIR)$(PREFIX)/lib\n\
             \tcp {underscore}.h $(DESTDIR)$(PREFIX)/include\n\
             \tsed -e 's|@PREFIX@|$(PREFIX)|g' {name}.pc.in > $(DESTDIR)$(PREFIX)/lib/pkgconfig/{name}.pc\n"
        ),
    )
    .unwrap();
    fs::write(
        dir.join(format!("{underscore}.c")),
        format!("#include \"{underscore}.h\"\n{uses_include}{body}\n"),
    )
    .unwrap();
    fs::write(
        dir.join(format!("{underscore}.h")),
        format!(
            "#ifndef FAKE_{upper}_H\n#define FAKE_{upper}_H\nint {underscore}_entry(int n);\n#endif\n",
            upper = underscore.to_uppercase()
        ),
    )
    .unwrap();
    // relocatable .pc: prefix follows the .pc file's own location, so the
    // entry keeps working after gitfull moves it into <root>/libs/;
    // `Requires:` propagates the static link closure like a real library
    let requires = match uses {
        Some(u) => format!("Requires: {u}\n"),
        None => String::new(),
    };
    fs::write(
        dir.join(format!("{name}.pc.in")),
        format!(
            "prefix=${{pcfiledir}}/../..\n\
             libdir=${{prefix}}/lib\n\
             includedir=${{prefix}}/include\n\n\
             {requires}\
             Name: {name}\n\
             Description: gitfull e2e fixture library\n\
             Version: 1.0.0\n\
             Libs: -L${{libdir}} -l{name}\n\
             Cflags: -I${{includedir}}\n"
        ),
    )
    .unwrap();
}

fn write_app_repo(dir: &Path, binname: &str, deps: &[&str], code: &str) {
    fs::create_dir_all(dir).unwrap();
    let decl = if deps.is_empty() {
        String::new()
    } else {
        format!(
            "CFLAGS += $(shell pkg-config --cflags {})\nLDLIBS += $(shell pkg-config --libs {})\n",
            deps.join(" "),
            deps.join(" ")
        )
    };
    fs::write(
        dir.join("Makefile"),
        format!(
            "PREFIX ?= /usr/local\n\n{decl}\
             {binname}: main.c\n\
             \t$(CC) $(CFLAGS) -o {binname} main.c $(LDLIBS)\n\n\
             install: {binname}\n\
             \tmkdir -p $(DESTDIR)$(PREFIX)/bin\n\
             \tcp {binname} $(DESTDIR)$(PREFIX)/bin\n"
        ),
    )
    .unwrap();
    fs::write(dir.join("main.c"), code).unwrap();
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        Self::with_conf(label, None)
    }

    /// `extra_conf` appends TOML (dep mappings, forge overrides, ...).
    fn with_conf(label: &str, extra_conf: Option<String>) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "gitfull-e2edeps-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("root/toolchains/gcc-14.2.0/bin")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();

        // faked shared toolchain (an already-bootstrapped gcc), like the
        // other e2e fixtures: builds are Toolchain-class, never host-cc
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

        // ---- the app: declares fake-zlib + fake-utils in its Makefile --
        write_app_repo(
            &root.join("src-app"),
            "depshello",
            &["fake-zlib", "fake-utils"],
            r#"#include <stdio.h>
#include "fake_zlib.h"
#include "fake_utils.h"
int main(void) {
    printf("z=%d u=%d\n", fake_zlib_entry(2), fake_utils_entry(5));
    return 0;
}
"#,
        );
        // ---- library repos (fake-zlib itself uses fake-utils) ----------
        write_lib_repo(
            &root.join("src-zlib"),
            "fake-zlib",
            Some("fake-utils"),
            "int fake_zlib_entry(int n) { return n * 7 + fake_utils_entry(n); }",
        );
        write_lib_repo(
            &root.join("src-utils"),
            "fake-utils",
            None,
            "int fake_utils_entry(int n) { return n * 100; }",
        );

        let mut conf = format!(
            "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\njobs = 2\n\n\
             [toolchain.preferences]\ngcc = \"14.2.0\"\n",
            root.join("root").display(),
            root.join("bin").display()
        );
        if let Some(extra) = extra_conf {
            conf.push_str(&extra);
        }
        fs::write(root.join("gitfull.conf"), conf).unwrap();
        Fixture { root }
    }

    /// Fixture WITHOUT [dep] overrides (for search-resolution tests and
    /// failure-mode tests).
    fn with_dep_overrides(label: &str) -> Fixture {
        let extra = format!(
            "\n[dep.fake-zlib]\nsource = \"{}\"\n\n[dep.fake-utils]\nsource = \"{}\"\n",
            label_to_path(label).join("src-zlib").display(),
            label_to_path(label).join("src-utils").display()
        );
        Self::with_conf(label, Some(extra))
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

    fn install_app(&self, src: &Path) -> planner::InstallRecord {
        let cfg = self.cfg();
        let ctx = self.ctx(&cfg);
        let spec = PkgSpec::parse(&src.display().to_string()).unwrap();
        planner::install(&cfg, &ctx, &spec, &Fixture::opts())
            .unwrap()
            .expect("install should produce a record")
    }
}

fn label_to_path(label: &str) -> PathBuf {
    // NOTE: same label → same root as Fixture::with_conf used
    std::env::temp_dir().join(format!(
        "gitfull-e2edeps-{label}-{}",
        std::process::id()
    ))
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn libs_entries(cfg: &Config) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&cfg.libs_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().join("meta.toml").is_file() {
                out.push((name, e.path()));
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// tests: discovery -> provisioning -> linking -> cache -> reuse
// ---------------------------------------------------------------------------

#[test]
fn discovers_provisions_and_links_transitive_deps() {
    if !tools_ready() {
        eprintln!("skipping deps e2e: gcc/cc/make/pkg-config/git not present");
        return;
    }
    let fx = Fixture::with_dep_overrides("transitive");
    let cfg = fx.cfg();

    // BEFORE: the fix, this install failed at the build's own dependency
    // resolution (pkg-config: fake-zlib not found). NOW the declared deps
    // are discovered from the Makefile, fetched, built and linked.
    let rec = fx.install_app(&fx.root.join("src-app"));

    assert_eq!(rec.build_system, "make");
    // two dependency nodes: fake-zlib + fake-utils (deduped from the
    // app-level and the transitive declaration)
    assert_eq!(rec.packages.len(), 2, "{:?}", rec.packages);
    assert!(
        rec.packages
            .iter()
            .any(|k| k.contains("src-zlib") || k.contains("fake-zlib")),
        "{:?}",
        rec.packages
    );
    // both registered in the shared library cache
    assert_eq!(rec.libs.len(), 2, "{:?}", rec.libs);
    let entries = libs_entries(&cfg);
    assert_eq!(entries.len(), 2, "{entries:?}");
    for (key, dir) in &entries {
        assert!(dir.join("meta.toml").is_file());
        let meta_text = fs::read_to_string(dir.join("meta.toml")).unwrap();
        let meta: toml::Value = toml::from_str(&meta_text).unwrap();
        let provides = meta
            .get("provides")
            .and_then(|p| p.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        let _ = key;
        assert!(
            provides.iter().any(|p| *p == "fake-zlib")
                || provides.iter().any(|p| *p == "fake-utils"),
            "{provides:?}"
        );
    }

    // the built binary REALLY links against the provisioned libraries
    let bin = fx.root.join("bin/depshello");
    assert!(bin.is_file());
    let out = Command::new(&bin).output().unwrap();
    assert!(out.status.success());
    // fake_zlib_entry(2) = 2*7 + fake_utils_entry(2) = 14 + 200 = 214
    // fake_utils_entry(5) = 500
    assert_eq!(String::from_utf8_lossy(&out.stdout), "z=214 u=500\n");

    // both dep sandboxes were cloned inside the app sandbox
    let deps_dir = cfg.apps_dir.join(&rec.name).join("deps");
    let dep_subdirs: Vec<_> = fs::read_dir(&deps_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(dep_subdirs.len(), 2, "{deps_dir:?}");

    // the built libs' artifacts live in the shared cache now
    let zlib_entry = entries
        .iter()
        .find(|(_, d)| d.join("lib/pkgconfig/fake-zlib.pc").is_file())
        .map(|(_, d)| d.clone())
        .expect("fake-zlib cache entry with a .pc file");
    assert!(zlib_entry.join("lib/libfake-zlib.a").is_file());
    assert!(zlib_entry.join("include/fake_zlib.h").is_file());

    // relocatable .pc: prefix tracks the .pc file's own location
    let pc = fs::read_to_string(zlib_entry.join("lib/pkgconfig/fake-zlib.pc")).unwrap();
    assert!(pc.contains("${pcfiledir}"), "{pc}");

    // the audit trail covers dependency builds too: every build step of
    // every dep sandbox went through the exec chokepoint (toolchain
    // class); pkg-config itself runs inside make's $(shell) and is not
    // separately audited — its effect is proven by the link succeeding
    let audit = fs::read_to_string(cfg.root.join("audit.log")).unwrap();
    assert!(audit.contains("\ttoolchain\t"), "{audit}");
    assert!(audit.contains("src-zlib"), "dependency sandbox builds audited: {audit}");
    assert!(audit.contains("src-utils"), "dependency sandbox builds audited: {audit}");
}

#[test]
fn second_app_reuses_the_shared_lib_cache_without_rebuilding() {
    if !tools_ready() {
        eprintln!("skipping deps e2e reuse: tools not present");
        return;
    }
    let fx = Fixture::with_dep_overrides("reuse");
    let cfg = fx.cfg();

    // app A: provisions fake-zlib + fake-utils from scratch
    let _rec_a = fx.install_app(&fx.root.join("src-app"));
    assert_eq!(libs_entries(&cfg).len(), 2);

    // app B: a DIFFERENT repo that needs only fake-zlib
    write_app_repo(
        &fx.root.join("src-app2"),
        "depshello2",
        &["fake-zlib"],
        r#"#include <stdio.h>
#include "fake_zlib.h"
int main(void) {
    printf("b=%d\n", fake_zlib_entry(3));
    return 0;
}
"#,
    );
    let rec_b = fx.install_app(&fx.root.join("src-app2"));

    // the library was NOT fetched again: no dep sandboxes for app B
    let deps_dir = cfg.apps_dir.join(&rec_b.name).join("deps");
    let dep_subdirs: Vec<_> = match fs::read_dir(&deps_dir) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).collect(),
        Err(_) => Vec::new(),
    };
    assert!(
        dep_subdirs.is_empty(),
        "cache hit must not clone dependency sources: {deps_dir:?}"
    );

    // and nothing new was registered: still exactly two entries
    assert_eq!(libs_entries(&cfg).len(), 2, "no new cache entries");

    // the reuse is recorded as a library reference in meta.toml
    assert!(rec_b.libs.iter().any(|l| l.name == "fake-zlib"), "{:?}", rec_b.libs);

    // app B's binary links the SAME cache entry and runs
    let bin = fx.root.join("bin/depshello2");
    let out = Command::new(&bin).output().unwrap();
    assert!(out.status.success());
    // fake_zlib_entry(3) = 3*7 + 300 = 321
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b=321\n");
}

#[test]
fn unresolvable_required_dep_is_a_clear_error() {
    if !tools_ready() {
        eprintln!("skipping deps e2e unresolvable: tools not present");
        return;
    }
    // no [dep] overrides, and every forge configured without a usable
    // search API (github's api_base is a constant for the github kind,
    // so it must be overridden explicitly) → resolution must fail with
    // the dependency's name and the [dep] hint
    let extra = "\n[forge.github]\nkind = \"github\"\nhost = \"fake.invalid\"\napi_base = \"https://fake.invalid\"\n\n\
                 [forge.gitlab]\nkind = \"gitlab\"\nhost = \"fake.invalid\"\n\n\
                 [forge.codeberg]\nkind = \"forgejo\"\nhost = \"fake.invalid\"\n";
    let fx = Fixture::with_conf("unresolvable", Some(extra.to_string()));
    write_app_repo(
        &fx.root.join("src-app3"),
        "neverbuilt",
        &["ghost-lib"],
        "int main(void){return 0;}\n",
    );
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse(&fx.root.join("src-app3").display().to_string()).unwrap();

    let err = match planner::install(&cfg, &ctx, &spec, &Fixture::opts()) {
        Ok(_) => panic!("expected an unresolvable-dependency error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(msg.contains("ghost-lib"), "{msg}");
    assert!(msg.contains("[dep."), "{msg}");
    // nothing was installed and no cache entry was created
    assert!(libs_entries(&cfg).is_empty());
    assert_eq!(fs::read_dir(fx.root.join("bin")).unwrap().flatten().count(), 0);
}

#[test]
fn dry_run_discovers_but_provisions_nothing() {
    if !tools_ready() {
        eprintln!("skipping deps e2e dry-run: tools not present");
        return;
    }
    let fx = Fixture::with_dep_overrides("dryrun");
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse(&fx.root.join("src-app").display().to_string()).unwrap();
    let opts = InstallOpts {
        dry_run: true,
        yes: true,
        verbose: false,
    };

    let rec = planner::install(&cfg, &ctx, &spec, &opts).unwrap();
    assert!(rec.is_none(), "dry-run must not produce a record");
    assert!(libs_entries(&cfg).is_empty(), "dry-run must not build libs");
    assert_eq!(fs::read_dir(fx.root.join("bin")).unwrap().flatten().count(), 0);
}

// ---------------------------------------------------------------------------
// the fully generic path: ranked forge search -> dumb-HTTP git clone
// ---------------------------------------------------------------------------

/// A std-only fake forge: JSON search API + static file server for git's
/// dumb HTTP protocol (the repos are prepared with `git
/// update-server-info`).
struct FakeForgeServer {
    addr: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl FakeForgeServer {
    fn start(docroot: PathBuf) -> FakeForgeServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        let docroot = Arc::new(docroot);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = match conn {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let hits = hits2.clone();
                let docroot = docroot.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match conn.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_string();
                    if let Ok(mut h) = hits.lock() {
                        h.push(path.clone());
                    }
                    let resp = route(&path, &docroot);
                    let _ = conn.write_all(resp.as_slice());
                    let _ = conn.flush();
                });
            }
        });
        FakeForgeServer { addr, hits }
    }

    fn hit_count(&self, needle: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains(needle))
            .count()
    }
}

fn route(path: &str, docroot: &Path) -> Vec<u8> {
    // ---- the search API (github kind) -----------------------------------
    if path.starts_with("/search/repositories") {
        let body = r#"{"total_count": 1, "items": [
            {"full_name": "octocat/fake-zlib",
             "stargazers_count": 142,
             "pushed_at": "2026-08-30T08:21:00Z"}
        ]}"#;
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
    }
    // ---- github-style detail enrichment ----------------------------------
    if path.contains("/repos/octocat/fake-zlib/contributors") {
        let body = r#"[{"login":"a"},{"login":"b"},{"login":"c"}]"#;
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
    }
    if path.contains("/repos/octocat/fake-zlib/commits") {
        let body = r#"[{"sha":"1"},{"sha":"2"}]"#;
        return format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
    }
    // ---- static files: git's dumb-HTTP protocol ---------------------------
    let rel = path.trim_start_matches('/');
    let file = docroot.join(rel);
    if file.is_file() {
        match fs::read(&file) {
            Ok(bytes) => {
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .into_bytes();
                resp.extend_from_slice(&bytes);
                return resp;
            }
            Err(_) => {}
        }
    }
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        .as_bytes()
        .to_vec()
}

/// Prepare a git repo servable over dumb HTTP: returns the repo worktree.
fn dumb_http_repo(parent: &Path, rel: &str) -> PathBuf {
    let work = parent.join(rel).join("work");
    fs::create_dir_all(&work).unwrap();
    let repo_name = rel.rsplit('/').next().unwrap();
    write_lib_repo(
        &work,
        repo_name,
        None, // leaf library for the search test
        "int fake_zlib_entry(int n) { return n * 11; }",
    );
    let out = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(out.status.success(), "git init: {}", String::from_utf8_lossy(&out.stderr));
    for cmd in [
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
        vec!["add", "."],
        vec!["commit", "-qm", "fixture"],
    ] {
        let out = Command::new("git")
            .args(&cmd)
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {cmd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // expose the .git content as <docroot>/<owner>/<repo>.git — a
    // non-bare copy of the internal .git directory, servable as static
    // files by git's dumb HTTP protocol
    let serve_dir = parent.join(format!("{rel}.git"));
    fs::create_dir_all(&serve_dir).unwrap();
    copy_dir(&work.join(".git"), &serve_dir);
    // update-server-info writes info/refs for the dumb protocol
    let out = Command::new("git")
        .arg("update-server-info")
        .current_dir(&serve_dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    work
}

fn copy_dir(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).unwrap();
    for e in fs::read_dir(src).unwrap().flatten() {
        let p = e.path();
        let d = dest.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &d);
        } else {
            fs::copy(&p, &d).unwrap();
        }
    }
}

#[test]
fn search_resolves_dep_name_then_clones_builds_and_links() {
    if !tools_ready() {
        eprintln!("skipping deps e2e search: tools not present");
        return;
    }
    if !have("curl") {
        eprintln!("skipping deps e2e search: curl not present");
        return;
    }
    // staging area for the fake forge + its servable git repo — a path
    // DISJOINT from the fixture root (the fixture constructor wipes its
    // own root, which would otherwise destroy the docroot)
    let stage = std::env::temp_dir().join(format!(
        "gitfull-e2edeps-fakeforge-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(stage.join("docroot/octocat")).unwrap();
    dumb_http_repo(&stage.join("docroot"), "octocat/fake-zlib");

    let server = FakeForgeServer::start(stage.join("docroot"));

    // fixture with a github-kind forge pointed at the fake server — NO
    // [dep] overrides: the dependency name must resolve through search
    let extra = format!(
        "\n[forge.github]\nkind = \"github\"\nhost = \"127.0.0.1\"\napi_base = \"{api}\"\nclone_template = \"{api}/{{owner}}/{{repo}}.git\"\n\n\
         [forge.gitlab]\nkind = \"gitlab\"\nhost = \"fake.invalid\"\n\n\
         [forge.codeberg]\nkind = \"forgejo\"\nhost = \"fake.invalid\"\n",
        api = server.addr
    );
    let fx = Fixture::with_conf("searchfx", Some(extra));
    write_app_repo(
        &fx.root.join("src-app4"),
        "searchhello",
        &["fake-zlib"],
        r#"#include <stdio.h>
#include "fake_zlib.h"
int main(void) {
    printf("s=%d\n", fake_zlib_entry(4));
    return 0;
}
"#,
    );
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse(&fx.root.join("src-app4").display().to_string()).unwrap();

    let rec = planner::install(&cfg, &ctx, &spec, &Fixture::opts())
        .unwrap()
        .expect("search-resolved install should succeed");

    // the search API was actually used for the dep name
    assert!(server.hit_count("/search/repositories") >= 1);
    assert!(server.hit_count("/octocat/fake-zlib.git/info/refs") >= 1);

    // the dep was cloned from the fake forge and cached
    assert_eq!(rec.packages.len(), 1, "{:?}", rec.packages);
    assert!(rec.packages[0].contains("octocat/fake-zlib"), "{:?}", rec.packages);
    let entries = libs_entries(&cfg);
    assert_eq!(entries.len(), 1, "{entries:?}");

    // and the app really links the searched-and-built library
    let bin = fx.root.join("bin/searchhello");
    let out = Command::new(&bin).output().unwrap();
    assert!(out.status.success());
    // fake_zlib_entry(4) = 4 * 11
    assert_eq!(String::from_utf8_lossy(&out.stdout), "s=44\n");

    // clone progress: the dep fetch ran through the shared progress-UI
    // code path (same git_clone entry point as the main repo)
    let audit = fs::read_to_string(cfg.root.join("audit.log")).unwrap();
    assert!(
        audit.contains("fetch-tool\t/usr/bin/git\t"),
        "dep clone through the shared git_clone entry point: {audit:?}"
    );
    assert!(audit.contains("octocat/fake-zlib"), "{audit}");

    let _ = fs::remove_dir_all(&stage);
}

// ---------------------------------------------------------------------------
// cargo: registry deps are report-only; a zero-dep crate builds + installs
// ---------------------------------------------------------------------------

/// Locate the REAL cargo/rustc binaries (not rustup shims — the shims
/// resolve a default toolchain from RUSTUP_HOME/HOME, which does not
/// exist inside a gitfull sandbox). Scans `$RUSTUP_HOME/toolchains/*`
/// (falling back to `~/.rustup/toolchains/*`) and honors the `CARGO`
/// env var when it points inside a toolchains dir.
fn real_cargo_rustc() -> Option<(PathBuf, PathBuf)> {
    if let Ok(c) = std::env::var("CARGO") {
        let c = PathBuf::from(c);
        if c.display().to_string().contains("toolchains") && c.is_file() {
            let rustc = c.parent().unwrap().join("rustc");
            if rustc.is_file() {
                return Some((c, rustc));
            }
        }
    }
    let home = std::env::var("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
                .join(".rustup")
        });
    let tcs = home.join("toolchains");
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&tcs) {
        for e in rd.flatten() {
            let cargo = e.path().join("bin/cargo");
            let rustc = e.path().join("bin/rustc");
            if cargo.is_file() && rustc.is_file() {
                found.push((cargo, rustc));
            }
        }
    }
    found.sort();
    found.into_iter().next()
}

#[test]
fn cargo_app_with_no_registry_deps_builds_and_installs() {
    // rustc determines its sysroot from the resolved executable path
    // (current_exe follows symlinks), so symlinking the real binaries
    // into the faked toolchain keeps them fully functional under the
    // sandbox HOME
    let Some((cargo, rustc)) = real_cargo_rustc() else {
        eprintln!("skipping cargo e2e: real cargo/rustc binaries not found");
        return;
    };

    let fx = Fixture::new("cargo");
    // seed a faked shared rust toolchain (version read from the real one)
    let vout = Command::new(&rustc).arg("--version").output().unwrap();
    let v = String::from_utf8_lossy(&vout.stdout);
    let version = v
        .split_whitespace()
        .nth(1)
        .expect("rustc --version output")
        .to_string();
    let tcbin = fx
        .root
        .join(format!("root/toolchains/rust-{version}/bin"));
    fs::create_dir_all(&tcbin).unwrap();
    std::os::unix::fs::symlink(&cargo, tcbin.join("cargo")).unwrap();
    std::os::unix::fs::symlink(&rustc, tcbin.join("rustc")).unwrap();

    // the app: a cargo project with NO registry dependencies (offline)
    let app = fx.root.join("src-cargo-app");
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = \"cargohello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        app.join("src/main.rs"),
        "fn main() { println!(\"cargo says hi\"); }\n",
    )
    .unwrap();

    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse(&app.display().to_string()).unwrap();
    let rec = planner::install(&cfg, &ctx, &spec, &Fixture::opts())
        .unwrap()
        .expect("cargo install should succeed");

    assert_eq!(rec.build_system, "cargo");
    // no source dependencies to provision for a zero-dep crate
    assert!(rec.packages.is_empty(), "{:?}", rec.packages);
    let bin = fx.root.join("bin/cargohello");
    assert!(bin.is_file(), "binary must be collected from CARGO_TARGET_DIR");
    let mode = fs::metadata(&bin).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111);
    let out = Command::new(&bin).output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "cargo says hi\n");
}
