//! Conditional-context evaluation for meson build files.
//!
//! `dependency()` calls in a meson.build are NOT all unconditional
//! declarations: meson is a full DSL whose `if`/`elif`/`else` blocks gate
//! them on platform checks, options and compiler probes. A call like
//!
//! ```meson
//! if host_machine.system() == 'darwin'
//!   framework_dep = dependency('appleframeworks', ...)
//! endif
//! ```
//!
//! declares a dependency of a *darwin* build only — surfacing it as a
//! required dependency of a *Linux* build (the scanner's platform) is a
//! parsing bug, not an over-reporting quirk: it sends the resolver
//! hunting for a macOS framework on Linux.
//!
//! This module supplies the conditional half of meson parsing for
//! [`crate::depgraph`]: given a target system name it decides, per line
//! of every `meson.build`, whether that line is **provably unreachable**
//! for that platform. The rules are deliberately one-sided:
//!
//! * a branch is excluded only when its condition **provably evaluates
//!   to false** for the target platform (e.g. `host_system == 'darwin'`
//!   when scanning for `linux`);
//! * anything not statically decidable — `get_option(...)`, compiler
//!   probes, runtime lookups — counts as *possibly taken* and stays
//!   included. Under-provisioning a real dependency is a build failure;
//!   over-including an undecided one costs at most a report line.
//!
//! # Variable scope — `subdir()` shares the root scope
//!
//! meson executes `subdir('x')` in the **same variable scope** as the
//! calling file (unlike `subproject()`), in call order. Real projects
//! exploit this: GLib's root meson.build computes
//! `glib_have_cocoa = false` (only set true inside a
//! `host_system == 'darwin'` block) and *subdir*'d files gate
//! `dependency('appleframeworks', ...)` on `if glib_have_cocoa`. So the
//! evaluator processes the root file and every statically-reachable
//! `subdir()` child in execution order with ONE shared symbol table,
//! and applies only assignments on active lines — on Linux
//! `glib_have_cocoa` stays `false` and the framework dep is provably
//! unreachable, exactly the decision meson itself would make at
//! configure time.
//!
//! Files that are NOT statically reachable (dynamic `subdir(var)`
//! paths, foreach-driven subdirs) are evaluated with an isolated scope
//! by the caller — their unconditional declarations still count.
//!
//! # Machine objects
//!
//! gitfull builds natively (on the machine it runs, for that machine),
//! so `host_machine`, `build_machine` and `target_machine` all describe
//! the machine running gitfull. `.system()` is therefore compared
//! against the current platform's meson system name; `.cpu_family()` /
//! `.endian()` likewise (conservative where Rust's arch name and
//! meson's cpu_family spellings could diverge). Cross compilation would
//! need a `--host` flag — out of scope today.
//!
//! # Project options — `get_option()` against declared defaults
//!
//! A project's own options (the `option()` declarations of
//! `meson.options` / `meson_options.txt`, plus `default_options:` in the
//! `project()` call) decide reachability and requiredness just as much
//! as the platform does:
//!
//! ```meson
//! if get_option('bash-completion')            # boolean, default true
//!   bash_completion_dep = dependency('bash-completion')
//! endif
//! gst_dep = dependency('gstreamer-play-1.0',
//!                      required: get_option('media-gstreamer'))  # feature
//! ```
//!
//! meson evaluates `get_option` against the *default* value unless the
//! user passes `-D` overrides — and a default configuration is exactly
//! what gitfull provisions. Options are therefore resolved to their
//! declared defaults: booleans to `true`/`false`, feature options to
//! `'enabled'`/`'disabled'`/`'auto'` (with the `.enabled()`/`.disabled()`/
//! `.auto()`/`.allowed()` predicates and the `.disable_auto()`/
//! `.enable_auto()` coercions meson defines on them), string/combo
//! options to their default string. Anything else — meson's builtin
//! options (`prefix`, `warning_level`, …), integers, arrays, options no
//! file declares — stays *unknown*, and the same one-sided rule as
//! everywhere in this module applies: unknown keeps the branch
//! reachable and the dependency required.
//!
//! # What is understood
//!
//! | construct                                        | examples (all found in real upstreams) |
//! |--------------------------------------------------|----------------------------------------|
//! | machine `.system()` comparisons                  | `if host_machine.system() == 'darwin'` |
//! | variable aliases of machine calls                | `host_system = host_machine.system()` |
//! | boolean/string/array literals & variables        | `glib_have_cocoa = false`              |
//! | `and` / `or` / `not` (tri-state)                 | `… and not get_option('quartz').disabled()` |
//! | `if` / `elif` / `else` / `endif`, nested         | `if host_system == 'linux'` … `else` … |
//! | `in` / `not in` against arrays                   | `if host_system not in ['windows', 'darwin']` |
//! | `.contains()` on arrays                          | `if ['x86','x86_64'].contains(host_machine.cpu_family())` |
//! | `get_option('x')` against declared defaults       | `if get_option('bash-completion')` |
//! | feature-option predicates & coercions            | `required: get_option('x').disable_auto()` |
//! | `subdir('path')` scope sharing, in order         | `subdir('src')`                        |
//! | `+` concatenation / `/` path join                | `subdir('utils' / 'vdf')`              |
//!
//! Everything else evaluates to *unknown* → the branch stays reachable.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// tri-state + statically-known values
// ---------------------------------------------------------------------------

/// Three-valued logic for meson conditions that cannot always be
/// decided statically. `Unknown` means "could be either", and every
/// consumer treats it as *possibly true* (branch reachable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn from_bool(b: bool) -> Tri {
        if b {
            Tri::True
        } else {
            Tri::False
        }
    }

    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }

    fn and(self, other: Tri) -> Tri {
        // short-circuit: a provable false on either side decides
        if self == Tri::False || other == Tri::False {
            Tri::False
        } else if self == Tri::True && other == Tri::True {
            Tri::True
        } else {
            Tri::Unknown
        }
    }

    fn or(self, other: Tri) -> Tri {
        if self == Tri::True || other == Tri::True {
            Tri::True
        } else if self == Tri::False && other == Tri::False {
            Tri::False
        } else {
            Tri::Unknown
        }
    }

    /// Whether a branch guarded by this value must be considered
    /// reachable for the target platform.
    fn reachable(self) -> bool {
        self != Tri::False
    }
}

/// A meson value that can be known statically. Machine introspection is
/// resolved eagerly at the call site (`host_machine.system()` becomes
/// `Str("linux")` for a Linux scan), so downstream logic is plain value
/// comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Bool(bool),
    Str(String),
    Array(Vec<Val>),
    /// `host_machine` / `build_machine` / `target_machine` before its
    /// introspection method is applied (gitfull builds natively: all
    /// three are the machine running the scan).
    Machine,
    Unknown,
}

impl Val {
    fn tri(&self) -> Tri {
        match self {
            Val::Bool(b) => Tri::from_bool(*b),
            _ => Tri::Unknown,
        }
    }

    fn tri_val(t: Tri) -> Val {
        match t {
            Tri::True => Val::Bool(true),
            Tri::False => Val::Bool(false),
            Tri::Unknown => Val::Unknown,
        }
    }

    /// Truth-table equality. Cross-type compares are simply unequal
    /// (meson would reject them at configure time anyway); anything
    /// involving `Unknown` stays undecidable.
    fn eq_val(&self, other: &Val) -> Tri {
        match (self, other) {
            (Val::Unknown, _) | (_, Val::Unknown) => Tri::Unknown,
            (Val::Bool(a), Val::Bool(b)) => Tri::from_bool(a == b),
            (Val::Str(a), Val::Str(b)) => Tri::from_bool(a == b),
            (Val::Array(a), Val::Array(b)) => {
                if a.len() == b.len() {
                    let mut acc = Tri::True;
                    for (x, y) in a.iter().zip(b.iter()) {
                        acc = acc.and(x.eq_val(y));
                        if acc == Tri::False {
                            break;
                        }
                    }
                    acc
                } else {
                    Tri::False
                }
            }
            _ => Tri::False,
        }
    }
}

// ---------------------------------------------------------------------------
// the platform we scan for
// ---------------------------------------------------------------------------

/// meson's `system()` name for the machine gitfull runs on (and, being
/// a native builder, builds for). Rust and meson disagree only on
/// macOS, which meson calls `darwin`.
pub fn current_system() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_string(),
        other => other.to_string(),
    }
}

/// meson `cpu_family()` names Rust's target arch can vouch for. Where
/// the spellings could diverge (Rust `powerpc64` vs meson `ppc64`, …)
/// the answer is `Unknown` rather than a wrong comparison.
fn current_cpu_family() -> Val {
    match std::env::consts::ARCH {
        "x86" | "x86_64" | "aarch64" | "arm" | "riscv64" | "loongarch64" | "s390x" => {
            Val::Str(std::env::consts::ARCH.to_string())
        }
        _ => Val::Unknown,
    }
}

fn current_endian() -> Val {
    Val::Str(if cfg!(target_endian = "little") {
        "little".to_string()
    } else {
        "big".to_string()
    })
}

// ---------------------------------------------------------------------------
// project options (meson.options / meson_options.txt + default_options)
// ---------------------------------------------------------------------------

/// A meson project option definition, reduced to what static evaluation
/// needs: its type and its declared default value.
#[derive(Debug, Clone, PartialEq)]
pub struct OptDef {
    /// meson option type: `'boolean'`, `'feature'`, `'string'`,
    /// `'combo'`, `'integer'`, `'array'`.
    pub ty: String,
    /// The option's default as a static value (`Bool` for booleans,
    /// feature-state `Str` for features, `Str` for string/combo,
    /// `Unknown` for integer/array/missing).
    pub default: Val,
}

/// The project's option table: option name → definition.
pub type OptTable = HashMap<String, OptDef>;

/// The three states a meson feature option can hold.
fn is_feature_state(s: &str) -> bool {
    matches!(s, "enabled" | "disabled" | "auto")
}

/// Byte-slice substring search (`haystack[from..]` contains `needle`?
/// — offset into the full haystack).
fn find_sub(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
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

/// Read the project's option table from `meson.options` (modern) or
/// `meson_options.txt` (legacy) in `project_root`. Both filenames are
/// accepted; meson only ever reads one, and projects do not ship both.
pub fn load_project_options(project_root: &Path) -> OptTable {
    for name in ["meson.options", "meson_options.txt"] {
        let p = project_root.join(name);
        if let Ok(text) = fs::read_to_string(&p) {
            return parse_options_file(&text);
        }
    }
    OptTable::new()
}

/// Parse the `option('name', type: '...', value: <default>, …)` calls of
/// a meson options file. Only the type and value kwargs matter here;
/// description/yield/deprecated and unknown kwargs are ignored.
pub fn parse_options_file(text: &str) -> OptTable {
    let mut out = OptTable::new();
    for body in extract_call_bodies(text, "option") {
        let segs = split_top_level_args(&body);
        // first positional argument: the option name (string literal)
        let Some(name) = segs.first().and_then(|s| string_literal(s.trim())) else {
            continue;
        };
        let mut ty = String::from("string");
        let mut value: Option<Val> = None;
        for seg in segs.iter().skip(1) {
            let Some((k, v)) = split_kwarg(&clean_segment(seg)) else {
                continue;
            };
            match k.as_str() {
                "type" => {
                    if let Some(t) = string_literal(v.trim()) {
                        ty = t;
                    }
                }
                "value" => value = Some(parse_value_literal(v.trim())),
                _ => {}
            }
        }
        let default = typed_default(&ty, value);
        out.insert(name, OptDef { ty, default });
    }
    out
}

/// The statically-known default of an option of type `ty` whose
/// `value:` kwarg evaluated to `value` (meson's own fallbacks when
/// `value:` is absent: boolean `false`, feature `'auto'`).
fn typed_default(ty: &str, value: Option<Val>) -> Val {
    match (ty, value) {
        ("boolean", Some(Val::Bool(b))) => Val::Bool(b),
        ("boolean", _) => Val::Bool(false),
        ("feature", Some(Val::Str(s))) if is_feature_state(&s) => Val::Str(s),
        ("feature", _) => Val::Str("auto".to_string()),
        ("string" | "combo", Some(Val::Str(s))) => Val::Str(s),
        // integer / array / combo-without-value: not statically decidable
        _ => Val::Unknown,
    }
}

/// Overlay `project(..., default_options: ['k=v', …])` onto the option
/// table: those values are the defaults meson applies for this project,
/// overriding the option-definition values (meson's option precedence:
/// command line > env > native files > `default_options` > option
/// definition). Only options already in the table are updated —
/// `default_options` for meson builtins (`warning_level`, `werror`, …)
/// changes nothing here because builtins stay `Unknown` anyway.
pub fn overlay_default_options(root_text: &str, opts: &mut OptTable) {
    // only the FIRST project() call is the real one
    let Some(body) = extract_call_bodies(root_text, "project").into_iter().next() else {
        return;
    };
    for seg in split_top_level_args(&body).into_iter().skip(1) {
        let Some((k, v)) = split_kwarg(&clean_segment(&seg)) else {
            continue;
        };
        if k != "default_options" {
            continue;
        }
        for item in string_array_items(v.trim()) {
            let Some((name, raw)) = item.split_once('=') else {
                continue;
            };
            let (name, raw) = (name.trim(), raw.trim());
            let Some(def) = opts.get_mut(name) else {
                continue; // builtin or unknown option: stays Unknown
            };
            let coerced = match &def.default {
                Val::Bool(_) => match raw {
                    "true" => Some(Val::Bool(true)),
                    "false" => Some(Val::Bool(false)),
                    _ => None,
                },
                Val::Str(s) if is_feature_state(s) => match raw {
                    "enabled" | "disabled" | "auto" => Some(Val::Str(raw.to_string())),
                    _ => None,
                },
                Val::Str(_) => Some(Val::Str(raw.to_string())),
                _ => None,
            };
            if let Some(c) = coerced {
                def.default = c;
            }
        }
    }
}

// -- argument-level helpers (string/comment aware) -------------------------

/// The argument-body text of every `func(…)` call in `text` whose opening
/// is a standalone token (not `xfunc(`, not `obj.func(`). Multi-line calls
/// are handled: the body runs to the balanced close paren, skipping string
/// literals.
fn extract_call_bodies(text: &str, func: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let needle: Vec<u8> = func.bytes().chain(std::iter::once(b'(')).collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = find_sub(bytes, i, &needle) {
        // a standalone call: preceded by a non-identifier, non-dot byte
        // (or the start of the text)
        let standalone = match rel.checked_sub(1).map(|k| bytes[k]) {
            None => true,
            Some(c) => !c.is_ascii_alphanumeric() && c != b'_' && c != b'.',
        };
        if standalone {
            let start = rel + needle.len();
            if let Some(end) = balanced_end(bytes, start) {
                out.push(text[start..end].to_string());
                i = end + 1;
                continue;
            }
        }
        i = rel + 1;
    }
    out
}

/// Index just past the `)` matching the open paren at depth 1 starting
/// from `start` (which points AFTER the opening `(`), skipping string
/// literals. `None` when unbalanced.
fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
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
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Split an argument list at top-level commas — string-, paren- and
/// bracket-aware. Segments keep their raw text (multi-line calls span
/// lines); callers clean comments per segment via [`clean_segment`].
fn split_top_level_args(args: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2; // skip the escaped character entirely
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => in_str = Some(c),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push(args[seg_start..i].to_string());
                seg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(args[seg_start..].to_string());
    out
}

/// Strip `# comment` tails (to end of line) from a multi-line argument
/// segment, quote-aware. Comments are legal inside multi-line calls:
///
/// ```meson
/// dependency('foo',   # the foo provider
///   required: false)
/// ```
fn clean_segment(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = String::with_capacity(seg.len());
    let mut in_str: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if c == b'#' {
            // comment: skip to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'\'' || c == b'"' {
            in_str = Some(c);
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// `key : value` — a kwarg-shaped argument segment. The split colon is
/// the first top-level `:` outside string literals; the key must be a
/// bare identifier (comparisons use `==`, never a lone `:`).
fn split_kwarg(seg: &str) -> Option<(String, String)> {
    let t = seg.trim();
    let bytes = t.as_bytes();
    let mut in_str: Option<u8> = None;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2; // skip the escaped character entirely
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => in_str = Some(c),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                let key = t[..i].trim();
                let valid = !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if valid {
                    return Some((key.to_string(), t[i + 1..].trim().to_string()));
                }
                return None;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A `'…'` / `"…"` string literal → its value.
fn string_literal(v: &str) -> Option<String> {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        return Some(v[1..v.len() - 1].to_string());
    }
    None
}

/// A meson value literal: `true`/`false`, a string literal, or
/// `Unknown` (numbers, arrays, anything computed).
fn parse_value_literal(v: &str) -> Val {
    match v {
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        _ => string_literal(v).map(Val::Str).unwrap_or(Val::Unknown),
    }
}

/// The string items of an array literal `['a', 'b']`, or a single
/// quoted string. Anything else yields nothing.
fn string_array_items(v: &str) -> Vec<String> {
    if let Some(s) = string_literal(v) {
        return vec![s];
    }
    let t = v.trim();
    if !(t.starts_with('[') && t.ends_with(']')) {
        return Vec::new();
    }
    split_top_level_args(&t[1..t.len() - 1])
        .iter()
        .filter_map(|s| string_literal(s.trim()))
        .collect()
}

// ---------------------------------------------------------------------------
// per-file result
// ---------------------------------------------------------------------------

/// One physical line of a meson.build: byte range into the file text
/// plus whether statements on it are reachable for the scan platform.
#[derive(Debug, Clone)]
pub struct LineAct {
    /// Byte offset of the line start (into the file text).
    pub start: usize,
    /// Byte offset of the line end, exclusive (before the `\n`).
    pub end: usize,
    /// False only when the line is provably unreachable for the target
    /// platform (an inactive `if`/`elif`/`else` branch somewhere up the
    /// stack).
    pub active: bool,
    /// The line begins inside a multi-line `'''…'''` string — it is
    /// string *content*, not a statement (`dependency(` inside it is
    /// never a real call).
    pub in_string: bool,
}

/// One analyzed meson.build: its text plus the activity of every line.
#[derive(Debug, Clone)]
pub struct FileEval {
    pub path: PathBuf,
    pub text: String,
    pub lines: Vec<LineAct>,
    /// Variable-scope snapshots taken at statement lines containing a
    /// `dependency(` call — keyed by line START offset. The `required:`
    /// kwarg of a call may reference variables assigned just above it
    /// (`gssapi_opt = get_option('gssapi')` → `required: gssapi_opt`),
    /// so the live scope at the call site is what evaluates it.
    pub dep_scopes: BTreeMap<usize, HashMap<String, Val>>,
}

impl FileEval {
    /// Whether a `dependency(` call starting at byte `offset` sits on a
    /// reachable, non-string-content line.
    pub fn statement_active_at(&self, offset: usize) -> bool {
        let idx = self.lines.partition_point(|l| l.start <= offset);
        match idx.checked_sub(1) {
            Some(i) => {
                let l = &self.lines[i];
                offset <= l.end && l.active && !l.in_string
            }
            None => true, // before the first recorded line: reachable
        }
    }

    /// The start offset of the line containing byte `offset`.
    pub fn line_start_at(&self, offset: usize) -> Option<usize> {
        let idx = self.lines.partition_point(|l| l.start <= offset);
        idx.checked_sub(1).map(|i| self.lines[i].start)
    }

    /// The variable-scope snapshot taken at the statement line that
    /// contains byte `offset` (multi-line calls use the scope at their
    /// first line — where the statement begins executing).
    pub fn dep_scope_at(&self, offset: usize) -> Option<&HashMap<String, Val>> {
        self.line_start_at(offset).and_then(|s| self.dep_scopes.get(&s))
    }
}

// ---------------------------------------------------------------------------
// scope + evaluation driver
// ---------------------------------------------------------------------------

/// Shared variable scope for one meson project: the root file plus
/// every file pulled in by active `subdir()` calls, in execution order
/// (meson semantics: `subdir()` executes the child in the SAME scope),
/// plus the project's option table (`get_option` resolves against the
/// declared defaults — the configuration gitfull provisions).
struct Scope {
    system: String,
    vars: HashMap<String, Val>,
    options: OptTable,
}

/// Analyze the conditional structure of a whole meson project for
/// `system`: the root `meson.build` plus every statically-reachable
/// `subdir()` child, in meson's execution order, with one shared
/// variable scope. The option table is loaded from the project root
/// (`meson.options` / `meson_options.txt`) with `project()`'s
/// `default_options:` overlaid.
pub fn eval_project(root_file: &Path, system: &str) -> Vec<FileEval> {
    let mut options = load_project_options(
        root_file
            .parent()
            .unwrap_or_else(|| Path::new(".")),
    );
    if let Ok(text) = fs::read_to_string(root_file) {
        overlay_default_options(&text, &mut options);
    }
    eval_project_with_options(root_file, system, options).files
}

/// [`eval_project`] with a caller-supplied option table (the scanner
/// loads the table once and shares it with isolated-file evaluation).
/// Returns the analyzed files plus the set of meson.build files that
/// are provably never entered (see [`ProjectEval::excluded`]).
pub fn eval_project_with_options(
    root_file: &Path,
    system: &str,
    options: OptTable,
) -> ProjectEval {
    let mut scope = Scope {
        system: system.to_string(),
        vars: HashMap::new(),
        options,
    };
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out: Vec<FileEval> = Vec::new();
    let mut excluded: BTreeSet<PathBuf> = BTreeSet::new();
    eval_file(&mut scope, root_file, &mut visited, &mut out, &mut excluded);
    ProjectEval { files: out, excluded }
}

/// Analyze one meson.build with a FRESH scope — for files not reachable
/// through static `subdir()` calls (dynamic paths, foreach-driven
/// subdirs). The file's own conditionals are still honored (including
/// `get_option` against the PROJECT's option table — the options file
/// lives at the root regardless of how the file was reached); it simply
/// cannot see the root file's variables. Provably-dead `subdir()`
/// targets found along the way are added to `excluded` so the scanner's
/// fallback skips them too.
pub fn eval_isolated_with_options(
    file: &Path,
    system: &str,
    options: &OptTable,
    excluded: &mut BTreeSet<PathBuf>,
) -> FileEval {
    let mut scope = Scope {
        system: system.to_string(),
        vars: HashMap::new(),
        options: options.clone(),
    };
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out: Vec<FileEval> = Vec::new();
    eval_file(&mut scope, file, &mut visited, &mut out, excluded);
    out.into_iter().next().unwrap_or(FileEval {
        path: file.to_path_buf(),
        text: String::new(),
        lines: Vec::new(),
        dep_scopes: BTreeMap::new(),
    })
}

/// One open `if` (an if/elif/else chain).
struct Frame {
    /// Some earlier branch of this chain provably executed (condition
    /// True with all previous conditions provably False), so no later
    /// branch — and no `else` — can run.
    resolved: bool,
    /// Every earlier branch's condition was provably False.
    prev_all_false: bool,
    /// Whether statements of the CURRENT branch are reachable.
    active: bool,
}

/// One physical line, comment-stripped, with its byte range into the
/// file text.
#[derive(Debug, Clone)]
struct LineCode {
    start: usize,
    /// End of the physical line, exclusive (before the `\n`).
    end: usize,
    /// The line with a trailing `# comment` stripped (prefix untouched).
    code: String,
    /// The line begins inside a multi-line `'''…'''` string.
    started_in_string: bool,
}

/// The result of evaluating a whole project: the analyzed files plus
/// the set of directories that are **provably never entered** (a
/// `subdir()` inside a provably-dead branch, a dead foreach-dispatch
/// item — recorded as DIRECTORY paths so the exclusion covers the
/// target's whole subtree). The scanner's fallback pass skips anything
/// under them: their unconditional declarations would otherwise be
/// over-reported.
#[derive(Debug, Default)]
pub struct ProjectEval {
    pub files: Vec<FileEval>,
    pub excluded: BTreeSet<PathBuf>,
}

/// Process one file: track `if`/`elif`/`else`/`endif` frames, evaluate
/// branch conditions against the shared scope, bind variables from
/// active assignments, unroll `foreach VAR : [literal list]` loops
/// (per-item — the shape backend/plugin dispatch uses), and recurse
/// into active `subdir()` calls at the point meson would execute them.
/// Dead-branch `subdir()` targets are recorded into `excluded`.
fn eval_file(
    scope: &mut Scope,
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    out: &mut Vec<FileEval>,
    excluded: &mut BTreeSet<PathBuf>,
) {
    if !visited.insert(path.to_path_buf()) {
        return; // the same file twice: meson errors; be tolerant
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };

    // pre-pass: split into comment-stripped physical lines (the
    // multi-line '''…''' state machine runs here), merging `\`
    // line-continuations into their statement's first line (meson
    // statements span lines via a trailing backslash — libxml2's
    // `.require()` chains are written that way). Continuation lines
    // keep a physical entry with empty code: they emit a LineAct but
    // never form statements of their own.
    let mut code_at: Vec<LineCode> = Vec::new();
    let mut line_start = 0usize;
    let mut in_string: Option<Quote> = None;
    let mut merge_into: Option<usize> = None;
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        let line_end = line_start + body.len();
        let started_in_string = in_string.is_some();
        let code = strip_comment(body, &mut in_string);
        let trimmed_end = code.trim_end();
        let continued = !started_in_string && trimmed_end.ends_with('\\');
        if let Some(idx) = merge_into {
            // continuation of the statement started at idx
            let joined = format!("{} {}", code_at[idx].code.trim_end(), code.trim());
            code_at[idx].code = joined;
            if !continued {
                merge_into = None;
            }
            code_at.push(LineCode {
                start: line_start,
                end: line_end,
                code: String::new(), // content merged above
                started_in_string: false,
            });
        } else {
            let code = if continued {
                format!("{} ", &trimmed_end[..trimmed_end.len() - 1])
            } else {
                code.clone()
            };
            let idx = code_at.len();
            code_at.push(LineCode {
                start: line_start,
                end: line_end,
                code,
                started_in_string,
            });
            if continued {
                merge_into = Some(idx);
            }
        }
        line_start += raw.len();
    }

    let mut frames: Vec<Frame> = Vec::new();
    let mut lines: Vec<LineAct> = Vec::new();
    let mut dep_scopes: BTreeMap<usize, HashMap<String, Val>> = BTreeMap::new();
    walk_statements(
        scope,
        path,
        &code_at,
        0,
        code_at.len(),
        &mut frames,
        visited,
        out,
        excluded,
        &mut lines,
        &mut dep_scopes,
    );

    out.push(FileEval {
        path: path.to_path_buf(),
        text,
        lines,
        dep_scopes,
    });
}

/// Process the statements of `code_at[idx..end_idx)` against the
/// caller's `frames` (empty at file level; body-local during a foreach
/// pass), pushing one [`LineAct`] per physical line into `lines` in
/// order.
#[allow(clippy::too_many_arguments)]
fn walk_statements(
    scope: &mut Scope,
    path: &Path,
    code_at: &[LineCode],
    idx: usize,
    end_idx: usize,
    frames: &mut Vec<Frame>,
    visited: &mut BTreeSet<PathBuf>,
    out: &mut Vec<FileEval>,
    excluded: &mut BTreeSet<PathBuf>,
    lines: &mut Vec<LineAct>,
    dep_scopes: &mut BTreeMap<usize, HashMap<String, Val>>,
) {
    let is_kw = |t: &str, kw: &str| {
        t == kw || (t.starts_with(kw) && t[kw.len()..].starts_with(char::is_whitespace))
    };
    let mut i = idx;
    while i < end_idx {
        let lc = code_at[i].clone();
        let trimmed = lc.code.trim();

        if !lc.started_in_string {
            if is_kw(trimmed, "foreach") {
                // find the matching endforeach (nested foreachs only;
                // if/endif inside the body is balanced on its own)
                let mut depth = 1usize;
                let mut body_end = end_idx; // unmatched: body = rest
                let mut j = i + 1;
                while j < end_idx {
                    let t = code_at[j].code.trim();
                    if is_kw(t, "foreach") {
                        depth += 1;
                    } else if t == "endforeach" {
                        depth -= 1;
                        if depth == 0 {
                            body_end = j;
                            break;
                        }
                    }
                    j += 1;
                }
                if let Some((var, list_expr)) = foreach_header(trimmed) {
                    // unroll only a bracket-balanced literal list on the
                    // header line; anything computed runs ONE pass with
                    // the loop variable Unknown (conservative — the
                    // subdir targets stay dynamic, the fallback scans them)
                    let items = if brackets_balanced(list_expr) {
                        match eval_expr(scope, list_expr) {
                            Val::Array(items) if !items.is_empty() => items,
                            _ => vec![Val::Unknown],
                        }
                    } else {
                        vec![Val::Unknown]
                    };
                    // per-item passes: the loop variable is bound in the
                    // SHARED scope (meson semantics), restored afterwards
                    let saved = scope.vars.get(&var).cloned();
                    let body_len = body_end.saturating_sub(i + 1);
                    let mut any_active = vec![false; body_len];
                    for item in items {
                        scope.vars.insert(var.clone(), item);
                        let mut pass_lines: Vec<LineAct> = Vec::new();
                        let mut pass_frames: Vec<Frame> = Vec::new();
                        walk_statements(
                            scope,
                            path,
                            code_at,
                            i + 1,
                            body_end,
                            &mut pass_frames,
                            visited,
                            out,
                            excluded,
                            &mut pass_lines,
                            dep_scopes,
                        );
                        for (k, l) in pass_lines.iter().enumerate() {
                            any_active[k] = any_active[k] || l.active;
                        }
                    }
                    match saved {
                        Some(old) => {
                            scope.vars.insert(var.clone(), old);
                        }
                        None => {
                            scope.vars.remove(&var);
                        }
                    }
                    // emit activity: header line, body lines (union over
                    // the item passes), footer line
                    let all_active = frames.iter().all(|f| f.active);
                    lines.push(LineAct {
                        start: lc.start,
                        end: lc.end,
                        active: all_active,
                        in_string: false,
                    });
                    for k in 0..body_len {
                        let blc = &code_at[i + 1 + k];
                        lines.push(LineAct {
                            start: blc.start,
                            end: blc.end,
                            active: any_active[k],
                            in_string: blc.started_in_string,
                        });
                    }
                    if body_end < end_idx {
                        let flc = &code_at[body_end];
                        lines.push(LineAct {
                            start: flc.start,
                            end: flc.end,
                            active: all_active,
                            in_string: flc.started_in_string,
                        });
                    }
                    i = body_end + 1;
                    continue;
                }
                // an unparseable header (two-var dict form, multi-line
                // header): fall through as a plain no-op statement
            }
            if is_kw(trimmed, "if") {
                let cond = trimmed[2..].trim();
                let tri = eval_expr(scope, cond).tri();
                frames.push(Frame {
                    resolved: tri == Tri::True,
                    prev_all_false: tri == Tri::False,
                    active: tri.reachable(),
                });
            } else if is_kw(trimmed, "elif") {
                let cond = trimmed[4..].trim();
                let tri = eval_expr(scope, cond).tri();
                if let Some(f) = frames.last_mut() {
                    let active = tri.reachable() && !f.resolved;
                    let definitely_taken = tri == Tri::True && f.prev_all_false;
                    f.resolved = f.resolved || definitely_taken;
                    f.prev_all_false = f.prev_all_false && tri == Tri::False;
                    f.active = active;
                }
            } else if trimmed == "else" {
                if let Some(f) = frames.last_mut() {
                    f.active = !f.resolved;
                    f.resolved = true; // `else` ends the chain
                }
            } else if trimmed == "endif" {
                frames.pop();
            } else if trimmed == "endforeach" || trimmed == "break" || trimmed == "continue" {
                // loop flow control: no static effect (replays process
                // every item)
            } else if frames.iter().all(|f| f.active) {
                // executable statement: bind variables, follow subdirs
                if let Some((ident, rhs)) = split_assignment(trimmed) {
                    let val = eval_expr(scope, rhs);
                    scope.vars.insert(ident, val);
                } else if let Some(target) = subdir_target(trimmed, scope) {
                    if let Some(dir) = target.filter(|d| !path_escapes(d)) {
                        // relative to THIS file's directory; executed
                        // now, in the same scope
                        let child = path
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(&dir)
                            .join("meson.build");
                        if child.is_file() {
                            eval_file(scope, &child, visited, out, excluded);
                        }
                    }
                }
                // a statement line that declares a dependency (or starts
                // / continues a multi-line dependency() call): snapshot
                // the live variable scope for the `required:` kwarg's
                // evaluation — it may reference variables bound just
                // above the call (`gssapi_opt = get_option('gssapi')`).
                // Continuation lines of a multi-line call pass through
                // here too (they match no keyword, no assignment), which
                // is exactly right: the call's kwarg lives on one of them.
                if lc.code.contains("dependency(") {
                    dep_scopes.insert(lc.start, scope.vars.clone());
                }
            } else {
                // a PROVABLY-dead branch (an undecided condition keeps
                // its branches reachable — see Frame): if it names a
                // statically-resolvable subdir() target, that DIRECTORY
                // is provably never entered — nor is anything under it —
                // and the fallback scan must not count its declarations.
                // This is how a foreach-dispatched backend
                // ("subdir(backend)" gated on
                // "@0@_enabled".format(backend) resolving to false) and
                // an option-gated component (`if get_option('compose')
                // subdir('compose/')` with a default-off option) drop
                // out of the scan.
                if let Some(Some(dir)) = subdir_target(trimmed, scope) {
                    if !path_escapes(&dir) {
                        let child_dir = path
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(&dir);
                        if child_dir.is_dir() {
                            excluded.insert(child_dir);
                        }
                    }
                }
            }
        }

        lines.push(LineAct {
            start: lc.start,
            end: lc.end,
            active: frames.iter().all(|f| f.active),
            in_string: lc.started_in_string,
        });
        i += 1;
    }
}

/// `foreach VAR : LIST` with a single loop variable (the two-variable
/// dict form and multi-line headers return None — those loops are not
/// statically unrollable and fall back to conservative behavior).
fn foreach_header(stmt: &str) -> Option<(String, &str)> {
    let rest = stmt.strip_prefix("foreach")?.trim_start();
    let colon = rest.find(':')?;
    let var = rest[..colon].trim();
    let valid = !var.is_empty()
        && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Some((var.to_string(), rest[colon + 1..].trim()))
    } else {
        None
    }
}

/// Whether a single-line expression's brackets balance outside string
/// literals (a multi-line list would otherwise unroll PARTIALLY — a
/// soundness hazard in the unsafe direction).
fn brackets_balanced(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => in_str = Some(c),
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth == 0
}

/// Whether a `subdir()` path tries to escape its own directory.
fn path_escapes(dir: &str) -> bool {
    Path::new(dir)
        .components()
        .any(|c| c == std::path::Component::ParentDir)
}

// ---------------------------------------------------------------------------
// line-level helpers
// ---------------------------------------------------------------------------

/// Quote kinds tracked while stripping comments.
#[derive(Clone, Copy, PartialEq)]
enum Quote {
    Single,
    Double,
    TripleSingle,
    TripleDouble,
}

/// Remove a trailing `# comment` (only outside string literals) and
/// track multi-line `'''…'''` / `"""…"""` state across lines.
fn strip_comment(line: &str, st: &mut Option<Quote>) -> String {
    let b = line.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match *st {
            None => {
                if b[i] == b'#' {
                    return line[..i].to_string();
                } else if b[i] == b'\'' {
                    if b[i..].starts_with(b"'''") {
                        *st = Some(Quote::TripleSingle);
                        i += 3;
                    } else {
                        *st = Some(Quote::Single);
                        i += 1;
                    }
                } else if b[i] == b'"' {
                    if b[i..].starts_with(b"\"\"\"") {
                        *st = Some(Quote::TripleDouble);
                        i += 3;
                    } else {
                        *st = Some(Quote::Double);
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            Some(Quote::Single) => {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'\'' {
                    *st = None;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Some(Quote::Double) => {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == b'"' {
                    *st = None;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Some(Quote::TripleSingle) => {
                if b[i..].starts_with(b"'''") {
                    *st = None;
                    i += 3;
                } else {
                    i += 1;
                }
            }
            Some(Quote::TripleDouble) => {
                if b[i..].starts_with(b"\"\"\"") {
                    *st = None;
                    i += 3;
                } else {
                    i += 1;
                }
            }
        }
    }
    line.to_string()
}

/// `ident = expr` (plain `=` only). Comparison (`==`), inequality
/// (`!=`), ordering (`<=`, `>=`) and list augmentation (`+=`) do not
/// rebind a platform fact, so only a bare identifier on the left of a
/// lone `=` counts.
fn split_assignment(stmt: &str) -> Option<(String, &str)> {
    let bytes = stmt.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c != b'=' {
            continue;
        }
        let prev = bytes.get(i.wrapping_sub(1)).copied().unwrap_or(b' ');
        let next = bytes.get(i + 1).copied().unwrap_or(b' ');
        if matches!(prev, b'=' | b'!' | b'<' | b'>' | b'+') || next == b'=' {
            continue;
        }
        let ident = stmt[..i].trim();
        let mut it = ident.bytes();
        let valid = !ident.is_empty()
            && it
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == b'_')
                .unwrap_or(false)
            && ident
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_');
        if !valid {
            return None;
        }
        let rhs = stmt[i + 1..].trim();
        return Some((ident.to_string(), rhs));
    }
    None
}

/// A `subdir('x')` statement: `Some(Some(path))` when the path is
/// statically computable (string literals, `/`/`+` joins, or a variable
/// holding a known string), `Some(None)` when it is dynamic (the caller
/// must not guess), `None` when the statement is not a subdir() call.
fn subdir_target(stmt: &str, scope: &Scope) -> Option<Option<String>> {
    let rest = stmt
        .strip_prefix("subdir")
        .map(|r| r.trim_start())
        .filter(|r| r.starts_with('('))
        .map(|r| &r[1..])?;
    // first argument only (subdir takes exactly one)
    let arg = rest.split(',').next().unwrap_or(rest);
    let arg = arg.trim().trim_end_matches(')').trim();
    match eval_path_arg(arg, scope) {
        Some(p) => Some(Some(p)),
        None => Some(None),
    }
}

/// The shared empty option table used where options cannot appear
/// (path expressions inside `subdir()` calls).
fn no_options() -> &'static OptTable {
    static EMPTY: std::sync::OnceLock<OptTable> = std::sync::OnceLock::new();
    EMPTY.get_or_init(OptTable::new)
}

/// `'x'`, `'a' / 'b'`, `'a' + 'b'`, or a scope variable holding a
/// string — the path shapes real subdir() calls use. Anything dynamic
/// is None.
fn eval_path_arg(arg: &str, scope: &Scope) -> Option<String> {
    let toks = tokenize(arg).ok()?;
    let mut parser = Parser {
        toks,
        pos: 0,
        system: "", // machines never appear in path expressions
        vars: &scope.vars,
        options: no_options(),
    };
    match parser.parse_or() {
        Val::Str(s) => Some(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// expression tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Question,
    Colon,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Slash,
    Eof,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, ()> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => break, // comment tail inside a condition
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b'?' => {
                out.push(Tok::Question);
                i += 1;
            }
            b':' => {
                out.push(Tok::Colon);
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'=' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Eq);
                    i += 2;
                } else {
                    return Err(());
                }
            }
            b'!' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    return Err(()); // meson spells negation `not`
                }
            }
            b'<' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Le);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            b'\'' | b'"' => {
                let q = c;
                if b[i..].starts_with(&[q, q, q][..]) {
                    let rest = &src[i + 3..];
                    let pat = [q as char; 3].iter().collect::<String>();
                    let Some(end) = rest.find(pat.as_str()) else {
                        return Err(());
                    };
                    out.push(Tok::Str(rest[..end].to_string()));
                    i += 3 + end + 3;
                } else {
                    let rest = &src[i + 1..];
                    let mut val = String::new();
                    let mut j = 0usize;
                    let mut closed = false;
                    let rb = rest.as_bytes();
                    while j < rb.len() {
                        if rb[j] == b'\\' && j + 1 < rb.len() {
                            val.push(unescape(rb[j + 1]));
                            j += 2;
                            continue;
                        }
                        if rb[j] == q {
                            closed = true;
                            break;
                        }
                        val.push(rb[j] as char);
                        j += 1;
                    }
                    if !closed {
                        return Err(());
                    }
                    out.push(Tok::Str(val));
                    i += 1 + j + 1;
                }
            }
            c if c.is_ascii_digit() => {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Tok::Num);
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok::Ident(src[start..i].to_string()));
            }
            _ => return Err(()),
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

fn unescape(c: u8) -> char {
    match c {
        b'n' => '\n',
        b't' => '\t',
        other => other as char,
    }
}

// ---------------------------------------------------------------------------
// expression parser + evaluator
// ---------------------------------------------------------------------------

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    system: &'a str,
    vars: &'a HashMap<String, Val>,
    options: &'a OptTable,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.toks.get(self.pos).cloned().unwrap_or(Tok::Eof);
        self.pos += 1;
        t
    }

    fn eat_ident(&mut self, kw: &str) -> bool {
        if let Tok::Ident(s) = self.peek() {
            if s == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// ternary := or ('?' ternary ':' ternary)? — meson's conditional
    /// expression, the shape libxml2's option logic uses heavily
    /// (`want_x = get_option('history').enabled() ? feature.allowed()
    /// : feature.enabled()`). An undecided condition stays Unknown
    /// unless both branches agree.
    fn parse_ternary(&mut self) -> Val {
        let cond = self.parse_or();
        if *self.peek() != Tok::Question {
            return cond;
        }
        self.next();
        let true_val = self.parse_ternary();
        if *self.peek() != Tok::Colon {
            return Val::Unknown; // malformed ternary
        }
        self.next();
        let false_val = self.parse_ternary();
        match cond.tri() {
            Tri::True => true_val,
            Tri::False => false_val,
            Tri::Unknown => {
                if true_val.eq_val(&false_val) == Tri::True {
                    true_val
                } else {
                    Val::Unknown
                }
            }
        }
    }

    /// or := and ('or' and)* — a lone operand passes through as its
    /// VALUE (`host_machine.system()`, array literals, strings): the
    /// tri-state coercion happens only when a boolean operator really
    /// combines two operands. Coercing unconditionally would erase the
    /// statically-known values assignments depend on.
    fn parse_or(&mut self) -> Val {
        let lhs = self.parse_and();
        if !self.eat_ident("or") {
            return lhs;
        }
        let mut acc = lhs.tri();
        loop {
            let rhs = self.parse_and().tri();
            acc = acc.or(rhs);
            if !self.eat_ident("or") {
                break;
            }
        }
        Val::tri_val(acc)
    }

    /// and := not ('and' not)* — same pass-through rule as parse_or.
    fn parse_and(&mut self) -> Val {
        let lhs = self.parse_not();
        if !self.eat_ident("and") {
            return lhs;
        }
        let mut acc = lhs.tri();
        loop {
            let rhs = self.parse_not().tri();
            acc = acc.and(rhs);
            if !self.eat_ident("and") {
                break;
            }
        }
        Val::tri_val(acc)
    }

    /// not := 'not' not | in — which also gives `x not in y` the
    /// correct `not (x in y)` reading.
    fn parse_not(&mut self) -> Val {
        if self.eat_ident("not") {
            // `not` applies to booleans only; other operand types are a
            // meson configure-time error -> undecidable (Unknown)
            let inner = self.parse_not().tri();
            return Val::tri_val(inner.not());
        }
        self.parse_in()
    }

    /// in := cmp ('in' cmp | 'not' 'in' cmp)?   (a LEADING `not` was
    /// absorbed by parse_not; this handles the trailing `not in`)
    fn parse_in(&mut self) -> Val {
        let lhs = self.parse_cmp();
        let negated = match self.peek() {
            Tok::Ident(s) if s == "in" => {
                self.pos += 1;
                false
            }
            Tok::Ident(s) if s == "not" => {
                // only a `not in` pair; a bare `not` here ends this level
                match self.toks.get(self.pos + 1) {
                    Some(Tok::Ident(nxt)) if nxt == "in" => {
                        self.pos += 2;
                        true
                    }
                    _ => return lhs,
                }
            }
            _ => return lhs,
        };
        let rhs = self.parse_cmp();
        let m = member_of(&lhs, &rhs);
        Val::tri_val(if negated { m.not() } else { m })
    }

    /// cmp := add (('=='|'!='|'<'|'<='|'>'|'>=') add)?
    fn parse_cmp(&mut self) -> Val {
        let lhs = self.parse_add();
        let ordered = matches!(self.peek(), Tok::Le | Tok::Lt | Tok::Ge | Tok::Gt);
        if *self.peek() == Tok::Eq || *self.peek() == Tok::Ne || ordered {
            let eq = *self.peek() == Tok::Eq;
            self.next();
            let rhs = self.parse_add();
            return if ordered {
                Val::Unknown // version ordering: not statically decidable
            } else if eq {
                Val::tri_val(lhs.eq_val(&rhs))
            } else {
                Val::tri_val(lhs.eq_val(&rhs).not())
            };
        }
        lhs
    }

    /// add := postfix (('+'|'/') postfix)* — string concat / path join
    fn parse_add(&mut self) -> Val {
        let mut acc = self.parse_postfix();
        loop {
            match self.peek() {
                Tok::Plus => {
                    self.next();
                    let rhs = self.parse_postfix();
                    acc = match (acc.clone(), rhs) {
                        (Val::Str(a), Val::Str(b)) => Val::Str(format!("{a}{b}")),
                        (Val::Array(a), Val::Array(b)) => {
                            let mut v = a;
                            v.extend(b);
                            Val::Array(v)
                        }
                        _ => Val::Unknown,
                    };
                }
                Tok::Slash => {
                    self.next();
                    let rhs = self.parse_postfix();
                    acc = match (acc.clone(), rhs) {
                        (Val::Str(a), Val::Str(b)) => Val::Str(format!("{a}/{b}")),
                        _ => Val::Unknown,
                    };
                }
                _ => break,
            }
        }
        acc
    }

    /// postfix := primary ('.' ident '(' args ')')*
    fn parse_postfix(&mut self) -> Val {
        let mut val = self.parse_primary();
        while *self.peek() == Tok::Dot {
            self.next();
            let Tok::Ident(method) = self.next() else {
                return Val::Unknown;
            };
            let mut args: Vec<Val> = Vec::new();
            if *self.peek() == Tok::LParen {
                self.next();
                if *self.peek() != Tok::RParen {
                    loop {
                        args.push(self.parse_ternary());
                        if *self.peek() == Tok::Comma {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                if *self.peek() == Tok::RParen {
                    self.next();
                }
            }
            val = self.call_method(&val, &method, &args);
        }
        val
    }

    fn call_method(&self, recv: &Val, method: &str, args: &[Val]) -> Val {
        match (recv, method) {
            // the machine introspection that platform gating uses
            (Val::Machine, "system") => Val::Str(self.system.to_string()),
            (Val::Machine, "cpu_family") => current_cpu_family(),
            (Val::Machine, "endian") => current_endian(),
            // kernel()/subsystem() have no reliable static answer
            (Val::Machine, _) => Val::Unknown,
            // array membership test
            (Val::Array(items), "contains") => {
                let mut acc = Tri::False;
                for it in items {
                    let hit = match args.first() {
                        Some(a) => it.eq_val(a),
                        None => Tri::Unknown,
                    };
                    acc = acc.or(hit);
                }
                Val::tri_val(acc)
            }
            // string formatting: '@0@'.format(x) — the idiom backend
            // dispatch uses to compose variable names
            // ('@0@_enabled'.format(backend))
            (Val::Str(s), "format") => {
                let mut out = s.clone();
                for (i, a) in args.iter().enumerate() {
                    let rep = match a {
                        Val::Str(x) => x.clone(),
                        Val::Bool(b) => b.to_string(),
                        _ => return Val::Unknown,
                    };
                    out = out.replace(&format!("@{i}@"), &rep);
                }
                Val::Str(out)
            }
            // feature.require(cond, ...): the feature stays as-is when
            // cond holds and becomes 'disabled' when it does not (meson
            // errors instead if the feature was forced enabled —
            // irrelevant for default analysis). Takes arguments — kept
            // OUTSIDE the zero-arg predicates arm below.
            (Val::Str(s), "require") if is_feature_state(s) => {
                match args.first().map(|a| a.tri()) {
                    Some(Tri::True) => Val::Str(s.clone()),
                    Some(Tri::False) => Val::Str("disabled".to_string()),
                    _ => Val::Unknown,
                }
            }
            // feature-option predicates and coercions: meson defines
            // these on feature values (and ONLY on feature values — the
            // receiver gate keeps plain strings out)
            (Val::Str(s), m) if args.is_empty() && is_feature_state(s) => match m {
                "enabled" => Val::Bool(s == "enabled"),
                "disabled" => Val::Bool(s == "disabled"),
                "auto" => Val::Bool(s == "auto"),
                "allowed" => Val::Bool(s != "disabled"),
                // auto -> disabled / auto -> enabled, value unchanged
                // otherwise (the meson idiom for forcing a definite
                // state out of an 'auto' feature: `required:
                // get_option('x').disable_auto()`)
                "disable_auto" => Val::Str(
                    if s == "auto" { "disabled" } else { s }.to_string(),
                ),
                "enable_auto" => Val::Str(
                    if s == "auto" { "enabled" } else { s }.to_string(),
                ),
                _ => Val::Unknown,
            },
            _ => Val::Unknown,
        }
    }

    /// primary := '(' ternary ')' | '[' args ']' | literal | machine | call
    fn parse_primary(&mut self) -> Val {
        match self.next() {
            Tok::LBracket => {
                let mut items = Vec::new();
                if *self.peek() != Tok::RBracket {
                    loop {
                        items.push(self.parse_ternary());
                        if *self.peek() == Tok::Comma {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                if *self.peek() == Tok::RBracket {
                    self.next();
                }
                Val::Array(items)
            }
            Tok::LParen => {
                let v = self.parse_ternary();
                if *self.peek() == Tok::RParen {
                    self.next();
                }
                v
            }
            Tok::Str(s) => Val::Str(s),
            Tok::Num => Val::Unknown,
            Tok::Ident(id) => self.ident_value(id),
            _ => Val::Unknown,
        }
    }

    fn ident_value(&mut self, id: String) -> Val {
        match id.as_str() {
            "true" => return Val::Bool(true),
            "false" => return Val::Bool(false),
            "host_machine" | "build_machine" | "target_machine" => return Val::Machine,
            _ => {}
        }
        if *self.peek() == Tok::LParen {
            // a call. `get_option('x')` / `get_variable('x')` — including
            // COMPUTED names ('@0@_enabled'.format(backend), the backend
            // dispatch idiom) — resolve against the option table /
            // variable scope; everything else (import(), dependency(),
            // …) is not statically decidable.
            self.next();
            if id == "get_option" || id == "get_variable" {
                let save = self.pos;
                if let Some(v) = self.resolve_name_lookup(&id) {
                    return v;
                }
                self.pos = save; // not a shape we understand: rewind
            }
            // consume balanced args
            let mut depth = 1usize;
            while depth > 0 {
                match self.next() {
                    Tok::LParen => depth += 1,
                    Tok::RParen => depth -= 1,
                    Tok::Eof => break,
                    _ => {}
                }
            }
            return Val::Unknown;
        }
        self.vars.get(&id).cloned().unwrap_or(Val::Unknown)
    }

    /// `get_option(EXPR)` / `get_variable(EXPR[, fallback])` where EXPR
    /// is a full expression (usually a string literal; the backend
    /// dispatch idiom computes it with `.format()`). Returns `None` when
    /// the argument list is not one of the understood shapes (the
    /// caller rewinds and treats the call as opaque).
    fn resolve_name_lookup(&mut self, id: &str) -> Option<Val> {
        let first = self.parse_or();
        if *self.peek() == Tok::RParen {
            self.next();
            if let Val::Str(name) = first {
                return Some(if id == "get_option" {
                    self.options
                        .get(&name)
                        .map(|o| o.default.clone())
                        .unwrap_or(Val::Unknown)
                } else {
                    self.vars.get(&name).cloned().unwrap_or(Val::Unknown)
                });
            }
            return Some(Val::Unknown); // computed a non-string name
        }
        if *self.peek() == Tok::Comma {
            // get_variable('name', fallback)
            self.next();
            let fallback = self.parse_or();
            if *self.peek() == Tok::RParen {
                self.next();
                if id == "get_variable" {
                    if let Val::Str(name) = first {
                        return Some(self.vars.get(&name).cloned().unwrap_or(fallback));
                    }
                }
                return Some(Val::Unknown);
            }
        }
        None
    }
}

/// `x in [..]` membership.
fn member_of(item: &Val, list: &Val) -> Tri {
    match (item, list) {
        (_, Val::Array(items)) => {
            let mut acc = Tri::False;
            for it in items {
                acc = acc.or(it.eq_val(item));
            }
            acc
        }
        _ => Tri::Unknown,
    }
}

/// Evaluate one meson expression in a scope. Any parse trouble or
/// unknown construct yields `Val::Unknown` (branch stays reachable).
fn eval_expr(scope: &Scope, src: &str) -> Val {
    let Ok(toks) = tokenize(src) else {
        return Val::Unknown;
    };
    let mut parser = Parser {
        toks,
        pos: 0,
        system: &scope.system,
        vars: &scope.vars,
        options: &scope.options,
    };
    parser.parse_ternary()
}

// ---------------------------------------------------------------------------
// `required:` kwarg semantics
// ---------------------------------------------------------------------------

/// The meson-semantics verdict on a `dependency()` call's `required:`
/// keyword argument — the difference between a dependency whose absence
/// fails the configure step and one the project merely *uses if present*:
///
/// * `required: true` (or no `required:` at all — the default) makes the
///   lookup mandatory;
/// * `required: false` makes it best-effort: a not-found dependency is
///   legal, the project checks `.found()` and carries on;
/// * `required: <feature option>` maps the option's state:
///   `'enabled'` behaves like `true`, `'auto'` like `false` (best-effort),
///   and `'disabled'` **skips the lookup entirely** — meson never even
///   asks for the dependency, so it is not a dependency of this
///   configuration at all (same category as a platform-gated call in a
///   dead branch).
///
/// Undecidable values (unknown options, computed booleans the scanner
/// cannot see) fall back to [`Requiredness::Required`] — the module's
/// one-sided rule: under-provisioning a real dependency breaks builds,
/// over-including an undecided one costs at most a report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requiredness {
    Required,
    Optional,
    Disabled,
}

/// Evaluate the `required:` kwarg of a `dependency()` call.
///
/// * `call_args` — the argument-list text INSIDE `dependency( … )`;
/// * `vars` — the variable-scope snapshot at the call site (values like
///   `required: gssapi_opt` reference variables assigned just above);
/// * `options` — the project's option table (`required:
///   get_option('x')` resolves against the option's default).
///
/// Returns the verdict plus a short human-readable reason for the
/// report (empty when required).
pub fn requiredness_of(
    call_args: &str,
    vars: Option<&HashMap<String, Val>>,
    options: &OptTable,
    system: &str,
) -> (Requiredness, String) {
    let empty = HashMap::new();
    let vars = vars.unwrap_or(&empty);
    for seg in split_top_level_args(call_args) {
        let Some((k, v)) = split_kwarg(&clean_segment(&seg)) else {
            continue;
        };
        if k != "required" {
            continue;
        }
        let val = {
            let Ok(toks) = tokenize(v.trim()) else {
                return (Requiredness::Required, String::new());
            };
            let mut parser = Parser {
                toks,
                pos: 0,
                system,
                vars,
                options,
            };
            parser.parse_ternary()
        };
        return match val {
            Val::Bool(true) => (Requiredness::Required, String::new()),
            Val::Bool(false) => (
                Requiredness::Optional,
                "required: false".to_string(),
            ),
            Val::Str(s) if is_feature_state(&s) => match s.as_str() {
                "enabled" => (Requiredness::Required, String::new()),
                "auto" => (
                    Requiredness::Optional,
                    "feature option resolving to 'auto' (best-effort)".to_string(),
                ),
                _ => (
                    Requiredness::Disabled,
                    "feature option resolving to 'disabled' (lookup skipped)".to_string(),
                ),
            },
            _ => (Requiredness::Required, String::new()),
        };
    }
    (Requiredness::Required, String::new())
}

/// The positional (name) arguments of a `dependency(…)` call, in declared
/// order — meson's multi-name fallback form
/// `dependency('libsystemd', 'libelogind', …)` tries each name in order
/// and uses the **first one that is found**.
///
/// Every positional argument of `dependency()` is a dependency name;
/// kwarg-shaped segments (`required:`, `version:`, `fallback:`, …) are
/// skipped. A call whose FIRST positional argument is not a string
/// literal (a variable/computed name) declares nothing statically
/// knowable and yields an EMPTY list — the same skip the single-name
/// recording applies. A dynamic positional later in the list is skipped
/// while the statically-known names around it keep their relative order
/// (meson tries the chain in sequence at runtime either way).
pub fn positional_string_names(call_args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen_positional = false;
    for seg in split_top_level_args(call_args) {
        let seg = clean_segment(&seg);
        if split_kwarg(&seg).is_some() {
            continue; // `key: value` — a kwarg, not a name
        }
        match string_literal(seg.trim()) {
            Some(name) => out.push(name),
            None if !seen_positional => {
                // dynamic PRIMARY name: the whole call is not statically
                // knowable
                return Vec::new();
            }
            None => {} // dynamic name later in the chain: skip it
        }
        seen_positional = true;
    }
    out
}

/// The first string item of a kwarg's array-literal value — meson's
/// `fallback: ['subproject', 'variable']` form, whose FIRST element
/// names the subproject (wrap) that satisfies the dependency when the
/// system lookup fails.
pub fn kwarg_first_string(call_args: &str, kwarg: &str) -> Option<String> {
    for seg in split_top_level_args(call_args) {
        if let Some((k, v)) = split_kwarg(&clean_segment(&seg)) {
            if k == kwarg {
                return string_array_items(v.trim()).into_iter().next();
            }
        }
    }
    None
}
