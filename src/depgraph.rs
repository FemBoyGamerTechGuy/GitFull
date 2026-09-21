//! Build-system-native dependency-graph discovery.
//!
//! gitfull does not stop at "this project uses meson": it parses the
//! **build system's own dependency declarations** in the cloned source
//! tree and provisions every declared library dependency from source,
//! exactly like it provisions toolchain components.
//!
//! | build system | files parsed                                    | declarations recognized                                     |
//! |--------------|-------------------------------------------------|-------------------------------------------------------------|
//! | meson        | every `meson.build`, plus `subprojects/*.wrap`  | `dependency('name', ...)` calls; `.wrap` subprojects (git or file) |
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

use std::collections::BTreeSet;
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
    /// (`required: false`, `find_package(... OPTIONAL)`, ...).
    pub required: bool,
    /// Version constraint as written (e.g. `>=1.2`), report-only: the
    /// build system's own dependency check stays the version authority.
    pub version: Option<String>,
    /// `file:line` where it was declared.
    pub origin: String,
    /// For `CrateGit` deps: the git URL from the manifest.
    pub git_url: Option<String>,
}

/// A parsed meson subproject wrap file.
#[derive(Debug, Clone, PartialEq)]
pub struct WrapDep {
    /// Wrap file stem (e.g. `zlib` for `subprojects/zlib.wrap`).
    pub name: String,
    /// `[wrap-git]`: repository url + revision.
    pub git: Option<(String, String)>,
    /// `[wrap-file]`: source url (+ optional patch url).
    pub file: Option<(String, Option<String>)>,
    /// `[provide] dependency_names = a,b` — pkg-config names this wrap
    /// provides (newer wrap-db convention). Falls back to the stem.
    pub provides: Vec<String>,
    pub origin: String,
}

/// Everything one source tree declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclaredDeps {
    /// Dependencies gitfull must provision (or prove satisfied).
    pub required: Vec<DeclaredDep>,
    /// Declared but optional per the manifest — reported, not provisioned.
    pub optional: Vec<DeclaredDep>,
    /// meson subproject wraps (an independent, self-describing source).
    pub wraps: Vec<WrapDep>,
    /// meson vendored subprojects (`subprojects/<name>/` checked-in
    /// trees, no wrap): resolved in-tree, never fetched.
    pub vendored: Vec<String>,
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
pub const MESON_BUILTINS: &[&str] = &["threads", "python3", "gtest", "gmock", "disabler"];

/// cmake built-in `find_package()` modules that are satisfied by the
/// toolchain (threads / language runtimes), not external libraries.
pub const CMAKE_BUILTINS: &[&str] = &[
    "threads",
    "python3",
    "pythoninterp",
    "python",
    "cmake",
    "pkgconfig",
];

/// `AC_CHECK_LIB` / `AC_SEARCH_LIBS` targets that live in libc / the
/// compiler runtime delivered by the toolchain's gcc (the classic
/// libc helper libraries).
pub const LIBC_LIBS: &[&str] = &[
    "c", "m", "dl", "pthread", "rt", "intl", "gcc", "gcc_s", "supc++", "socket", "nsl",
    "resolv", "crypt",
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

    // dependency() calls in every meson.build (subprojects/ trees are
    // managed via wraps — their own manifests are not the project's
    // declarations)
    let mut builds: Vec<PathBuf> = Vec::new();
    walk_files(src, &mut builds, &|n| n == "meson.build", &["subprojects"]);
    for f in builds {
        let text = fs::read_to_string(&f).unwrap_or_default();
        let origin_base = f.strip_prefix(src).unwrap_or(&f).display().to_string();
        parse_meson_dependency_calls(&text, &origin_base, &mut deps);
    }
    Ok(deps)
}

/// Extract `dependency('name', ...)` calls (including multi-line ones)
/// with their salient kwargs. Also records `subproject('x')`-style
/// references implicitly: a name provided by a wrap is fetched via the
/// wrap (see [`DeclaredDeps::wrap_provided`]).
fn parse_meson_dependency_calls(text: &str, origin_base: &str, deps: &mut DeclaredDeps) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = find_sub(bytes, i, b"dependency(") {
        // avoid matching `meson.get_compiler(...).dependency(` twice is
        // fine — same call; but avoid `subproject.dependency(`? meson
        // vars can shadow; over-approximation is safe (dedup by name).
        let start = rel + b"dependency(".len();
        // scan the argument list for a balanced ')'
        let mut depth = 1usize;
        let mut j = start;
        let mut in_str: Option<u8> = None;
        while j < bytes.len() && depth > 0 {
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
            }
            j += 1;
        }
        let call = &text[start..j.saturating_sub(1).max(start)];
        let line = 1 + text[..rel].matches('\n').count();
        record_meson_call(call, &format!("{origin_base}:{line}"), deps);
        i = j.max(rel + 1);
    }
}

fn record_meson_call(call: &str, origin: &str, deps: &mut DeclaredDeps) {
    // first argument: a string literal (the dependency name)
    let trimmed = call.trim_start();
    let name = if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        let q = trimmed.as_bytes()[0];
        trimmed[1..]
            .split(q as char)
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        return; // dynamic name (variable/computed): not statically knowable
    };
    if name.is_empty() || MESON_BUILTINS.contains(&name.as_str()) {
        // build-system built-ins (threads, gtest, ...) are satisfied by
        // the toolchain itself: skip entirely
        return;
    }
    // kwargs of interest: required: false / version: '...'
    let lower = call.to_ascii_lowercase();
    let required =
        !lower.contains("required") || lower.contains("required: true") || lower.contains("required : true");
    let version = extract_kwarg_string(call, "version");
    let d = DeclaredDep {
        name: name.clone(),
        name_norm: normalize_name(&name),
        kind: DepKind::PkgConfig,
        required,
        version,
        origin: origin.to_string(),
        git_url: None,
    };
    if required {
        deps.required.push(d);
    } else {
        deps.optional.push(d);
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
    let mut file: Option<(String, Option<String>)> = None;
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
        let Some((k, v)) = line.split_once('=') else { continue };
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
                file = Some((v.to_string(), None));
            }
            ("wrap-file", "patch_url") => {
                if let Some(f) = file.as_mut() {
                    f.1 = Some(v.to_string());
                }
            }
            ("provide", "dependency_names") => {
                provides = v
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
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
    let mut push = |name: &str, kind: DepKind, required: bool, version: Option<String>, line: usize| {
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) {
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
            version,
            origin: format!("{origin_base}:{line}"),
            git_url: None,
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
                if matches!(tu.as_str(), "HINTS" | "PATHS" | "PATH_SUFFIXES" | "DOC" | "REQUIRED" | "NO_DEFAULT_PATH" | "NAMES_PER_DIR") {
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
                if t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
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
                if tu == "IMPORTED_TARGET" || tu == "REQUIRED" || tu == "QUIET" || tu == "NO_CMAKE_PATH" || tu == "NO_CMAKE_ENVIRONMENT_PATH" {
                    continue;
                }
                if matches!(t.as_str(), ">=" | "<=" | "==" | "!=" | ">" | "<" | "=") {
                    expect_version = true;
                    continue;
                }
                // split off an attached version constraint
                let (name, _ver) = split_pkg_constraint(t);
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) {
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
            return (&tok[..p], Some(format!("{op}{}", tok[p + op.len()..].trim())));
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
    let doc: toml::Value = toml::from_str(&text).map_err(|e| {
        GitfullError::Unsupported(format!("in {origin}: {e}"))
    })?;

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
            .and_then(|t| if table_name.contains('.') { t.get("dependencies") } else { Some(t) })
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
                let git_url = spec.get("git").and_then(|g| g.as_str()).map(|s| s.to_string());
                let d = DeclaredDep {
                    name: crate_name.clone(),
                    name_norm: crate_name.to_ascii_lowercase(),
                    kind: if git_url.is_some() {
                        DepKind::CrateGit
                    } else {
                        DepKind::CrateRegistry
                    },
                    required: !is_dev,
                    version: spec.as_str().map(|s| s.to_string()).or_else(|| {
                        spec.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
                    }),
                    origin: origin.clone(),
                    git_url,
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
                    || !norm.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
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
                    version: ver.or(ver2.map(|s| s.to_string())),
                    origin: format!("{origin}:{line}"),
                    git_url: None,
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
                    if !norm.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) {
                        continue;
                    }
                    deps.required.push(DeclaredDep {
                        name: l.to_string(),
                        name_norm: norm,
                        kind: DepKind::AutoconfLib,
                        required: true,
                        version: None,
                        origin: format!("{origin}:{line}"),
                        git_url: None,
                    });
                }
            }
            idx = abs + macro_name.len();
        }
    }
    Ok(deps)
}

/// Split an m4 macro invocation into its bracketed arguments
/// (`NAME([a], [b]) → ["a", "b"]`). `pos` points at the macro name; the
/// name (and optional whitespace) is skipped before the `(`.
fn m4_args(text: &str, pos: usize) -> Option<Vec<String>> {
    let bytes = text.as_bytes();
    let mut i = pos;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
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
            args.push(cur.clone());
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
    args.push(cur);
    Some(args)
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
            .map(|c| if matches!(c, '(' | ')' | '$' | '"' | '\'') { ' ' } else { c })
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
                || !norm.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
            {
                continue;
            }
            deps.required.push(DeclaredDep {
                name: name.to_string(),
                name_norm: norm,
                kind: DepKind::PkgConfig,
                required: true,
                version: None,
                origin: format!("Makefile:{}", i + 1),
                git_url: None,
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
                "cairo", "glib-2.0", "harfbuzz", "libpng", "libxml-2.0", "zlib"
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
    fn meson_wraps_and_vendored_subprojects() {
        let d = tmpdir("wraps");
        fs::write(d.join("meson.build"), "project('w')\nz = dependency('zlib')\n").unwrap();
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
        fs::write(d.join("subprojects/vendored-thing/meson.build"), "project('v')\n").unwrap();

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
            s.file.as_ref().map(|(u, p)| (u.as_str(), p.clone())),
            Some(("https://example.com/somelib-2.3.tar.gz", Some("https://example.com/somelib-2.3-patch.tar.gz".to_string())))
        );
        // vendored dirs are recorded (nothing must be fetched for them)
        assert!(deps.vendored.contains(&"vendored-thing".to_string()));
        // wrap-provided names are queryable
        assert!(deps.wrap_provided().contains("zlib"));
        let _ = fs::remove_dir_all(&d);
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
            vec![
                "anyhow", "cc", "my-git-lib", "nix", "serde", "winapi"
            ]
        );
        // dev-dependencies are optional (reported, never provisioned)
        assert_eq!(names(&deps.optional), vec!["criterion"]);
        let git = deps.required.iter().find(|x| x.name == "my-git-lib").unwrap();
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
                "fake-gio-unix-2.0", "fake-glib-2.0", "fake-gobject-2.0", "fake-gtk4",
                "fake-utils", "fake-zlib", "gee-0.8", "sdl3", "something-else"
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
