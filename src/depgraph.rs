//! Build-system-native dependency-graph discovery.
//!
//! gitfull does not stop at "this project uses meson": it parses the
//! **build system's own dependency declarations** in the cloned source
//! tree and provisions every declared library dependency from source,
//! exactly like it provisions toolchain components.
//!
//! | build system | files parsed                                    | declarations recognized                                     |
//! |--------------|-------------------------------------------------|-------------------------------------------------------------|
//! | meson        | every `meson.build`, plus `subprojects/*.wrap`  | `dependency('name', ...)` calls **in their conditional context** (see [`crate::mesoneval`]: `if`/`elif`/`else`, `host_machine.system()` & friends — deps provably unreachable for this platform are not surfaced); `.wrap` subprojects (git or file) |
//! | cmake        | every `CMakeLists.txt` and `*.cmake`            | `find_package(Name)`, `find_library(... NAMES x ...)`, `pkg_check_modules(... mods)` |
//! | cargo        | `Cargo.toml` (workspace members too)            | `[dependencies]` / `[build-dependencies]` / `[target...]` (+ git deps) |
//! | autotools    | `configure.ac`                                  | `PKG_CHECK_MODULES`, `AC_CHECK_LIB`, `AC_SEARCH_LIBS`       |
//! | make         | `Makefile`                                      | `pkg-config ... <module>` invocations                       |
//!
//! # Generality constraint (by design, enforced by tests)
//!
//! Discovery is **generic across arbitrary repositories**. It is driven
//! exclusively by parsing each build system's own manifest format at
//! install time. gitfull contains **no per-repo, per-project or
//! per-library name tables** — the only name lists in this module are
//! *build-system-semantic* skip sets (e.g. `threads` is a meson
//! built-in dependency; `pthread`/`m`/`dl` are libc pieces delivered by
//! the toolchain's own compiler). Those sets describe build-system
//! semantics, not any particular project, and they are covered by tests
//! that scan several unrelated fixture repositories with different
//! dependency sets to prove the parsers are not tuned to one example.
//!
//! Resolution of a discovered name to a source happens in
//! [`crate::planner`] through fixed layers (see docs/ARCHITECTURE.md):
//! user `[dep.<name>]` config override → meson `.wrap` directives →
//! vendored subprojects → shared library cache (`<root>/libs/`, matched
//! by the names the built library actually *provides*) → the curated
//! upstream map ([`crate::libmap`]: well-known pkg-config module names
//! → their correct upstream repos) → ranked forge search as a FLAGGED
//! fallback whose unconfirmed matches are never auto-built.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{GitfullError, Result};
use crate::manifest::BuildSystem;

// ---------------------------------------------------------------------------
// model
// ---------------------------------------------------------------------------

/// How one dependency was declared inside the build files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// meson `dependency('x')` / pkg-config module name.
    PkgConfig,
    /// cmake `find_package(X)` / `find_library(NAMES x)`.
    CmakePackage,
    /// autotools `AC_CHECK_LIB([x])` (library `-lx`).
    AutoconfLib,
    /// a crates.io-style registry dependency of a cargo project — the
    /// cargo resolver itself fetches these into the app's sandbox at
    /// build time, so gitfull reports them but does not provision them.
    CrateRegistry,
    /// cargo `foo = { git = "..." }` dependency.
    CrateGit,
    /// meson `[wrap-git]` subproject.
    WrapGit,
    /// meson `[wrap-file]` subproject (source tarball).
    WrapFile,
}

impl DepKind {
    pub fn label(self) -> &'static str {
        match self {
            DepKind::PkgConfig => "pkg-config module",
            DepKind::CmakePackage => "cmake package",
            DepKind::AutoconfLib => "autoconf lib",
            DepKind::CrateRegistry => "cargo registry dep",
            DepKind::CrateGit => "cargo git dep",
            DepKind::WrapGit => "meson wrap (git)",
            DepKind::WrapFile => "meson wrap (file)",
        }
    }
}

/// One dependency declaration found while parsing a build file.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredDep {
    /// The name as written in the build file (display).
    pub name: String,
    /// Name normalized for matching/resolution (lowercase).
    pub name_norm: String,
    pub kind: DepKind,
    /// `true` unless the declaration marks it optional
    /// (`required: false`, a feature option resolving to `'auto'`,
    /// `find_package(... OPTIONAL)`, ...).
    pub required: bool,
    /// Why the declaration is optional, per the manifest's own semantics
    /// (`required: false`, the feature option's resolved state, ...) —
    /// shown in reports so the classification is auditable.
    pub optional_why: Option<String>,
    /// Version constraint as written (e.g. `>=1.2`), report-only: the
    /// build system's own dependency check stays the version authority.
    pub version: Option<String>,
    /// `file:line` where it was declared.
    pub origin: String,
    /// For `CrateGit` deps: the git URL from the manifest.
    pub git_url: Option<String>,
    /// meson `fallback: ['subproject', 'var']` — the subproject (wrap)
    /// that satisfies this dependency when the system lookup fails.
    /// meson's own fallback semantics: a wrap with this stem is THE
    /// source for the name (e.g. `dependency('xmlb', fallback:
    /// ['libxmlb', …])` resolves through subprojects/libxmlb.wrap).
    pub fallback_subproject: Option<String>,
    /// Fallback names declared AFTER the primary in the same meson call
    /// — `dependency('libsystemd', 'libelogind', …)` tries names in
    /// order and uses the FIRST one that resolves. Normalized,
    /// deduplicated, declared order preserved; empty for single-name
    /// calls and for non-meson build systems.
    pub alt_names: Vec<String>,
}

/// A parsed meson subproject wrap file.
#[derive(Debug, Clone, PartialEq)]
pub struct WrapDep {
    /// Wrap file stem (e.g. `zlib` for `subprojects/zlib.wrap`).
    pub name: String,
    /// `[wrap-git]`: repository url + revision.
    pub git: Option<(String, String)>,
    /// `[wrap-file]`: source url, the wrapdb `source_fallback_url`,
    /// and an optional patch url.
    pub file: Option<(String, Option<String>, Option<String>)>,
    /// `[provide]` provided pkg-config names — both wrap-db conventions
    /// (`dependency_names = a,b` and `a = a_dep` per line).
    pub provides: Vec<String>,
    pub origin: String,
}

/// Everything one source tree declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclaredDeps {
    /// Dependencies gitfull must provision (or prove satisfied).
    pub required: Vec<DeclaredDep>,
    /// Declared but optional per the manifest (`required: false`, a
    /// feature option in `'auto'`, ...) — resolved silently when a sound
    /// source exists (pin / wrap / cache / curated map), never blocking.
    pub optional: Vec<DeclaredDep>,
    /// meson subproject wraps (an independent, self-describing source).
    pub wraps: Vec<WrapDep>,
    /// meson vendored subprojects (`subprojects/<name>/` checked-in
    /// trees, no wrap): resolved in-tree, never fetched.
    pub vendored: Vec<String>,
    /// Modules this very tree's own build provides — meson's
    /// `meson.override_dependency('name', <dep>)` declarations (e.g.
    /// GLib's tree provides `girepository-2.0`). A `dependency()` call
    /// for such a name is satisfied by building THIS tree, not by
    /// fetching anything.
    pub provided_in_tree: Vec<String>,
}

impl DeclaredDeps {
    /// The set of *source* dependencies to resolve (excludes the
    /// build-system built-ins and cargo registry deps, which are
    /// satisfied by other means).
    pub fn required_names(&self) -> Vec<&DeclaredDep> {
        self.required
            .iter()
            .filter(|d| d.kind != DepKind::CrateRegistry)
            .collect()
    }

    /// Names that wraps already satisfy (a dependency() call whose name
    /// is provided by a wrap is fetched via that wrap, not via search).
    pub fn wrap_provided(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for w in &self.wraps {
            for p in &w.provides {
                out.insert(p.to_ascii_lowercase());
            }
            out.insert(w.name.to_ascii_lowercase());
        }
        out
    }
}

// ---------------------------------------------------------------------------
// build-system built-in skip sets (semantics, NOT project-specific lists)
// ---------------------------------------------------------------------------

/// meson built-in `dependency()` names that never denote an external
/// library to fetch (they resolve against the toolchain itself).
/// `iconv` is included because every libc gitfull builds against
/// (glibc, musl, darwin, the BSDs) provides it in the C library — the
/// same resolution category as `threads`, not a fetchable module.
pub const MESON_BUILTINS: &[&str] = &[
    "threads",
    "python3",
    "gtest",
    "gmock",
    "disabler",
    "iconv",
];

/// cmake built-in `find_package()` modules that are satisfied by the
/// toolchain (threads / language runtimes), not external libraries —
/// plus the classic PROGRAM-lookup modules (FindGit, FindDoxygen, …):
/// they resolve against tools on PATH, the same category as `threads`.
pub const CMAKE_BUILTINS: &[&str] = &[
    "threads",
    "python3",
    "pythoninterp",
    "python",
    "cmake",
    "pkgconfig",
    "git",
    "doxygen",
    "perl",
    "flex",
    "bison",
    "swig",
];

/// `AC_CHECK_LIB` / `AC_SEARCH_LIBS` targets that live in libc / the
/// compiler runtime delivered by the toolchain's gcc (the classic
/// libc helper libraries — including the socket-service fallbacks like
/// `inet` that modern libc provides in `libc` itself, so
/// `AC_SEARCH_LIBS(connect, inet)` finds it without any external
/// library).
pub const LIBC_LIBS: &[&str] = &[
    "c", "m", "dl", "pthread", "rt", "intl", "gcc", "gcc_s", "supc++", "socket", "nsl", "resolv",
    "crypt", "inet",
];

fn normalize_name(s: &str) -> String {
    // CMake package names are CamelCase (`ZLIB`, `PNG`); pkg-config and
    // resolution keys are lowercase. Strip nothing else — the matching
    // stays conservative.
    s.trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// scanning entry point
// ---------------------------------------------------------------------------

/// Scan a source tree with the parsers of its own build system.
pub fn scan(src: &Path, bs: BuildSystem) -> Result<DeclaredDeps> {
    let mut out = match bs {
        BuildSystem::Meson => scan_meson(src)?,
        BuildSystem::Cmake => scan_cmake(src)?,
        BuildSystem::Cargo => scan_cargo(src)?,
        BuildSystem::Autotools => scan_autotools(src)?,
        BuildSystem::Make => scan_make(src)?,
    };
    out.required.sort_by(|a, b| a.name_norm.cmp(&b.name_norm));
    out.required.dedup_by(|a, b| a.name_norm == b.name_norm);
    out.optional.sort_by(|a, b| a.name_norm.cmp(&b.name_norm));
    out.optional.dedup_by(|a, b| a.name_norm == b.name_norm);
    // a name declared both required (anywhere) and optional is required
    out.optional
        .retain(|o| !out.required.iter().any(|r| r.name_norm == o.name_norm));
    out.provided_in_tree.sort();
    out.provided_in_tree.dedup();
    Ok(out)
}

/// Walk `dir` (recursively, bounded) collecting files whose names match
/// `pred`. `skip` names prune whole subtrees (build outputs, vendored
/// toolchains' own manifests inside the cache...).
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, want: &dyn Fn(&str) -> bool, skip: &[&str]) {
    const MAX_FILES: usize = 4096;
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let p = e.path();
        if p.is_dir() {
            // never descend into build output dirs / VCS internals
            if name == ".git" || name == "target" || name == "build" || name == "_build" {
                continue;
            }
            if skip.contains(&name.as_str()) {
                continue;
            }
            walk_files(&p, out, want, skip);
        } else if want(&name) {
            out.push(p);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// meson
// ---------------------------------------------------------------------------

fn scan_meson(src: &Path) -> Result<DeclaredDeps> {
    scan_meson_for(src, &crate::mesoneval::current_system())
}

/// Scan a meson tree for an explicit target platform (a meson
/// `system()` name like `linux`, `darwin`, `windows`). Production
/// scans use the machine gitfull runs on ([`mesoneval::current_system`]
/// — gitfull builds natively); tests pin other platforms to prove a
/// gated dependency is excluded when its platform does not match and
/// included when it does.
fn scan_meson_for(src: &Path, system: &str) -> Result<DeclaredDeps> {
    let mut deps = DeclaredDeps::default();

    // wraps first: they self-describe sources and provided names
    let mut wraps: Vec<PathBuf> = Vec::new();
    walk_files(
        &src.join("subprojects"),
        &mut wraps,
        &|n| n.ends_with(".wrap"),
        &[],
    );
    for w in wraps {
        if let Some(wd) = parse_wrap(&w)? {
            deps.wraps.push(wd);
        }
    }
    // vendored subprojects: checked-in trees (directories, not wraps)
    if let Ok(rd) = fs::read_dir(src.join("subprojects")) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                if name != ".wrap" && !name.starts_with('.') {
                    deps.vendored.push(name);
                }
            }
        }
    }

    // dependency() calls in every meson.build, in conditional context:
    // the root file plus every statically-reachable subdir() child is
    // evaluated with ONE shared variable scope in meson's execution
    // order (subdir() runs in the caller's scope — that is how GLib
    // gates `dependency('appleframeworks')` in subdir files on a
    // variable the root computes as darwin-only). foreach loops over
    // literal lists are unrolled per item, so a backend dispatched as
    // `subdir(backend)` behind `get_variable('@0@_enabled'.format(
    // backend))` drops out when its enabled flag is provably false.
    // subprojects/ trees are managed via wraps — their own manifests
    // are not the project's declarations.
    let root = src.join("meson.build");
    let mut options = crate::mesoneval::load_project_options(src);
    if let Ok(text) = fs::read_to_string(&root) {
        crate::mesoneval::overlay_default_options(&text, &mut options);
    }
    let mctx = MesonScanCtx {
        system,
        options: &options,
    };

    let analyzed = if root.is_file() {
        crate::mesoneval::eval_project_with_options(&root, system, options.clone())
    } else {
        crate::mesoneval::ProjectEval::default()
    };
    let seen: BTreeSet<PathBuf> = analyzed.files.iter().map(|f| f.path.clone()).collect();
    for fe in &analyzed.files {
        let origin_base = relativize(&fe.path, src);
        parse_meson_dependency_calls(fe, &origin_base, &mut deps, &mctx);
    }

    // fallback: meson.build files NOT reachable through static
    // subdir() calls (dynamic `subdir(var)` paths, foreach-driven
    // subdirs) — evaluated with an isolated scope (plus the project's
    // option table) so their unconditional declarations are still
    // reported, and their own conditionals are still honored. Anything
    // under a PROVABLY never-entered directory (a subdir() in a
    // provably-dead branch, a dead foreach-dispatch item) is excluded:
    // meson never executes it, so its declarations are not part of any
    // build here.
    let mut excluded = analyzed.excluded;
    let mut builds: Vec<PathBuf> = Vec::new();
    walk_files(src, &mut builds, &|n| n == "meson.build", &["subprojects"]);
    for f in builds {
        if seen.contains(&f) || excluded.iter().any(|d| f.starts_with(d)) {
            continue;
        }
        let fe = crate::mesoneval::eval_isolated_with_options(&f, system, &options, &mut excluded);
        let origin_base = relativize(&f, src);
        parse_meson_dependency_calls(&fe, &origin_base, &mut deps, &mctx);
    }
    Ok(deps)
}

/// `path` relative to the tree root, for `file:line` origins.
fn relativize(p: &Path, src: &Path) -> String {
    p.strip_prefix(src).unwrap_or(p).display().to_string()
}

/// Shared context for one meson scan: the target platform and the
/// project's option table.
struct MesonScanCtx<'a> {
    system: &'a str,
    options: &'a crate::mesoneval::OptTable,
}

/// Extract `dependency('name', ...)` calls (including multi-line ones)
/// with their salient kwargs — but only real ones:
///
/// * only from statements **reachable for the platform the tree was
///   evaluated for** ([`FileEval::statement_active_at`]): a call inside
///   a provably-dead branch (`if host_machine.system() == 'darwin'`
///   when scanning for Linux, `if get_option('docs')` for a
///   default-off option) is not a dependency of this build at all;
/// * never from string literals or `#` comments (string content is
///   not a call);
/// * never from **method-call forms** — `meson.override_dependency(
///   'x', dep)` (this tree PROVIDES module x — the mirror image of a
///   dependency declaration), `subproject.dependency(...)`, … only
///   the standalone global `dependency()` function declares one.
///
/// Each call's `required:` kwarg is evaluated structurally against the
/// project's option table and the variable scope at the call site (see
/// [`crate::mesoneval::requiredness_of`]) — never against names.
fn parse_meson_dependency_calls(
    fe: &crate::mesoneval::FileEval,
    origin_base: &str,
    deps: &mut DeclaredDeps,
    mctx: &MesonScanCtx<'_>,
) {
    let text = &fe.text;
    let bytes = text.as_bytes();
    let strs = string_ranges(text);
    let mut i = 0usize;
    while let Some(rel) = find_sub(bytes, i, b"dependency(") {
        let next = rel + 1;
        // string/comment content: never a call
        if strs.iter().any(|&(a, b)| rel >= a && rel <= b) {
            i = next;
            continue;
        }
        // token-boundary check: a real global `dependency(` declaration
        // NEVER has identifier characters — or a method-call dot —
        // abutting the match. When either is present the match is part
        // of a longer token: a method call (`meson.override_dependency(
        // 'x', dep)`, `sub.dependency(...)`) or a different function
        // whose name merely contains "dependency"
        // (`declare_dependency(...)`) — neither declares a dependency of
        // this build.
        let mut k = rel;
        while k > 0
            && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_')
        {
            k -= 1;
        }
        let is_dot_method = k > 0 && bytes[k - 1] == b'.';
        if k < rel || is_dot_method {
            // the method name extends THROUGH the matched "dependency"
            // ("override_" + "dependency" = "override_dependency")
            if is_dot_method {
                let method = format!("{}dependency", &text[k..rel]);
                if method == "override_dependency" {
                    // meson.override_dependency('mod', dep): THIS tree's
                    // own build provides the module — a
                    // provides-declaration, not a requirement
                    let start = rel + b"dependency(".len();
                    if let Some(close) = call_close_paren(bytes, start) {
                        if fe.statement_active_at(rel) {
                            let call = &text[start..close];
                            if let Some(name) = first_string_arg(call) {
                                let norm = normalize_name(&name);
                                if !norm.is_empty() {
                                    deps.provided_in_tree.push(norm);
                                }
                            }
                        }
                        i = close + 1;
                        continue;
                    }
                }
            }
            // any other longer token or method call: not a global
            // declaration
            i = next;
            continue;
        }
        // the standalone global dependency() declaration
        let start = rel + b"dependency(".len();
        let Some(close) = call_close_paren(bytes, start) else {
            i = next;
            continue;
        };
        let call = &text[start..close];
        // conditional context: only a statement that can execute on
        // the scanned platform declares a dependency of this build
        if fe.statement_active_at(rel) {
            let line = 1 + text[..rel].matches('\n').count();
            let vars = fe.dep_scope_at(rel);
            record_meson_call(
                call,
                &format!("{origin_base}:{line}"),
                deps,
                mctx,
                vars,
            );
        }
        i = close + 1;
    }
}

/// The value of the first argument when it is a string literal (the
/// dependency-name shape; dynamic names are not statically knowable).
fn first_string_arg(call: &str) -> Option<String> {
    let trimmed = call.trim_start();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        let q = trimmed.as_bytes()[0];
        return trimmed[1..]
            .split(q as char)
            .next()
            .map(|s| s.to_string());
    }
    None
}

/// Byte ranges (inclusive of delimiters) of every NON-CODE region —
/// string literals (`'…'`, `"…"`, `'''…'''`, `"""…"""`) and `#`
/// comments — where a `dependency(` match is text, not a call.
fn string_ranges(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'#' {
            // comment runs to end of line — record its extent
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push((start, i.saturating_sub(1)));
            i += 1;
            continue;
        }
        if c != b'\'' && c != b'"' {
            i += 1;
            continue;
        }
        let triple = bytes[i..].starts_with(&[c, c, c][..]);
        let mut j = i + if triple { 3 } else { 1 };
        let mut closed = false;
        while j < bytes.len() {
            if bytes[j] == b'\\' && !triple {
                j += 2;
                continue;
            }
            if triple {
                if bytes[j..].starts_with(&[c, c, c][..]) {
                    j += 3;
                    closed = true;
                    break;
                }
            } else if bytes[j] == c {
                j += 1;
                closed = true;
                break;
            }
            j += 1;
        }
        let end = if closed { j - 1 } else { bytes.len().saturating_sub(1) };
        out.push((i, end));
        i = j.max(i + 1);
    }
    out
}

/// Index of the `)` matching the opening paren just before `start`
/// (`start` points at the first argument byte), skipping string
/// literals — `None` when unbalanced.
fn call_close_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut j = start;
    let mut in_str: Option<u8> = None;
    while j < bytes.len() {
        let c = bytes[j];
        if let Some(q) = in_str {
            if c == b'\\' {
                j += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
        } else if c == b'\'' || c == b'"' {
            in_str = Some(c);
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

fn record_meson_call(
    call: &str,
    origin: &str,
    deps: &mut DeclaredDeps,
    mctx: &MesonScanCtx<'_>,
    vars: Option<&HashMap<String, crate::mesoneval::Val>>,
) {
    // positional arguments: the dependency-name chain, in meson's own
    // fallback order (`dependency('libsystemd', 'libelogind')` tries
    // libsystemd first, libelogind only if that is not found)
    let names = crate::mesoneval::positional_string_names(call);
    // first argument: a string literal (the dependency name)
    let Some(name) = names.first().cloned() else {
        return; // dynamic name (variable/computed): not statically knowable
    };
    if name.is_empty() || MESON_BUILTINS.contains(&name.as_str()) {
        // build-system built-ins (threads, gtest, ...) are satisfied by
        // the toolchain itself: skip entirely
        return;
    }
    // the remaining names of the chain — normalized, built-ins and
    // duplicates dropped, order preserved
    let primary_norm = normalize_name(&name);
    let mut alt_names: Vec<String> = Vec::new();
    for n in &names[1..] {
        let norm = normalize_name(n);
        if norm.is_empty()
            || norm == primary_norm
            || MESON_BUILTINS.contains(&norm.as_str())
            || alt_names.contains(&norm)
        {
            continue;
        }
        alt_names.push(norm);
    }
    let version = extract_kwarg_string(call, "version");
    // meson's own fallback semantics: the first element of
    // `fallback: ['subproject', 'var']` names the wrap that satisfies
    // this dependency when the system lookup fails
    let fallback_subproject = crate::mesoneval::kwarg_first_string(call, "fallback");
    // meson's own required/optional semantics for this call, read
    // structurally: `required: false`, `required: true`, `required:
    // get_option('x')` against the option's declared default, feature
    // coercions, variables at the call site. No `required:` (the
    // default) or an undecidable value stays REQUIRED.
    let (req, why) =
        crate::mesoneval::requiredness_of(call, vars, mctx.options, mctx.system);
    let d = DeclaredDep {
        name: name.clone(),
        name_norm: normalize_name(&name),
        kind: DepKind::PkgConfig,
        required: req == crate::mesoneval::Requiredness::Required,
        optional_why: (!why.is_empty()).then_some(why),
        version,
        origin: origin.to_string(),
        git_url: None,
        fallback_subproject,
        alt_names,
    };
    match req {
        crate::mesoneval::Requiredness::Required => deps.required.push(d),
        crate::mesoneval::Requiredness::Optional => deps.optional.push(d),
        // a feature option resolving to 'disabled': meson skips the
        // lookup entirely — not a dependency of this configuration
        crate::mesoneval::Requiredness::Disabled => {}
    }
}

/// Extract `kwarg: 'value'` (single or double quoted) from a meson call.
fn extract_kwarg_string(call: &str, kwarg: &str) -> Option<String> {
    let lower = call.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(pos) = lower[from..].find(kwarg) {
        let after = pos + from + kwarg.len();
        let rest = &call[after..];
        let rest_t = rest.trim_start();
        if let Some(r) = rest_t.strip_prefix(':') {
            let v = r.trim_start();
            if v.starts_with('\'') || v.starts_with('"') {
                let q = v.as_bytes()[0];
                let s = v[1..].split(q as char).next().unwrap_or("").to_string();
                return Some(s);
            }
        }
        from = after;
    }
    None
}

/// Parse one `subprojects/*.wrap` INI file.
pub fn parse_wrap(path: &Path) -> Result<Option<WrapDep>> {
    let text = match crate::util::read_file_if_exists(path)? {
        Some(t) => t,
        None => return Ok(None),
    };
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut section = String::new();
    let mut git: Option<(String, String)> = None;
    let mut file: Option<(String, Option<String>, Option<String>)> = None;
    let mut provides: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match (section.as_str(), k) {
            ("wrap-git", "url") | ("wrap-git", "repository_url") => {
                git = Some((v.to_string(), "main".to_string()));
            }
            ("wrap-git", "revision") => {
                if let Some(g) = git.as_mut() {
                    g.1 = v.to_string();
                }
            }
            ("wrap-file", "source_url") => {
                file = Some((v.to_string(), None, None));
            }
            ("wrap-file", "source_fallback_url") => {
                if let Some(f) = file.as_mut() {
                    f.1 = Some(v.to_string());
                }
            }
            ("wrap-file", "patch_url") => {
                if let Some(f) = file.as_mut() {
                    f.2 = Some(v.to_string());
                }
            }
            // [provide] — BOTH wrap-db conventions:
            //   dependency_names = foo, bar       (legacy comma list)
            //   foo = foo_dep                    (modern per-line: the KEY
            //                                     is a provided module name,
            //                                     the value the variable it
            //                                     is exposed as — e.g.
            //                                     `libpcre2-8 = libpcre2_8`,
            //                                     `intl = intl_dep`)
            ("provide", "dependency_names") => {
                provides = v
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
            }
            ("provide", "program_names") => {} // find_program provides, not deps
            ("provide", other) => {
                provides.push(other.to_string());
            }
            _ => {}
        }
    }
    if git.is_none() && file.is_none() {
        return Ok(None); // e.g. a bare [provide] section referencing an already-present subproject
    }
    if provides.is_empty() {
        // legacy wraps: the stem is the conventional provided name
        provides = vec![name.clone()];
    }
    Ok(Some(WrapDep {
        name,
        git,
        file,
        provides,
        origin: path.display().to_string(),
    }))
}

// ---------------------------------------------------------------------------
// cmake
// ---------------------------------------------------------------------------

fn scan_cmake(src: &Path) -> Result<DeclaredDeps> {
    let mut deps = DeclaredDeps::default();
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(
        src,
        &mut files,
        &|n| n == "CMakeLists.txt" || n.ends_with(".cmake"),
        &["subprojects", "third_party", "3rdparty"],
    );
    for f in files {
        let text = fs::read_to_string(&f).unwrap_or_default();
        let origin_base = f.strip_prefix(src).unwrap_or(&f).display().to_string();
        parse_cmake_commands(&text, &origin_base, &mut deps);
    }
    Ok(deps)
}

fn parse_cmake_commands(text: &str, origin_base: &str, deps: &mut DeclaredDeps) {
    // strip comments
    let clean: String = text
        .lines()
        .map(|l| match l.find('#') {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let lower = clean.to_ascii_lowercase();
    let mut push =
        |name: &str, kind: DepKind, required: bool, version: Option<String>, line: usize| {
            let name = name.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
            {
                return;
            }
            let norm = normalize_name(name);
            if CMAKE_BUILTINS.contains(&norm.as_str()) {
                return; // toolchain-provided (threads, python interpreter, ...)
            }
            let d = DeclaredDep {
                name: name.to_string(),
                name_norm: norm,
                kind,
                required,
                optional_why: (!required)
                    .then(|| "cmake find_package without REQUIRED".to_string()),
                version,
                origin: format!("{origin_base}:{line}"),
                git_url: None,
                fallback_subproject: None,
                alt_names: Vec::new(),
            };
            if required {
                deps.required.push(d);
            } else {
                deps.optional.push(d);
            }
        };

    // find_package(Name [version] [REQUIRED|OPTIONAL] [COMPONENTS ...])
    let mut idx = 0usize;
    while let Some(pos) = lower[idx..].find("find_package") {
        let abs = idx + pos;
        if let Some(args) = paren_args(&clean, abs) {
            let line = 1 + clean[..abs].matches('\n').count();
            let toks = tokenize_args(&args);
            if let Some(first) = toks.first() {
                if first.parse::<f64>().is_err() {
                    let rest = toks[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>();
                    let required = rest.iter().any(|t| t.eq_ignore_ascii_case("required"));
                    let version = toks
                        .get(1)
                        .filter(|t| t.parse::<f64>().is_ok())
                        .map(|t| t.to_string());
                    push(first, DepKind::CmakePackage, required, version, line);
                }
            }
        }
        idx = abs + 12;
    }

    // find_library(VAR [NAMES] name1 name2 ...)
    idx = 0;
    while let Some(pos) = lower[idx..].find("find_library") {
        let abs = idx + pos;
        if let Some(args) = paren_args(&clean, abs) {
            let line = 1 + clean[..abs].matches('\n').count();
            let toks = tokenize_args(&args);
            // skip leading VAR; NAMES introduces the candidate list; the
            // first candidate is the canonical library name
            let mut names: Vec<String> = Vec::new();
            let mut in_names = false;
            for (i, t) in toks.iter().enumerate() {
                if i == 0 {
                    continue; // output variable
                }
                let tu = t.to_ascii_uppercase();
                if tu == "NAMES" {
                    in_names = true;
                    continue;
                }
                if matches!(
                    tu.as_str(),
                    "HINTS"
                        | "PATHS"
                        | "PATH_SUFFIXES"
                        | "DOC"
                        | "REQUIRED"
                        | "NO_DEFAULT_PATH"
                        | "NAMES_PER_DIR"
                ) {
                    in_names = false;
                    continue;
                }
                if in_names && names.is_empty() {
                    names.push(t.clone());
                }
            }
            // bare form: find_library(VAR name)
            if names.is_empty() && toks.len() >= 2 {
                let t = &toks[1];
                if t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    names.push(t.clone());
                }
            }
            if let Some(n) = names.first() {
                let required = toks.iter().any(|t| t.eq_ignore_ascii_case("required"));
                push(n, DepKind::CmakePackage, required, None, line);
            }
        }
        idx = abs + 12;
    }

    // pkg_check_modules(PREFIX [REQUIRED] [IMPORTED_TARGET] module...)
    idx = 0;
    while let Some(pos) = lower[idx..].find("pkg_check_modules") {
        let abs = idx + pos;
        if let Some(args) = paren_args(&clean, abs) {
            let line = 1 + clean[..abs].matches('\n').count();
            let toks = tokenize_args(&args);
            // module list = trailing tokens; may carry version constraints
            // ("libpng >= 1.6" / "libpng>=1.6") and multiple modules
            let required = toks.iter().any(|t| t.eq_ignore_ascii_case("required"));
            let mut modules: Vec<String> = Vec::new();
            let mut expect_version = false;
            for t in toks.iter().skip(1) {
                if expect_version {
                    expect_version = false;
                    continue; // the version operand of a constraint
                }
                let tu = t.to_ascii_uppercase();
                if tu == "IMPORTED_TARGET"
                    || tu == "REQUIRED"
                    || tu == "QUIET"
                    || tu == "NO_CMAKE_PATH"
                    || tu == "NO_CMAKE_ENVIRONMENT_PATH"
                {
                    continue;
                }
                if matches!(t.as_str(), ">=" | "<=" | "==" | "!=" | ">" | "<" | "=") {
                    expect_version = true;
                    continue;
                }
                // split off an attached version constraint
                let (name, _ver) = split_pkg_constraint(t);
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
                {
                    modules.push(name.to_string());
                }
            }
            for m in modules {
                push(&m, DepKind::PkgConfig, required, None, line);
            }
        }
        idx = abs + 17;
    }
}

/// Text inside the balanced parens that follow a command/macro name
/// starting at `pos` (an optional identifier run and whitespace may
/// separate the name from `(`), or `None` when no `(` follows.
fn paren_args(text: &str, pos: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = pos;
    // skip the command name (identifier-like run)
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
    {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return None;
    }
    i += 1;
    let start = i;
    let mut depth = 1usize;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() && depth > 0 {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
        } else if c == b'"' {
            in_str = Some(c);
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
        }
        i += 1;
    }
    Some(text[start..i.saturating_sub(1).max(start)].to_string())
}

/// Split "zlib >= 1.2" into ("zlib", Some(">= 1.2"))-ish parts.
fn split_pkg_constraint(tok: &str) -> (&str, Option<String>) {
    for op in [">=", "<=", "==", "!=", ">", "<", "="] {
        if let Some(p) = tok.find(op) {
            return (
                &tok[..p],
                Some(format!("{op}{}", tok[p + op.len()..].trim())),
            );
        }
    }
    (tok.trim(), None)
}

fn tokenize_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str: Option<char> = None;
    for c in args.chars() {
        match in_str {
            Some(q) => {
                if c == q {
                    in_str = None;
                    out.push(cur.clone());
                    cur.clear();
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    if !cur.is_empty() {
                        out.push(cur.clone());
                        cur.clear();
                    }
                    in_str = Some(c);
                } else if c.is_whitespace() {
                    if !cur.is_empty() {
                        out.push(cur.clone());
                        cur.clear();
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// cargo
// ---------------------------------------------------------------------------

fn scan_cargo(src: &Path) -> Result<DeclaredDeps> {
    let mut deps = DeclaredDeps::default();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    scan_cargo_toml(&src.join("Cargo.toml"), src, &mut deps, &mut visited)?;
    Ok(deps)
}

fn scan_cargo_toml(
    path: &Path,
    src: &Path,
    deps: &mut DeclaredDeps,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if !visited.insert(path.to_path_buf()) {
        return Ok(()); // cycle in workspace references
    }
    let text = match crate::util::read_file_if_exists(path)? {
        Some(t) => t,
        None => return Ok(()),
    };
    let origin = path.strip_prefix(src).unwrap_or(path).display().to_string();
    let doc: toml::Value = toml::from_str(&text)
        .map_err(|e| GitfullError::Unsupported(format!("in {origin}: {e}")))?;

    // workspace members: each carries its own manifest
    if let Some(members) = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for m in members.iter().filter_map(|v| v.as_str()) {
            let dir = if m == "*" {
                // globs unsupported for member manifests: conservative skip
                continue;
            } else {
                path.parent().unwrap_or(src).join(m)
            };
            scan_cargo_toml(&dir.join("Cargo.toml"), src, deps, visited)?;
        }
    }

    // dependency tables that affect the build
    for table_name in [
        "dependencies",
        "build-dependencies",
        "workspace.dependencies",
        "dev-dependencies",
    ] {
        let table = doc
            .get(table_name.split('.').next().unwrap_or(""))
            .and_then(|t| {
                if table_name.contains('.') {
                    t.get("dependencies")
                } else {
                    Some(t)
                }
            })
            .and_then(|t| t.as_table())
            .cloned()
            .unwrap_or_default();
        let is_dev = table_name == "dev-dependencies";
        // target-specific tables: target.'cfg(...)'.dependencies
        let target_tables: Vec<toml::Table> = doc
            .get("target")
            .and_then(|t| t.as_table())
            .map(|t| {
                t.values()
                    .filter_map(|v| {
                        v.get(table_name.split('.').next().unwrap_or(""))
                            .and_then(|x| x.as_table())
                            .cloned()
                    })
                    .collect()
            })
            .unwrap_or_default();

        for dep_table in std::iter::once(table).chain(target_tables.into_iter()) {
            for (crate_name, spec) in dep_table {
                if crate_name.starts_with('_') {
                    continue; // rename keys etc.
                }
                let git_url = spec
                    .get("git")
                    .and_then(|g| g.as_str())
                    .map(|s| s.to_string());
                let d = DeclaredDep {
                    name: crate_name.clone(),
                    name_norm: crate_name.to_ascii_lowercase(),
                    kind: if git_url.is_some() {
                        DepKind::CrateGit
                    } else {
                        DepKind::CrateRegistry
                    },
                    required: !is_dev,
                    optional_why: is_dev.then(|| "dev-dependency (tests/examples only)".to_string()),
                    version: spec.as_str().map(|s| s.to_string()).or_else(|| {
                        spec.get("version")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    }),
                    origin: origin.clone(),
                    git_url,
                    fallback_subproject: None,
                    alt_names: Vec::new(),
                };
                if is_dev {
                    // dev-dependencies only build tests/examples: reported,
                    // never provisioned
                    deps.optional.push(d);
                } else {
                    deps.required.push(d);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// autotools
// ---------------------------------------------------------------------------

fn scan_autotools(src: &Path) -> Result<DeclaredDeps> {
    let mut deps = DeclaredDeps::default();
    let text = match crate::util::read_file_if_exists(&src.join("configure.ac"))? {
        Some(t) => t,
        None => return Ok(deps), // generated-only trees: nothing to parse
    };
    let origin = "configure.ac";
    let clean: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("dnl"))
        .collect::<Vec<_>>()
        .join("\n");
    let lower = clean.to_ascii_lowercase();

    // PKG_CHECK_MODULES([VAR], [mod1 >= ver mod2])
    let mut idx = 0usize;
    while let Some(pos) = lower[idx..].find("pkg_check_modules") {
        let abs = idx + pos;
        if let Some(args) = m4_args(&clean, abs) {
            let line = 1 + clean[..abs].matches('\n').count();
            // second macro argument holds the module list; tokens may
            // carry whitespace-separated version constraints
            // ("libcurl >= 7.70") — constraint pairs are collapsed onto the
            // preceding module name
            let toks: Vec<&str> = args
                .iter()
                .nth(1)
                .map(|a| a.split_whitespace().collect())
                .unwrap_or_default();
            let mut kept: Vec<(&str, Option<String>)> = Vec::new();
            let mut expect_version = false;
            let mut pending_op: Option<&str> = None;
            for t in toks {
                if expect_version {
                    expect_version = false;
                    if let Some(last) = kept.last_mut() {
                        let op = pending_op.take().unwrap_or(">");
                        last.1 = Some(format!("{op}{t}"));
                    }
                    continue;
                }
                if matches!(t, ">=" | "<=" | "==" | "!=" | ">" | "<" | "=") {
                    expect_version = true;
                    pending_op = Some(t);
                    continue;
                }
                kept.push((t, None));
            }
            for (t, ver) in kept {
                let (name, ver2) = split_pkg_constraint(t);
                let norm = normalize_name(name);
                if norm.is_empty()
                    || !norm
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
                {
                    continue;
                }
                // PKG_CHECK_MODULES(VAR, modules, if-found, if-not-found):
                // a non-empty 4th argument handles failure instead of
                // aborting configure — that is the autoconf "optional" form
                let required = args.len() < 4 || args[3].trim().is_empty();
                let d = DeclaredDep {
                    name: name.to_string(),
                    name_norm: norm,
                    kind: DepKind::PkgConfig,
                    required,
                    optional_why: (!required)
                        .then(|| "PKG_CHECK_MODULES with a not-found handler".to_string()),
                    version: ver.or(ver2.map(|s| s.to_string())),
                    origin: format!("{origin}:{line}"),
                    git_url: None,
                    fallback_subproject: None,
                    alt_names: Vec::new(),
                };
                if required {
                    deps.required.push(d);
                } else {
                    deps.optional.push(d);
                }
            }
        }
        idx = abs + "pkg_check_modules".len();
    }

    // AC_CHECK_LIB([lib], func) and AC_SEARCH_LIBS([func], [lib lib ...])
    for macro_name in ["ac_check_lib", "ac_search_libs"] {
        let mut idx = 0usize;
        while let Some(pos) = lower[idx..].find(macro_name) {
            let abs = idx + pos;
            if let Some(args) = m4_args(&clean, abs) {
                let line = 1 + clean[..abs].matches('\n').count();
                // AC_CHECK_LIB(lib, ...) → arg 0; AC_SEARCH_LIBS(func, libs) → arg 1
                let which = if macro_name == "ac_check_lib" { 0 } else { 1 };
                // AC_CHECK_LIB(lib, func, if-found, if-not-found): a
                // non-empty 4th argument handles absence instead of
                // aborting configure — the same optional form
                // PKG_CHECK_MODULES has (GTK2's `AC_CHECK_LIB(mlib, …,
                // use_mlib=yes, use_mlib=no)` probes mediaLib best-effort)
                let required = if macro_name == "ac_check_lib" {
                    args.len() < 4 || args[3].trim().is_empty()
                } else {
                    // AC_SEARCH_LIBS(func, libs): absence merely leaves
                    // LIBS untouched (non-fatal), but it has no explicit
                    // handler argument — conservative: required
                    true
                };
                let libs: Vec<&str> = args
                    .iter()
                    .nth(which)
                    .map(|a| a.split_whitespace().collect())
                    .unwrap_or_default();
                for l in libs {
                    let l = l.trim().trim_matches(|c| c == '[' || c == ']');
                    let norm = normalize_name(l);
                    if norm.is_empty() || LIBC_LIBS.contains(&norm.as_str()) {
                        continue;
                    }
                    if !norm
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
                    {
                        continue;
                    }
                    let d = DeclaredDep {
                        name: l.to_string(),
                        name_norm: norm,
                        kind: DepKind::AutoconfLib,
                        required,
                        optional_why: (!required)
                            .then(|| "AC_CHECK_LIB with a not-found handler".to_string()),
                        version: None,
                        origin: format!("{origin}:{line}"),
                        git_url: None,
                        fallback_subproject: None,
                        alt_names: Vec::new(),
                    };
                    if required {
                        deps.required.push(d);
                    } else {
                        deps.optional.push(d);
                    }
                }
            }
            idx = abs + macro_name.len();
        }
    }
    Ok(deps)
}

/// Split an m4 macro invocation into its bracketed arguments
/// (`NAME([a], [b]) → ["a", "b"]`). `pos` points at the macro name; the
/// name (and optional whitespace) is skipped before the `(`. m4 `dnl`
/// comments (discard-to-newline — common inside multi-line module
/// lists, as in GTK2's configure.ac) are stripped from each argument.
fn m4_args(text: &str, pos: usize) -> Option<Vec<String>> {
    let bytes = text.as_bytes();
    let mut i = pos;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return None;
    }
    i += 1;
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut depth = 1usize;
    let mut brack = 0i32;
    while i < bytes.len() && depth > 0 {
        let c = bytes[i] as char;
        if c == '[' && brack == 0 {
            brack += 1;
        } else if c == ']' && brack == 1 {
            brack -= 1;
        } else if c == ',' && depth == 1 && brack == 0 {
            args.push(strip_dnl(&cur));
            cur.clear();
        } else if c == '(' && brack == 0 {
            depth += 1;
        } else if c == ')' && brack == 0 {
            depth -= 1;
            if depth == 0 {
                break;
            }
        } else {
            cur.push(c);
        }
        i += 1;
    }
    args.push(strip_dnl(&cur));
    Some(args)
}

/// Strip m4 `dnl` comments (discard to end of line) from a macro
/// argument. `dnl` must be a standalone token (whitespace/bracket
/// delimited) — `libdnlfoo` is a name, not a comment. Multi-line
/// arguments collapse to single-line text with the comments gone.
fn strip_dnl(s: &str) -> String {
    let mut out = String::new();
    for l in s.lines() {
        let b = l.as_bytes();
        let mut cut = b.len();
        let mut i = 0usize;
        while i + 3 <= b.len() {
            if &b[i..i + 3] == b"dnl" {
                let before_ok = i == 0
                    || b[i - 1].is_ascii_whitespace()
                    || b[i - 1] == b'[';
                let after = i + 3;
                let after_ok = after >= b.len()
                    || b[after].is_ascii_whitespace()
                    || b[after] == b']';
                if before_ok && after_ok {
                    cut = i;
                    break;
                }
            }
            i += 1;
        }
        out.push_str(l[..cut].trim_end());
        out.push(' ');
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// make
// ---------------------------------------------------------------------------

fn scan_make(src: &Path) -> Result<DeclaredDeps> {
    let mut deps = DeclaredDeps::default();
    let text = match crate::util::read_file_if_exists(&src.join("Makefile"))? {
        Some(t) => t,
        None => return Ok(deps),
    };
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("");
        if !line.contains("pkg-config") && !line.contains("pkgconfig") {
            continue;
        }
        // conservative: tokenize the line with make syntax ($, parens,
        // quotes) treated as separators, then keep the tokens FOLLOWING the
        // pkg-config invocation — `$(shell pkg-config --cflags foo)` /
        // `pkg-config --libs foo bar`. Make function keywords (shell, ...)
        // and variables precede the invocation and are excluded naturally.
        let sanitized: String = line
            .chars()
            .map(|c| {
                if matches!(c, '(' | ')' | '$' | '"' | '\'') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        let toks: Vec<&str> = sanitized.split_whitespace().collect();
        let Some(pc) = toks
            .iter()
            .rposition(|t| *t == "pkg-config" || *t == "pkgconfig")
        else {
            continue;
        };
        for tok in toks.iter().skip(pc + 1) {
            let tok = *tok;
            if tok.contains('=')
                || tok.contains('>')
                || tok.contains('<')
                || tok.contains('/')
                || tok.starts_with('-')
                || !tok
                    .chars()
                    // module names include digits (glib-2.0, gtk4,
                    // sdl3, gee-0.8, ...) — letters+digits+separators
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
            {
                continue; // flags, redirects — not module names
            }
            let (name, _) = split_pkg_constraint(tok);
            let norm = normalize_name(name);
            if norm.is_empty()
                || !norm
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
            {
                continue;
            }
            deps.required.push(DeclaredDep {
                name: name.to_string(),
                name_norm: norm,
                kind: DepKind::PkgConfig,
                required: true,
                optional_why: None,
                version: None,
                origin: format!("Makefile:{}", i + 1),
                git_url: None,
                fallback_subproject: None,
                alt_names: Vec::new(),
            });
        }
    }
    Ok(deps)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn find_sub(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// tests: several unrelated fixture shapes, none of them special-cased
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gitfull-depgraph-{label}-{}-{}",
            std::process::id(),
            label
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(deps: &[DeclaredDep]) -> Vec<String> {
        let mut v: Vec<String> = deps.iter().map(|d| d.name_norm.clone()).collect();
        v.sort();
        v
    }

    // ---- meson: a GNOME-style project with wraps + vendored subprojects --

    #[test]
    fn meson_dependency_calls_multi_shape() {
        let d = tmpdir("meson");
        fs::write(
            d.join("meson.build"),
            r#"project('gnome-style-app', 'c',
  version: '1.4.2',
  default_options: ['warning_level=2'])

glib_dep = dependency('glib-2.0', version: '>=2.50.0')
png_dep = dependency('libpng')
threads = dependency('threads')            # build-system built-in: skipped
gettext = dependency('gettext', required: false)
zlib = dependency('zlib', version: '>=1.2',
                  required: true,
                  static: false)
some_var = dependency(
  'harfbuzz',
)
cc = meson.get_compiler('c')
"#,
        )
        .unwrap();
        // a subdirectory manifest must be scanned too
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(
            d.join("src/meson.build"),
            "cairo_dep = dependency('cairo')\nxml = dependency('libxml-2.0', version: '>= 2.9')\n",
        )
        .unwrap();

        let deps = scan(&d, BuildSystem::Meson).unwrap();
        assert_eq!(
            names(&deps.required),
            // harfbuzz deduped? no — appears once. Sorted set:
            vec![
                "cairo",
                "glib-2.0",
                "harfbuzz",
                "libpng",
                "libxml-2.0",
                "zlib"
            ]
        );
        // optional deps are reported, never provisioned
        assert_eq!(names(&deps.optional), vec!["gettext"]);
        // version constraints are captured for the report
        let zlib = deps.required.iter().find(|x| x.name == "zlib").unwrap();
        assert_eq!(zlib.version.as_deref(), Some(">=1.2"));
        assert!(zlib.origin.contains("meson.build"), "{}", zlib.origin);
        let glib = deps.required.iter().find(|x| x.name == "glib-2.0").unwrap();
        assert_eq!(glib.version.as_deref(), Some(">=2.50.0"));
        // threads is a meson built-in: never even reported
        assert!(!deps
            .required
            .iter()
            .chain(deps.optional.iter())
            .any(|x| x.name == "threads"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_multi_name_fallback_chain_is_recorded_in_order() {
        let d = tmpdir("meson-multiname");
        fs::write(
            d.join("meson.build"),
            r#"project('sd-client', 'c')

# the classic multi-name fallback: meson tries libsystemd first, and
# only if that is not found does it try libelogind (non-systemd/musl
# systems). gitfull records the whole chain, in declared order.
login_dep = dependency('libsystemd', 'libelogind', version: '>=239')
# optional multi-name: the chain carries regardless of requiredness
net_dep = dependency('opt-primary-fixture', 'opt-fallback-fixture', required: false)
# kwargs are never names; built-ins and duplicates in the chain are
# dropped; normalization lowercases (cmake-style CamelCase never
# appears in meson, but be conservative)
mixed = dependency('Foo', 'foo', 'threads', 'Bar')
# a dynamic first name declares nothing statically knowable — the whole
# call is skipped, exactly like the single-name form
dyn = dependency(some_var, 'libelogind')
# fallback: ['subproject', 'var'] kwarg still names the wrap
wrap_dep = dependency('xmlb', 'libxmlb', fallback: ['libxmlb', 'xmlb_dep'])
"#,
        )
        .unwrap();

        let deps = scan(&d, BuildSystem::Meson).unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["foo", "libsystemd", "xmlb"]
        );
        assert_eq!(names(&deps.optional), vec!["opt-primary-fixture"]);

        let find = |n: &str| {
            deps.required
                .iter()
                .chain(deps.optional.iter())
                .find(|x| x.name_norm == n)
                .unwrap_or_else(|| panic!("missing {n}"))
        };
        // the primary name leads; the fallback chain preserves declared
        // order, deduplicates, and drops build-system built-ins
        let sd = find("libsystemd");
        assert_eq!(sd.alt_names, vec!["libelogind"]);
        // version kwarg survives alongside the chain
        assert_eq!(sd.version.as_deref(), Some(">=239"));
        // dedup of the primary inside the chain + built-in dropping
        let foo = find("foo");
        assert_eq!(foo.alt_names, vec!["bar"]);
        assert_eq!(foo.name, "Foo");
        // the fallback kwarg is still read (meson's wrap semantics)
        let xmlb = find("xmlb");
        assert_eq!(xmlb.fallback_subproject.as_deref(), Some("libxmlb"));
        assert_eq!(xmlb.alt_names, vec!["libxmlb"]);
        // an optional multi-name call keeps its chain too (requiredness
        // covers the whole call, meson evaluates `required:` once)
        let opt = find("opt-primary-fixture");
        assert_eq!(opt.alt_names, vec!["opt-fallback-fixture"]);
        assert!(opt.optional_why.as_deref().unwrap().contains("required: false"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_wraps_modern_provide_convention() {
        // the modern wrap-db [provide] form: `module = variable` per
        // line (e.g. glib's pcre2.wrap provides libpcre2-8/16/32/posix;
        // proxy-libintl.wrap provides intl). The KEY is the provided
        // dependency name; the value is the subproject variable it is
        // exposed as (irrelevant for source resolution).
        let d = tmpdir("wraps-modern");
        fs::write(d.join("meson.build"), "project('w')\nz = dependency('zlib')\n").unwrap();
        fs::create_dir_all(d.join("subprojects")).unwrap();
        fs::write(
            d.join("subprojects/pcre2.wrap"),
            "[wrap-file]\ndirectory = pcre2-10.46\nsource_url = https://example.com/pcre2.tar.bz2\n\n[provide]\nlibpcre2-8 = libpcre2_8\nlibpcre2-16 = libpcre2_16\nlibpcre2-32 = libpcre2_32\nlibpcre2-posix = libpcre2_posix\n",
        )
        .unwrap();
        fs::write(
            d.join("subprojects/proxy-libintl.wrap"),
            "[wrap-file]\ndirectory = proxy-libintl-0.5\nsource_url = https://example.com/libintl.tar.gz\n\n[provide]\nintl = intl_dep\n",
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Meson).unwrap();
        let p = deps.wraps.iter().find(|w| w.name == "pcre2").unwrap();
        assert_eq!(
            p.provides,
            vec![
                "libpcre2-8".to_string(),
                "libpcre2-16".to_string(),
                "libpcre2-32".to_string(),
                "libpcre2-posix".to_string(),
            ]
        );
        let i = deps.wraps.iter().find(|w| w.name == "proxy-libintl").unwrap();
        assert_eq!(i.provides, vec!["intl".to_string()]);
        // wrap-provided names are queryable — a dependency('libpcre2-8')
        // resolves through the wrap layer, never through search
        assert!(deps.wrap_provided().contains("libpcre2-8"));
        assert!(deps.wrap_provided().contains("intl"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_fallback_kwarg_names_the_wrap() {
        // meson's fallback semantics: `dependency('xmlb', fallback:
        // ['libxmlb', 'libxmlb_dep'])` resolves through the libxmlb
        // wrap when the system lookup fails — the kwarg names THE
        // subproject for this dependency name
        let d = tmpdir("fallback-kwarg");
        fs::write(
            d.join("meson.build"),
            "project('a')\nxmlb_dep = dependency('xmlb', version: '>=0.3.14', fallback: ['libxmlb', 'libxmlb_dep'], default_options: ['gtkdoc=false'])\n",
        )
        .unwrap();
        fs::create_dir_all(d.join("subprojects")).unwrap();
        fs::write(
            d.join("subprojects/libxmlb.wrap"),
            "[wrap-git]\ndirectory = libxmlb\nurl = https://github.com/hughsie/libxmlb.git\nrevision = main\n",
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Meson).unwrap();
        let x = deps.required.iter().find(|x| x.name == "xmlb").unwrap();
        assert_eq!(
            x.fallback_subproject.as_deref(),
            Some("libxmlb"),
            "the fallback kwarg's first element names the subproject"
        );
        // version capture works alongside the fallback kwarg
        assert_eq!(x.version.as_deref(), Some(">=0.3.14"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_iconv_is_a_libc_builtin() {
        // iconv lives in the C library on every platform gitfull builds
        // against (glib's non-windows branch declares it) — the same
        // resolution category as `threads`: the toolchain provides it
        let d = tmpdir("iconv-builtin");
        fs::write(
            d.join("meson.build"),
            "project('i', 'c')\nif host_machine.system() != 'windows'\n  libiconv = dependency('iconv')\nendif\nz = dependency('zlib')\n",
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Meson).unwrap();
        assert_eq!(names(&deps.required), vec!["zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_ternary_and_require_chain_kill_default_off_features() {
        // the libxml2 shape: feature options chained through ternaries
        // and .require() — with 'history'/'readline' defaulting to
        // 'auto', BOTH want flags resolve false and the shell-history
        // deps drop out of a default build
        let d = tmpdir("ternary-require");
        fs::write(
            d.join("meson.options"),
            "option('history', type: 'feature', description: 'x')\noption('readline', type: 'feature', description: 'x')\noption('legacy', type: 'feature', value: 'disabled', description: 'x')\n",
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('xml-like', 'c')
feature = get_option('readline')
want_readline = get_option('history').enabled() ? feature.allowed() : feature.enabled()
feature = get_option('history') \
  .require(want_readline, error_message: 'history requires readline')
want_history = feature.enabled()

if want_readline
    readline_dep = dependency('readline')
endif
if want_history
    history_dep = dependency('history')
endif
zlib_dep = dependency('zlib')
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // auto (not enabled) propagates through the ternary's else arm
        // and the .require() chain: both features land disabled
        assert_eq!(names(&deps.required), vec!["zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_ternary_with_decided_condition_selects_branch() {
        // a ternary whose condition IS statically known picks its branch
        let d = tmpdir("ternary-pick");
        fs::write(
            d.join("meson.build"),
            r#"project('t', 'c')
want_thing = host_machine.system() == 'linux' ? true : false
if want_thing
  a = dependency('libaaa')
endif
other = get_option('prefix') != 'x' ? 'b' : 'c'
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libaaa"]);
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(names(&deps.required), Vec::<String>::new());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn autotools_dnl_comments_and_libc_socket_fallbacks() {
        // the GTK2 configure.ac shape: multi-line PKG_CHECK_MODULES
        // module lists with `dnl` comments inside, and the classic
        // AC_SEARCH_LIBS socket fallbacks that modern libc provides
        let d = tmpdir("gtk2-ac");
        fs::write(
            d.join("configure.ac"),
            r#"AC_INIT([gtk-like], [2.24.33])
PKG_CHECK_MODULES(BASE_DEPENDENCIES,
  [glib-2.0 >= glib_required_version dnl
   atk >= atk_required_version dnl
   pango >= pango_required_version])
AC_SEARCH_LIBS(gethostent, nsl)
AC_SEARCH_LIBS(setsockopt, socket)
AC_SEARCH_LIBS(connect, inet)
AC_CHECK_LIB(w, iswalnum, GDK_WLIBS=-lw)
AC_CHECK_LIB(mlib, mlib_ImageSetStruct, use_mlib=yes, use_mlib=no)
AC_CHECK_LIB(papi, papiServiceCreate, have_papi=yes, have_papi=no)
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Autotools).unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["atk", "glib-2.0", "pango", "w"],
            "dnl comments must vanish; nsl/socket/inet are libc; the 'w' \
             wide-char fallback lib stays declared (3-arg probe, no handler)"
        );
        // the 4-arg AC_CHECK_LIB form carries a not-found handler
        // (use_mlib=no / have_papi=no): best-effort probes, optional
        assert_eq!(names(&deps.optional), vec!["mlib", "papi"]);
        let mlib = deps.optional.first().unwrap();
        assert_eq!(
            mlib.optional_why.as_deref(),
            Some("AC_CHECK_LIB with a not-found handler")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_wraps_and_vendored_subprojects() {
        let d = tmpdir("wraps");
        fs::write(
            d.join("meson.build"),
            "project('w')\nz = dependency('zlib')\n",
        )
        .unwrap();
        fs::create_dir_all(d.join("subprojects")).unwrap();
        // wrap-git with a [provide] section (modern wrap-db style)
        fs::write(
            d.join("subprojects/zlib.wrap"),
            r#"[wrap-git]
url = https://gitlab.gnome.org/GNOME/zlib-ng.git
revision = main
depth = 1

[provide]
dependency_names = zlib, zlib-ng
"#,
        )
        .unwrap();
        // wrap-file (legacy tarball style)
        fs::write(
            d.join("subprojects/somelib.wrap"),
            r#"[wrap-file]
directory = somelib-2.3
source_url = https://example.com/somelib-2.3.tar.gz
source_filename = somelib-2.3.tar.gz
source_hash = abc
patch_url = https://example.com/somelib-2.3-patch.tar.gz
"#,
        )
        .unwrap();
        // vendored subproject: a checked-in tree, not a wrap
        fs::create_dir_all(d.join("subprojects/vendored-thing")).unwrap();
        fs::write(
            d.join("subprojects/vendored-thing/meson.build"),
            "project('v')\n",
        )
        .unwrap();

        let deps = scan(&d, BuildSystem::Meson).unwrap();
        assert_eq!(deps.wraps.len(), 2, "{:?}", deps.wraps);
        let z = deps.wraps.iter().find(|w| w.name == "zlib").unwrap();
        assert_eq!(
            z.git.clone(),
            Some((
                "https://gitlab.gnome.org/GNOME/zlib-ng.git".to_string(),
                "main".to_string()
            ))
        );
        assert_eq!(z.provides, vec!["zlib".to_string(), "zlib-ng".to_string()]);
        let s = deps.wraps.iter().find(|w| w.name == "somelib").unwrap();
        assert_eq!(
            s.file.as_ref().map(|(u, fb, p)| (
                u.as_str(),
                fb.clone(),
                p.clone(),
            )),
            Some((
                "https://example.com/somelib-2.3.tar.gz",
                None,
                Some("https://example.com/somelib-2.3-patch.tar.gz".to_string())
            ))
        );
        // vendored dirs are recorded (nothing must be fetched for them)
        assert!(deps.vendored.contains(&"vendored-thing".to_string()));
        // wrap-provided names are queryable
        assert!(deps.wrap_provided().contains("zlib"));
        let _ = fs::remove_dir_all(&d);
    }

    // ---- meson: platform-conditional dependency() calls --------------------
    //
    // The same fixture is scanned for several target platforms: a gated
    // dependency must disappear when the platform does not match and
    // reappear when it does — proving the exclusion follows meson's
    // conditional semantics, not any dependency NAME.

    fn write_platform_gated_fixture(d: &Path) {
        fs::write(
            d.join("meson.build"),
            r#"project('cross-app', 'c')

host_system = host_machine.system()

if host_machine.system() == 'darwin'
  framework_dep = dependency('appleframeworks', modules : ['Foundation', 'CoreFoundation'])
endif

if build_machine.system() == 'windows'
  dwrite_dep = dependency('dwrite')
endif

if target_machine.system() not in ['windows', 'darwin']
  posix_dep = dependency('libudev')
endif

if host_system == 'linux'
  systemd_dep = dependency('libsystemd')
endif

always = dependency('zlib')
"#,
        )
        .unwrap();
    }

    #[test]
    fn meson_platform_gated_deps_excluded_on_linux() {
        let d = tmpdir("meson-gated-linux");
        write_platform_gated_fixture(&d);
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libsystemd", "libudev", "zlib"]);
        // the whole point: darwin/windows-gated names never surface
        for gone in ["appleframeworks", "dwrite"] {
            assert!(
                !deps
                    .required
                    .iter()
                    .chain(deps.optional.iter())
                    .any(|x| x.name_norm == gone),
                "{gone} must not appear on a Linux scan"
            );
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_platform_gated_deps_included_on_matching_platform() {
        // the SAME fixture, scanned as its gated platforms: each gated
        // dependency returns exactly when its condition holds — this is
        // what proves the exclusion is conditional, not name-based
        let d = tmpdir("meson-gated-darwin");
        write_platform_gated_fixture(&d);
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(names(&deps.required), vec!["appleframeworks", "zlib"]);
        let _ = fs::remove_dir_all(&d);

        let d = tmpdir("meson-gated-windows");
        write_platform_gated_fixture(&d);
        let deps = scan_meson_for(&d, "windows").unwrap();
        assert_eq!(names(&deps.required), vec!["dwrite", "zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_if_elif_else_chain_selects_platform_branch() {
        let chain = r#"project('chained', 'c')
host_system = host_machine.system()
if host_system == 'darwin'
  a = dependency('appleframeworks')
elif host_system == 'windows'
  w = dependency('dwrite')
elif host_system == 'linux'
  l = dependency('libudev')
else
  o = dependency('libgen')
endif
base = dependency('zlib')
"#;
        for (system, expected) in [
            ("linux", vec!["libudev", "zlib"]),
            ("windows", vec!["dwrite", "zlib"]),
            ("darwin", vec!["appleframeworks", "zlib"]),
            // no branch matched: the else branch is the live one
            ("freebsd", vec!["libgen", "zlib"]),
        ] {
            let d = tmpdir("meson-chain");
            fs::write(d.join("meson.build"), chain).unwrap();
            let deps = scan_meson_for(&d, system).unwrap();
            assert_eq!(names(&deps.required), expected, "system={system}");
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn meson_unknown_conditions_stay_included() {
        // conditions gitfull cannot decide statically (options, compiler
        // probes) are conservatively reachable: the branch MIGHT run
        let d = tmpdir("meson-unknown");
        fs::write(
            d.join("meson.build"),
            r#"project('unknowns', 'c')
opt = get_option('feature')
if opt.enabled()
  a = dependency('libextra')
endif
if get_option('other').disabled()
  b = dependency('libother')
endif
cc = meson.get_compiler('c')
if cc.compiles('int main(){return 0;}')
  c = dependency('libprobe')
endif
d = dependency('zlib')
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["libextra", "libother", "libprobe", "zlib"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_else_of_unknown_condition_stays_included() {
        // an undecided `if` means BOTH branches might run — the else of
        // an unknown condition must never be dropped
        let d = tmpdir("meson-else-unknown");
        fs::write(
            d.join("meson.build"),
            r#"project('elseu', 'c')
mode = get_option('backend')
if mode == 'gtk'
  g = dependency('gtk4')
else
  o = dependency('qt6')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["gtk4", "qt6"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_platform_flag_propagates_across_subdir_scope() {
        // the shape real upstreams use (GLib): the ROOT file computes a
        // platform flag; subdir()'d files — which meson executes in the
        // SAME variable scope — gate framework dependencies on it. The
        // assignment inside the darwin block must be ignored on Linux,
        // so the subdir's `if have_fw` is provably dead there.
        let root = r#"project('scoped', 'c')
host_system = host_machine.system()
have_fw = false
if host_system == 'darwin'
  have_fw = true
endif
subdir('src')
base = dependency('zlib')
"#;
        let child = r#"if have_fw
  fw_dep = dependency('appleframeworks', modules : ['Foundation'])
endif
plain = dependency('json-c')
"#;
        let d = tmpdir("meson-subdir-scope");
        fs::write(d.join("meson.build"), root).unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/meson.build"), child).unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["json-c", "zlib"]);
        let _ = fs::remove_dir_all(&d);

        // and on darwin the flag flips true and the dep reappears —
        // again: conditional reasoning, not a name list
        let d = tmpdir("meson-subdir-scope-darwin");
        fs::write(d.join("meson.build"), root).unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/meson.build"), child).unwrap();
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["appleframeworks", "json-c", "zlib"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_and_or_not_tri_state_with_unknown_options() {
        // cairo/harfbuzz shape: platform check AND-ed with an option
        // probe. On a non-matching platform the whole conjunction is
        // provably false; on the matching platform it stays unknown
        // (reachable).
        let d = tmpdir("meson-tri");
        fs::write(
            d.join("meson.build"),
            r#"project('tri', 'c')
host_system = host_machine.system()
if host_system == 'darwin' and not get_option('quartz').disabled()
  q_dep = dependency('appleframeworks')
endif
if host_system == 'linux' or host_system == 'freebsd'
  un = dependency('libunwind')
endif
if not host_system == 'windows'
  n = dependency('libnih')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libnih", "libunwind"]);
        let _ = fs::remove_dir_all(&d);

        let d = tmpdir("meson-tri-darwin");
        fs::write(
            d.join("meson.build"),
            r#"project('tri', 'c')
host_system = host_machine.system()
if host_system == 'darwin' and not get_option('quartz').disabled()
  q_dep = dependency('appleframeworks')
endif
if host_system == 'linux' or host_system == 'freebsd'
  un = dependency('libunwind')
endif
if not host_system == 'windows'
  n = dependency('libnih')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(
            names(&deps.required),
            // appleframeworks back: True and not Unknown -> Unknown -> reachable
            vec!["appleframeworks", "libnih"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_array_contains_gating() {
        let body = r#"project('contains', 'c')
host_system = host_machine.system()
unixy = ['linux', 'freebsd', 'openbsd']
if unixy.contains(host_system)
  u = dependency('libudev')
endif
if not unixy.contains(host_system)
  n = dependency('appleframeworks')
endif
"#;
        let d = tmpdir("meson-contains");
        fs::write(d.join("meson.build"), body).unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libudev"]);
        let _ = fs::remove_dir_all(&d);

        let d = tmpdir("meson-contains-darwin");
        fs::write(d.join("meson.build"), body).unwrap();
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(names(&deps.required), vec!["appleframeworks"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_dead_branch_reassignment_does_not_leak() {
        // GLib's ios->darwin alias: an assignment inside a dead branch
        // must not leak into the live scope (on ios the alias applies
        // and darwin-gated deps activate — on linux it never does)
        let body = r#"project('alias', 'c')
host_system = host_machine.system()
if host_system == 'ios'
  host_system = 'darwin'
endif
if host_system == 'darwin'
  fw = dependency('appleframeworks')
endif
z = dependency('zlib')
"#;
        for (system, expected) in [
            ("linux", vec!["zlib"]),
            ("darwin", vec!["appleframeworks", "zlib"]),
            ("ios", vec!["appleframeworks", "zlib"]),
        ] {
            let d = tmpdir("meson-alias");
            fs::write(d.join("meson.build"), body).unwrap();
            let deps = scan_meson_for(&d, system).unwrap();
            assert_eq!(names(&deps.required), expected, "system={system}");
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn meson_multiline_strings_are_not_statements() {
        // compiler-probe strings (GLib shape) contain fake build code —
        // `dependency()` and `#endif` inside them are string content
        let d = tmpdir("meson-strings");
        fs::write(
            d.join("meson.build"),
            r#"project('strs', 'c')
host_system = host_machine.system()
probe = cc.compiles('''#include <stdio.h>
dependency('stringcontent')
#if host_system == 'darwin'
#endif''',
  name : 'stdio probe')
if host_system == 'linux'
  real = dependency('libxml-2.0')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libxml-2.0"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_dynamic_subdir_still_scanned_with_own_conditionals() {
        // foreach-driven subdir paths cannot be followed statically:
        // the child is scanned with an isolated scope — unconditional
        // deps still reported, its own platform conditionals honored
        let d = tmpdir("meson-dyn-subdir");
        fs::write(
            d.join("meson.build"),
            r#"project('dyn', 'c')
foreach p : ['models']
  subdir(p)
endforeach
base = dependency('zlib')
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("models")).unwrap();
        fs::write(
            d.join("models/meson.build"),
            "m = dependency('json-c')\nif host_machine.system() == 'darwin'\n  fw = dependency('appleframeworks')\nendif\n",
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["json-c", "zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    // ---- meson: required/optional semantics of dependency() calls ----------
    //
    // meson's own `required:` keyword (and the option gates around the
    // call) decide whether a dependency blocks a default build. Every
    // fixture here is scanned on linux with the project's DECLARED
    // option defaults — the configuration gitfull provisions.

    #[test]
    fn meson_required_kwarg_literal_shapes() {
        // every literal spelling meson uses, including the spaced
        // `required : false` form and multi-line calls with comments
        let d = tmpdir("meson-req-literals");
        fs::write(
            d.join("meson.build"),
            r#"project('literals', 'c')
a = dependency('libaaa')                                  # no kwarg: required
b = dependency('libbbb', required: false)
c = dependency('libccc', required : false)                # spaced form
d = dependency('libddd', required: true)
e = dependency('libeee',
   required: false)                                       # multi-line
f = dependency('libfff',   # a comment inside the call
   required: false)        # ...and after it
g = dependency('libggg', version: '>=1.0', required: false)
h = dependency('libhhh', required: false, version: '>=2.0')
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libaaa", "libddd"]);
        assert_eq!(
            names(&deps.optional),
            vec!["libbbb", "libccc", "libeee", "libfff", "libggg", "libhhh"]
        );
        // the optional classification carries its manifest reason
        let b = deps.optional.iter().find(|x| x.name == "libbbb").unwrap();
        assert_eq!(b.optional_why.as_deref(), Some("required: false"));
        // version capture is unaffected by the required: kwarg
        let g = deps.optional.iter().find(|x| x.name == "libggg").unwrap();
        assert_eq!(g.version.as_deref(), Some(">=1.0"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_required_get_option_resolves_against_declared_defaults() {
        // `required: get_option('x')` resolves against the option's own
        // declared default: booleans true/false, features
        // enabled/auto/disabled, plus the .disable_auto()/.enable_auto()
        // coercions — each a different verdict
        let d = tmpdir("meson-req-options");
        fs::write(
            d.join("meson.options"),
            r#"option('f-enabled', type: 'feature', value: 'enabled', description: 'x')
option('f-auto', type: 'feature', value: 'auto', description: 'x')
option('f-disabled', type: 'feature', value: 'disabled', description: 'x')
option('b-on', type: 'boolean', value: true, description: 'x')
option('b-off', type: 'boolean', value: false, description: 'x')
"#,
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('opts', 'c')
a = dependency('libaaa', required: get_option('f-enabled'))
b = dependency('libbbb', required: get_option('f-auto'))
c = dependency('libccc', required: get_option('f-disabled'))
dd = dependency('libddd', required: get_option('f-auto').disable_auto())
e = dependency('libeee', required: get_option('f-auto').enable_auto())
f = dependency('libfff', required: get_option('b-on'))
g = dependency('libggg', required: get_option('b-off'))
h = dependency('libhhh')                                # no kwarg: required
i = dependency('libiii', required: get_option('undeclared-option'))
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // required: feature 'enabled', auto coerced to 'enabled',
        // boolean true, no kwarg, and an UNDECLARED option (undecidable
        // stays required — the one-sided rule)
        assert_eq!(
            names(&deps.required),
            vec!["libaaa", "libeee", "libfff", "libhhh", "libiii"]
        );
        // optional: feature 'auto' (best-effort) and boolean false
        assert_eq!(names(&deps.optional), vec!["libbbb", "libggg"]);
        // dropped entirely: feature 'disabled' (meson skips the lookup)
        // and auto coerced to 'disabled'
        for gone in ["libccc", "libddd"] {
            assert!(
                !deps
                    .required
                    .iter()
                    .chain(deps.optional.iter())
                    .any(|x| x.name_norm == gone),
                "{gone} must not appear: its lookup is skipped entirely"
            );
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_default_off_option_gates_kill_the_branch() {
        // the AppStream/GTK shape: `if get_option('x')` around the call.
        // A default-false boolean makes the branch provably dead — the
        // dependency is not part of a default build AT ALL (same
        // treatment as a platform-gated call in a dead branch); a
        // default-true option keeps it, and `not get_option('x')`
        // selects the other arm.
        let d = tmpdir("meson-opt-gates");
        fs::write(
            d.join("meson.options"),
            r#"option('docs', type: 'boolean', value: false, description: 'x')
option('install-tools', type: 'boolean', value: true, description: 'x')
"#,
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('gated', 'c')
if get_option('docs')
  gtkdoc = dependency('gtk-doc')
endif
if get_option('install-tools')
  tools = dependency('libtools')
endif
if not get_option('docs')
  alt = dependency('libalt')
endif
if get_option('undeclared')
  unknown = dependency('libunknown')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libalt", "libtools", "libunknown"]);
        // gtk-doc sits behind a default-off option: gone, not optional
        assert!(
            !deps
                .required
                .iter()
                .chain(deps.optional.iter())
                .any(|x| x.name_norm == "gtk-doc"),
            "default-off option gate must exclude the dependency entirely"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_feature_option_predicates_gate_branches() {
        // feature options carry their own predicates; against the
        // declared defaults each is statically decidable
        let d = tmpdir("meson-feature-gates");
        fs::write(
            d.join("meson.options"),
            r#"option('f-enabled', type: 'feature', value: 'enabled', description: 'x')
option('f-auto', type: 'feature', value: 'auto', description: 'x')
option('f-disabled', type: 'feature', value: 'disabled', description: 'x')
"#,
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('featgates', 'c')
if get_option('f-auto').allowed()
  a = dependency('libaaa')
endif
if get_option('f-disabled').enabled()
  b = dependency('libbbb')
endif
if get_option('f-enabled').disabled()
  c = dependency('libccc')
endif
if not get_option('f-disabled').allowed()
  d = dependency('libddd')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // allowed() on auto: true. enabled() on disabled: false (dead).
        // disabled() on enabled: false (dead). not allowed() on
        // disabled: true (live).
        assert_eq!(names(&deps.required), vec!["libaaa", "libddd"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_required_kwarg_via_variable_at_call_site() {
        // the libsoup/harfbuzz shape: the feature value is stored in a
        // variable lines above the call (`gssapi_opt = get_option(...)`
        // -> `required: gssapi_opt`); a plain boolean variable works the
        // same way through the live-scope snapshot
        let d = tmpdir("meson-req-var");
        fs::write(
            d.join("meson.options"),
            r#"option('gssapi', type: 'feature', value: 'auto', description: 'x')
"#,
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('vars', 'c')
gssapi_opt = get_option('gssapi')
if not gssapi_opt.disabled()
  gssapi = dependency('libgssapi', required: gssapi_opt)
endif
force_off = false
x = dependency('libforceoff', required: force_off)
force_on = true
y = dependency('libforceon', required: force_on)
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // auto feature via variable: optional (best-effort); the branch
        // itself stays live (not .disabled() is true for auto)
        assert_eq!(names(&deps.optional), vec!["libforceoff", "libgssapi"]);
        assert_eq!(names(&deps.required), vec!["libforceon"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_project_default_options_override_declared_defaults() {
        // project(default_options: ['k=v']) sets the defaults meson
        // applies for THIS project, overriding the option definition's
        // own value
        let d = tmpdir("meson-default-options");
        fs::write(
            d.join("meson.options"),
            r#"option('flag', type: 'boolean', value: false, description: 'x')
option('feat', type: 'feature', value: 'auto', description: 'x')
"#,
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            r#"project('overridden', 'c',
  default_options: ['flag=true', 'feat=disabled'])
a = dependency('libaaa', required: get_option('flag'))
b = dependency('libbbb', required: get_option('feat'))
if get_option('flag')
  c = dependency('libccc')
endif
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // flag flipped true -> required + branch live; feat forced
        // disabled -> lookup skipped entirely
        assert_eq!(names(&deps.required), vec!["libaaa", "libccc"]);
        assert!(
            !deps
                .required
                .iter()
                .chain(deps.optional.iter())
                .any(|x| x.name_norm == "libbbb")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_legacy_options_filename_is_honored() {
        // meson_options.txt is the legacy filename still in wide use
        // (harfbuzz, appstream, ...)
        let d = tmpdir("meson-legacy-options");
        fs::write(
            d.join("meson_options.txt"),
            "option('legacy', type: 'boolean', value: false, description: 'x')\n",
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            "project('legacy', 'c')\na = dependency('libaaa', required: get_option('legacy'))\n",
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.optional), vec!["libaaa"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_override_dependency_is_a_provide_not_a_requirement() {
        // the GLib shape: glib/girepository/meson.build ends with
        //   meson.override_dependency('girepository-2.0', libgirepository_dep)
        // — a declaration that THIS tree's own build provides the
        // module. Misreading it as a dependency() call sends the
        // resolver hunting for a module the tree already builds (the
        // girepository-2.0 false positive).
        let d = tmpdir("meson-override-dep");
        fs::write(
            d.join("meson.build"),
            r#"project('glib-like', 'c')
subdir('girepository')
subdir('user')
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("girepository")).unwrap();
        fs::write(
            d.join("girepository/meson.build"),
            r#"libgirepository_dep = declare_dependency(
  include_directories: include_directories('.'),
)
meson.override_dependency('girepository-2.0', libgirepository_dep)
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("user")).unwrap();
        fs::write(
            d.join("user/meson.build"),
            r#"gir_dep = dependency('girepository-2.0', required: false)
plain = dependency('zlib')
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // the override line contributed a PROVIDES entry...
        assert!(
            deps.provided_in_tree.contains(&"girepository-2.0".to_string()),
            "override_dependency must register an in-tree provide"
        );
        // ...not a requirement of any kind
        assert!(
            !deps
                .required
                .iter()
                .chain(deps.optional.iter())
                .any(|x| x.name_norm == "girepository-2.0"
                    && x.origin.starts_with("girepository/meson.build")),
            "the override declaration itself must not be read as a dependency()"
        );
        // declare_dependency(...) is likewise never a dependency call
        assert!(
            !deps
                .required
                .iter()
                .chain(deps.optional.iter())
                .any(|x| x.origin.starts_with("girepository/meson.build")),
            "declare_dependency has no string first argument: nothing from it"
        );
        // a sibling dependency('girepository-2.0') call is still a
        // declaration (the planner satisfies it via provided_in_tree)
        assert_eq!(names(&deps.optional), vec!["girepository-2.0"]);
        assert_eq!(names(&deps.required), vec!["zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_string_and_comment_content_is_never_a_call() {
        // `dependency(` inside string literals and # comments is text,
        // not a declaration (single-line strings included — the
        // multi-line ''' form was already covered)
        let d = tmpdir("meson-str-comment");
        fs::write(
            d.join("meson.build"),
            r#"project('strs2', 'c')
s = 'dependency(''ghost-one'')'
# dependency('ghost-two')
msg = "use dependency('ghost-three') here"
t = dependency('libreal')
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libreal"]);
        for ghost in ["ghost-one", "ghost-two", "ghost-three"] {
            assert!(
                !deps
                    .required
                    .iter()
                    .chain(deps.optional.iter())
                    .any(|x| x.name_norm == ghost),
                "{ghost} is string/comment content, not a dependency"
            );
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_appstream_bash_completion_shape_is_required_by_default() {
        // grounding: AppStream's contrib/meson.build declares
        //   if get_option('bash-completion')       # boolean, default TRUE
        //     bash_completion_dep = dependency('bash-completion', version: '>=2.0')
        // A default build of AppStream genuinely requires it — the
        // structural reading keeps it REQUIRED (the curated map, not a
        // name filter, is what resolves it).
        let d = tmpdir("meson-appstream-shape");
        fs::write(
            d.join("meson_options.txt"),
            "option('bash-completion',\n       type: 'boolean',\n       value: true,\n       description: 'Bash completion')\n",
        )
        .unwrap();
        fs::write(
            d.join("meson.build"),
            "project('appstream-like', 'c')\nif get_option('bash-completion')\n  bash_completion_dep = dependency('bash-completion', version: '>=2.0')\nendif\n",
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["bash-completion"]);
        assert_eq!(names(&deps.optional), Vec::<String>::new());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_gtk_cairo_script_interpreter_shape_is_optional() {
        // grounding: gtk4's meson.build declares
        //   cairo_csi_dep = dependency('cairo-script-interpreter', required: false)
        // — the opportunistic "use it if present" form
        let d = tmpdir("meson-gtk-csi");
        fs::write(
            d.join("meson.build"),
            "project('gtk-like', 'c')\ncairo_csi_dep = dependency('cairo-script-interpreter', required: false)\nif not cairo_csi_dep.found()\n  cairo_csi_dep = cc.find_library('cairo-script-interpreter', required: get_option('build-tests'))\nendif\n",
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), Vec::<String>::new());
        assert_eq!(names(&deps.optional), vec!["cairo-script-interpreter"]);
        let csi = deps.optional.first().unwrap();
        assert_eq!(csi.optional_why.as_deref(), Some("required: false"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_foreach_dispatch_gates_backends_per_platform() {
        // the GTK shape: backends dispatched by
        //   foreach backend : ['android', 'broadway', 'wayland', 'win32', 'x11', 'macos']
        //     if get_variable('@0@_enabled'.format(backend))
        //       subdir(backend)
        // with each flag computed from platform checks. The loop is
        // unrolled per item: a backend whose flag is provably false for
        // the scan platform is never entered — its tree's declarations
        // (appleframeworks on the macos backend, DirectX-Headers on
        // win32) must not surface on Linux, while live backends' do.
        let d = tmpdir("meson-foreach-dispatch");
        fs::write(
            d.join("meson.build"),
            r#"project('gdk-like', 'c')
x11_enabled    = false
wayland_enabled = false
macos_enabled  = false
win32_enabled  = false
if host_machine.system() == 'linux'
  x11_enabled = true
  wayland_enabled = true
endif
if host_machine.system() == 'darwin'
  macos_enabled = true
endif
if host_machine.system() == 'windows'
  win32_enabled = true
endif
subdir('gdk')
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("gdk")).unwrap();
        fs::write(
            d.join("gdk/meson.build"),
            r#"foreach backend : ['android', 'broadway', 'wayland', 'win32', 'x11', 'macos']
  if get_variable('@0@_enabled'.format(backend))
    subdir(backend)
    gdk_backends += get_variable('gdk_@0@'.format(backend))
  endif
endforeach
"#,
        )
        .unwrap();
        for (b, dep) in [("x11", "libxcb"), ("wayland", "wayland-client"), ("macos", "appleframeworks"), ("win32", "directx-headers")] {
            fs::create_dir_all(d.join(format!("gdk/{b}"))).unwrap();
            fs::write(
                d.join(format!("gdk/{b}/meson.build")),
                format!("gdk_{b}_deps = [dependency('{dep}')]\n"),
            )
            .unwrap();
        }
        // android/broadway backends have no dir: skipped silently

        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["libxcb", "wayland-client"]);
        // the macos/win32 backends are provably dead on linux: their
        // trees are excluded, not merely unreachable-and-rescanned
        for gone in ["appleframeworks", "directx-headers"] {
            assert!(
                !deps
                    .required
                    .iter()
                    .chain(deps.optional.iter())
                    .any(|x| x.name_norm == gone),
                "{gone} must not appear on a Linux scan (dead dispatch item)"
            );
        }
        // ...and on darwin the SAME fixture surfaces the macos backend
        // and drops x11/wayland — proving the exclusion follows the
        // flags, not any name
        let deps = scan_meson_for(&d, "darwin").unwrap();
        assert_eq!(names(&deps.required), vec!["appleframeworks"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_foreach_unrolls_bindings_for_required_kwargs() {
        // foreach bindings also feed `required:` evaluation through the
        // call-site scope snapshot: an item-gated optional dep keeps its
        // verdict per the loop item's flag
        let d = tmpdir("meson-foreach-req");
        fs::write(
            d.join("meson.build"),
            r#"project('loopreq', 'c')
plugin_a_optional = false
plugin_b_optional = true
foreach plugin : ['plugin_a', 'plugin_b']
  opt = get_variable('@0@_optional'.format(plugin))
  if plugin == 'plugin_a'
    p = dependency('plugin-a', required: opt)
  else
    p = dependency('plugin-b', required: opt)
  endif
endforeach
"#,
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        // per item: plugin-a's pass binds opt=false (optional),
        // plugin-b's pass binds opt=true (required)
        assert_eq!(names(&deps.required), vec!["plugin-b"]);
        assert_eq!(names(&deps.optional), vec!["plugin-a"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn meson_dead_branch_subdir_is_not_rescanned_by_the_fallback() {
        // a subdir() inside a provably-dead branch (beyond foreach: a
        // plain platform check) is provably never entered — the
        // fallback must not resurrect its unconditional declarations
        let d = tmpdir("meson-dead-subdir");
        fs::write(
            d.join("meson.build"),
            r#"project('deadsub', 'c')
if host_machine.system() == 'darwin'
  subdir('macos-stuff')
endif
base = dependency('zlib')
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("macos-stuff")).unwrap();
        fs::write(
            d.join("macos-stuff/meson.build"),
            "fw = dependency('appleframeworks')\n",
        )
        .unwrap();
        let deps = scan_meson_for(&d, "linux").unwrap();
        assert_eq!(names(&deps.required), vec!["zlib"]);
        let _ = fs::remove_dir_all(&d);
    }

    /// Manual validation against real upstream checkouts (not part of
    /// the default suite — no network, no fixtures): point
    /// GITFULL_REAL_TREE=<dir> at a meson project checkout and run with
    /// `cargo test -- --ignored` to see its scan summary.
    #[test]
    #[ignore = "manual: set GITFULL_REAL_TREE to a meson checkout path"]
    fn scan_real_upstream_tree() {
        let Ok(dir) = std::env::var("GITFULL_REAL_TREE") else {
            return;
        };
        let deps = scan(Path::new(&dir), BuildSystem::Meson).unwrap();
        println!("== {dir} ==");
        println!("required ({}):", deps.required.len());
        for d in &deps.required {
            println!(
                "  {} {} [{}]{}",
                d.name,
                d.version.as_deref().unwrap_or(""),
                d.origin,
                d.git_url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default()
            );
        }
        println!("optional ({}):", deps.optional.len());
        for d in &deps.optional {
            println!(
                "  {} [{}] ({})",
                d.name,
                d.origin,
                d.optional_why.as_deref().unwrap_or("?")
            );
        }
        println!("provided in tree: {:?}", deps.provided_in_tree);
        println!("wraps: {:?}", deps.wraps.iter().map(|w| &w.name).collect::<Vec<_>>());
        println!("vendored: {:?}", deps.vendored);
    }

    // ---- cmake: a typical C/C++ project ------------------------------------

    #[test]
    fn cmake_find_package_and_find_library() {
        let d = tmpdir("cmake");
        fs::write(
            d.join("CMakeLists.txt"),
            r#"cmake_minimum_required(VERSION 3.16)
project(tinyviewer C)

find_package(Threads REQUIRED)             # toolchain built-in: skipped
find_package(PkgConfig REQUIRED)            # cmake's pkg-config bridge: skipped
find_package(ZLIB REQUIRED)
find_package(PNG 1.6 REQUIRED)             # version captured
find_package(CURL)                          # not REQUIRED: cmake treats it as soft
find_package(SDL2 CONFIG REQUIRED)
find_library(CURL_LIBRARY NAMES curl libcurl REQUIRED)
find_library(SQLITE3_LIB sqlite3)           # bare form, soft
include(${CMAKE_CURRENT_SOURCE_DIR}/cmake/extra.cmake)
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("cmake")).unwrap();
        fs::write(
            d.join("cmake/extra.cmake"),
            r#"find_package(Freetype REQUIRED)
find_package(Python3 COMPONENTS Interpreter)  # interpreter: skipped
"#,
        )
        .unwrap();

        let deps = scan(&d, BuildSystem::Cmake).unwrap();
        // REQUIRED find_package/find_library declarations
        assert_eq!(
            names(&deps.required),
            vec!["curl", "freetype", "png", "sdl2", "zlib"]
        );
        let png = deps.required.iter().find(|x| x.name == "PNG").unwrap();
        assert_eq!(png.version.as_deref(), Some("1.6"));
        assert_eq!(png.kind, DepKind::CmakePackage);
        // the soft forms are reported as optional
        assert_eq!(names(&deps.optional), vec!["sqlite3"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cmake_pkg_check_modules() {
        let d = tmpdir("cmake-pkgconf");
        fs::write(
            d.join("CMakeLists.txt"),
            r#"find_package(PkgConfig REQUIRED)
pkg_check_modules(GTK3 IMPORTED_TARGET gtk+-3.0)
pkg_check_modules(FOO REQUIRED libxml-2.0>=2.9 freetype2 glib-2.0 >= 2.50)
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Cmake).unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["freetype2", "glib-2.0", "libxml-2.0"]
        );
        // no REQUIRED keyword: soft per pkg_check_modules semantics
        assert_eq!(names(&deps.optional), vec!["gtk+-3.0"]);
        let _ = fs::remove_dir_all(&d);
    }

    // ---- cargo: registry, git, build, dev and target deps ------------------

    #[test]
    fn cargo_manifest_kinds() {
        let d = tmpdir("cargo");
        fs::write(
            d.join("Cargo.toml"),
            r#"[package]
name = "cli-tool"
version = "0.4.0"
edition = "2021"

[dependencies]
serde = { version = "1.0", features = ["derive"] }
anyhow = "1"
my-git-lib = { git = "https://github.com/example/my-git-lib.git", branch = "main" }

[build-dependencies]
cc = "1.0"

[dev-dependencies]
criterion = "0.5"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.27", features = ["fs"] }

[target.'cfg(windows)'.dependencies]
winapi = "0.3"
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Cargo).unwrap();
        // registry + git + build + target deps are all "required"
        assert_eq!(
            names(&deps.required),
            vec!["anyhow", "cc", "my-git-lib", "nix", "serde", "winapi"]
        );
        // dev-dependencies are optional (reported, never provisioned)
        assert_eq!(names(&deps.optional), vec!["criterion"]);
        let git = deps
            .required
            .iter()
            .find(|x| x.name == "my-git-lib")
            .unwrap();
        assert_eq!(git.kind, DepKind::CrateGit);
        assert_eq!(
            git.git_url.as_deref(),
            Some("https://github.com/example/my-git-lib.git")
        );
        let serde = deps.required.iter().find(|x| x.name == "serde").unwrap();
        assert_eq!(serde.kind, DepKind::CrateRegistry);
        assert_eq!(serde.version.as_deref(), Some("1.0"));
        // registry deps are satisfied by cargo itself, not by gitfull
        assert!(deps
            .required_names()
            .iter()
            .all(|d| d.kind == DepKind::CrateGit));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cargo_workspace_members_are_scanned() {
        let d = tmpdir("cargo-ws");
        fs::write(
            d.join("Cargo.toml"),
            r#"[workspace]
members = ["lib", "tools/cli"]

[workspace.dependencies]
log = "0.4"
"#,
        )
        .unwrap();
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(
            d.join("lib/Cargo.toml"),
            "[dependencies]\nlog = { workspace = true }\nflate2 = \"1\"\n",
        )
        .unwrap();
        fs::create_dir_all(d.join("tools/cli")).unwrap();
        fs::write(
            d.join("tools/cli/Cargo.toml"),
            "[dependencies]\nclap = \"4\"\n",
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Cargo).unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["clap", "flate2", "log"] // log once (deduped across manifests)
        );
        let _ = fs::remove_dir_all(&d);
    }

    // ---- autotools: configure.ac --------------------------------------------

    #[test]
    fn autotools_pkg_check_and_check_lib() {
        let d = tmpdir("autotools");
        fs::write(
            d.join("configure.ac"),
            r#"AC_INIT([downloader], [2.1])
AC_PROG_CC
PKG_CHECK_MODULES([DEPS], [libcurl >= 7.70 libxml-2.0 >= 2.9])
PKG_CHECK_MODULES([GUI], [gtk+-3.0], [], [have_gui=no])
AC_CHECK_LIB([m], [sqrt])
AC_CHECK_LIB([crypto], [SHA256_Init])
AC_SEARCH_LIBS([socket], [socket nsl])
dnl a comment line with PKG_CHECK_MODULES([X], [fake]) must be ignored
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Autotools).unwrap();
        assert_eq!(
            names(&deps.required),
            vec!["crypto", "libcurl", "libxml-2.0"]
        );
        // the 4-arg PKG_CHECK_MODULES form handles failure itself: optional
        assert_eq!(names(&deps.optional), vec!["gtk+-3.0"]);
        let curl = deps.required.iter().find(|x| x.name == "libcurl").unwrap();
        assert_eq!(curl.kind, DepKind::PkgConfig);
        assert_eq!(curl.version.as_deref(), Some(">=7.70"));
        assert!(curl.origin.starts_with("configure.ac"), "{}", curl.origin);
        // m/socket/nsl are libc pieces: never discovered
        // the dnl comment line is ignored
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn autotools_optional_and_libc_skips() {
        let d = tmpdir("autotools2");
        fs::write(
            d.join("configure.ac"),
            r#"AC_INIT([t], [1])
PKG_CHECK_MODULES([X], [openssl], [], [have_openssl=no])
AC_SEARCH_LIBS([getaddrinfo], [nsl socket resolv])
AC_CHECK_LIB([z], [compress2])
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Autotools).unwrap();
        // nsl/socket/resolv are libc: skipped; openssl is the 4-arg soft
        // form (optional); z is a real required library
        assert_eq!(names(&deps.required), vec!["z"]);
        assert_eq!(names(&deps.optional), vec!["openssl"]);
        let _ = fs::remove_dir_all(&d);
    }

    // ---- make: pkg-config invocations ---------------------------------------

    #[test]
    fn makefile_pkg_config_invocations() {
        let d = tmpdir("make");
        fs::write(
            d.join("Makefile"),
            r#"PREFIX ?= /usr/local
CFLAGS += $(shell pkg-config --cflags fake-zlib)
LDLIBS += $(shell pkg-config --libs fake-zlib fake-utils)
GLIB_CFLAGS = $(shell pkg-config --cflags fake-glib-2.0 fake-gio-unix-2.0)
GLIB_LIBS = $(shell pkg-config --libs fake-gobject-2.0 fake-gtk4 sdl3 gee-0.8)
# a comment: pkg-config --libs ignored-comment
other:
        pkg-config --cflags something-else >/dev/null
"#,
        )
        .unwrap();
        let deps = scan(&d, BuildSystem::Make).unwrap();
        assert_eq!(
            names(&deps.required),
            vec![
                "fake-gio-unix-2.0",
                "fake-glib-2.0",
                "fake-gobject-2.0",
                "fake-gtk4",
                "fake-utils",
                "fake-zlib",
                "gee-0.8",
                "sdl3",
                "something-else"
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    // ---- generality: four differently-shaped repos through the SAME scan ---

    #[test]
    fn discovery_is_generic_across_unrelated_shapes() {
        // The whole point of this fix: one generic mechanism, driven purely
        // by each repo's own files. Four unrelated fixture shapes with four
        // different dependency sets — none special-cased anywhere.
        let base = tmpdir("generic");

        // repo A: meson (GNOME-ish)
        let a = base.join("repo-a");
        fs::create_dir_all(&a).unwrap();
        fs::write(
            a.join("meson.build"),
            "project('a')\ndependency('glib-2.0')\ndependency('libpng', required: false)\n",
        )
        .unwrap();
        // repo B: cmake (viewer-ish)
        let b = base.join("repo-b");
        fs::create_dir_all(&b).unwrap();
        fs::write(
            b.join("CMakeLists.txt"),
            "find_package(ZLIB REQUIRED)\npkg_check_modules(PNG REQUIRED libpng)\n",
        )
        .unwrap();
        // repo C: cargo (cli-ish)
        let c = base.join("repo-c");
        fs::create_dir_all(&c).unwrap();
        fs::write(
            c.join("Cargo.toml"),
            "[dependencies]\nserde = \"1.0\"\nregex = { git = \"https://example.com/r.git\" }\n",
        )
        .unwrap();
        // repo D: autotools (downloader-ish)
        let dd = base.join("repo-d");
        fs::create_dir_all(&dd).unwrap();
        fs::write(
            dd.join("configure.ac"),
            "AC_INIT([d],[1])\nPKG_CHECK_MODULES([D], [libcurl >= 7.70])\nAC_CHECK_LIB([crypto], [x])\n",
        )
        .unwrap();
        // repo E: make (script-ish)
        let e = base.join("repo-e");
        fs::create_dir_all(&e).unwrap();
        fs::write(
            e.join("Makefile"),
            "LDLIBS += $(shell pkg-config --libs fake-zlib)\n",
        )
        .unwrap();

        let sa = scan(&a, BuildSystem::Meson).unwrap();
        assert_eq!(names(&sa.required), vec!["glib-2.0"]);
        let sb = scan(&b, BuildSystem::Cmake).unwrap();
        assert_eq!(names(&sb.required), vec!["libpng", "zlib"]);
        let sc = scan(&c, BuildSystem::Cargo).unwrap();
        assert_eq!(names(&sc.required), vec!["regex", "serde"]);
        let sd = scan(&dd, BuildSystem::Autotools).unwrap();
        assert_eq!(names(&sd.required), vec!["crypto", "libcurl"]);
        let se = scan(&e, BuildSystem::Make).unwrap();
        assert_eq!(names(&se.required), vec!["fake-zlib"]);

        // and no cross-contamination: A's optional dep never leaks into B
        assert_eq!(names(&sa.optional), vec!["libpng"]);
        assert!(sb.optional.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scans_survive_missing_files() {
        let d = tmpdir("empty");
        for bs in [
            BuildSystem::Meson,
            BuildSystem::Cmake,
            BuildSystem::Cargo,
            BuildSystem::Autotools,
            BuildSystem::Make,
        ] {
            let deps = scan(&d, bs).unwrap();
            assert!(deps.required.is_empty(), "{bs:?}");
            assert!(deps.wraps.is_empty(), "{bs:?}");
        }
        let _ = fs::remove_dir_all(&d);
    }
}

