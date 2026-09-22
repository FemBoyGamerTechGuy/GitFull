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
//! | `subdir('path')` scope sharing, in order         | `subdir('src')`                        |
//! | `+` concatenation / `/` path join                | `subdir('utils' / 'vdf')`              |
//!
//! Everything else evaluates to *unknown* → the branch stays reachable.

use std::collections::{BTreeSet, HashMap};
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
}

// ---------------------------------------------------------------------------
// scope + evaluation driver
// ---------------------------------------------------------------------------

/// Shared variable scope for one meson project: the root file plus
/// every file pulled in by active `subdir()` calls, in execution order
/// (meson semantics: `subdir()` executes the child in the SAME scope).
struct Scope {
    system: String,
    vars: HashMap<String, Val>,
}

/// Analyze the conditional structure of a whole meson project for
/// `system`: the root `meson.build` plus every statically-reachable
/// `subdir()` child, in meson's execution order, with one shared
/// variable scope.
pub fn eval_project(root_file: &Path, system: &str) -> Vec<FileEval> {
    let mut scope = Scope {
        system: system.to_string(),
        vars: HashMap::new(),
    };
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out: Vec<FileEval> = Vec::new();
    eval_file(&mut scope, root_file, &mut visited, &mut out);
    out
}

/// Analyze one meson.build with a FRESH scope — for files not reachable
/// through static `subdir()` calls (dynamic paths, foreach-driven
/// subdirs). The file's own conditionals are still honored; it simply
/// cannot see the root file's variables.
pub fn eval_isolated(file: &Path, system: &str) -> FileEval {
    let mut scope = Scope {
        system: system.to_string(),
        vars: HashMap::new(),
    };
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out: Vec<FileEval> = Vec::new();
    eval_file(&mut scope, file, &mut visited, &mut out);
    out.into_iter().next().unwrap_or(FileEval {
        path: file.to_path_buf(),
        text: String::new(),
        lines: Vec::new(),
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

/// Process one file line by line: track `if`/`elif`/`else`/`endif`
/// frames, evaluate branch conditions against the shared scope, bind
/// variables from active assignments, and recurse into active
/// `subdir()` calls at the point meson would execute them.
fn eval_file(
    scope: &mut Scope,
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    out: &mut Vec<FileEval>,
) {
    if !visited.insert(path.to_path_buf()) {
        return; // the same file twice: meson errors; be tolerant
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut frames: Vec<Frame> = Vec::new();
    let mut lines: Vec<LineAct> = Vec::new();
    let mut line_start = 0usize;
    let mut in_string: Option<Quote> = None; // multi-line string state

    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        let line_end = line_start + body.len();
        let started_in_string = in_string.is_some();

        // strip comments quote-aware; tracks '''…''' across lines
        let code = strip_comment(body, &mut in_string);
        let trimmed = code.trim();

        if !started_in_string {
            let is_kw = |kw: &str| {
                trimmed == kw
                    || trimmed.starts_with(kw)
                        && trimmed[kw.len()..].starts_with(char::is_whitespace)
            };
            if is_kw("if") {
                let cond = trimmed[2..].trim();
                let tri = eval_expr(scope, cond).tri();
                frames.push(Frame {
                    resolved: tri == Tri::True,
                    prev_all_false: tri == Tri::False,
                    active: tri.reachable(),
                });
            } else if is_kw("elif") {
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
                            eval_file(scope, &child, visited, out);
                        }
                    }
                }
            }
        }

        lines.push(LineAct {
            start: line_start,
            end: line_end,
            active: frames.iter().all(|f| f.active),
            in_string: started_in_string,
        });
        line_start += raw.len();
    }

    out.push(FileEval {
        path: path.to_path_buf(),
        text,
        lines,
    });
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
                        args.push(self.parse_or());
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
            _ => Val::Unknown,
        }
    }

    /// primary := '(' or ')' | '[' args ']' | literal | machine | call
    fn parse_primary(&mut self) -> Val {
        match self.next() {
            Tok::LParen => {
                let v = self.parse_or();
                if *self.peek() == Tok::RParen {
                    self.next();
                }
                v
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                if *self.peek() != Tok::RBracket {
                    loop {
                        items.push(self.parse_or());
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
            // a call — get_option(), import(), dependency(), … none of
            // which are statically decidable: consume balanced args
            self.next();
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
    };
    parser.parse_or()
}
