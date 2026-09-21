//! Small std-only helpers: TTY detection, byte/duration formatting,
//! dotted-version comparison, PATH scanning, name sanitization.
//!
//! Everything here is hand-rolled on purpose: gitfull carries no utility
//! crates beyond `serde`/`toml` (see docs/AUDIT.md).

use std::cmp::Ordering;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch (used for audit/meta timestamps).
pub fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is the given file descriptor connected to a terminal?
///
/// Linux `/proc`-based check — avoids any libc dependency. Falls back to
/// `false` when `/proc` is unavailable (the safe, non-TTY rendering path).
pub fn is_tty(fd: u32) -> bool {
    match fs::read_link(format!("/proc/self/fd/{fd}")) {
        Ok(p) => {
            let s = p.to_string_lossy();
            s.starts_with("/dev/pts/") || s.starts_with("/dev/tty") || s == "/dev/console"
        }
        Err(_) => false,
    }
}

/// Is there a real interactive terminal on BOTH ends of a prompt —
/// stdout (where the question is printed) and stdin (where the answer is
/// read)?
///
/// Both ends must be terminals. A question printed to a captured or piped
/// stdout (CI logs, `cargo test` output capture, `… | tee`) is invisible,
/// and a read from a non-terminal stdin (pipe, file, `/dev/null`, closed
/// fd) never yields a human answer — it either returns garbage
/// immediately or blocks forever. Callers must treat `false` as "no
/// prompting is possible" and take their non-interactive path
/// **immediately**: never print the question, never attempt the read.
///
/// This is the classic `isatty(0) && isatty(1)` prompt-safety idiom (as
/// used by apt, git and ssh), which single-ended checks get wrong: a
/// session whose stdin is still an inherited terminal (makepkg run from
/// a console, a build coordinator's pty, a chroot `/dev/console`) is NOT
/// interactive just because fd 0 happens to look like a TTY.
pub fn is_interactive() -> bool {
    is_tty(0) && is_tty(1)
}

/// Terminal width from `$COLUMNS`, defaulting to 80.
pub fn term_width() -> usize {
    env::var("COLUMNS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&w| w >= 20)
        .unwrap_or(80)
}

/// Should ANSI colors be emitted on stderr?
pub fn colors_enabled(no_color_flag: bool, force: bool) -> bool {
    if force {
        return true;
    }
    if no_color_flag
        || env::var_os("NO_COLOR").is_some()
        || env::var("TERM").map(|t| t == "dumb").unwrap_or(false)
    {
        return false;
    }
    is_tty(2)
}

/// Basename of a program path (used by the forbidden-program check).
pub fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Host `$PATH` of the running gitfull process (only used to *locate*
/// external tools like `git`; children never inherit it wholesale).
pub fn host_path() -> String {
    env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
}

/// Find `prog` inside a PATH-style string, without invoking a shell.
pub fn find_in_path(prog: &str, path: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        return if Path::new(prog).is_file() {
            Some(PathBuf::from(prog))
        } else {
            None
        };
    }
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = Path::new(dir).join(prog);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Sanitize one name component: lowercase, `[a-z0-9._-]`, collapsed dashes.
pub fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    // collapse runs of '-', trim them, cap the length
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_dash = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_dash && !collapsed.is_empty() {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    while collapsed.ends_with('-') {
        collapsed.pop();
    }
    if collapsed.chars().count() > 48 {
        collapsed.chars().take(48).collect()
    } else {
        collapsed
    }
}

/// Stable sandbox name for a package: `<forge>-<owner-path>-<repo>`.
pub fn sanitize_name(forge: &str, owner: &str, repo: &str) -> String {
    let owner = owner.replace('/', "-");
    format!(
        "{}-{}-{}",
        sanitize_component(forge),
        sanitize_component(&owner),
        sanitize_component(repo)
    )
}

/// `true` when `s` is a valid repo/owner slug segment
/// (`[A-Za-z0-9._-]`, no leading '.', non-empty).
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Human-readable byte size: `1.2 MiB`, `980 B`, ...
pub fn format_bytes(bytes: f64) -> String {
    const K: f64 = 1024.0;
    let (v, unit) = if bytes >= K * K * K * K {
        (bytes / (K * K * K * K), "TiB")
    } else if bytes >= K * K * K {
        (bytes / (K * K * K), "GiB")
    } else if bytes >= K * K {
        (bytes / (K * K), "MiB")
    } else if bytes >= K {
        (bytes / K, "KiB")
    } else {
        (bytes, "B")
    };
    if unit == "B" {
        format!("{v:.0} {unit}")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// `mm:ss` or `h:mm:ss`.
pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// Compare dotted version strings, numerically per segment.
///
/// `13.9.0 < 13.10.0`; `1.2 == 1.2.0`; suffixes like `rc1` compare
/// lexicographically after the numeric part (so `1.5 < 1.5rc1`, which is
/// fine for our "prefer stable, filter pre-release tags" uses).
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let sa: Vec<&str> = a.split('.').collect();
    let sb: Vec<&str> = b.split('.').collect();
    let n = sa.len().max(sb.len());
    for i in 0..n {
        let x = sa.get(i).copied().unwrap_or("0");
        let y = sb.get(i).copied().unwrap_or("0");
        let (xn, xs) = split_num_suffix(x);
        let (yn, ys) = split_num_suffix(y);
        match xn.cmp(&yn) {
            Ordering::Equal => {}
            o => return o,
        }
        match xs.cmp(&ys) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

fn split_num_suffix(seg: &str) -> (u64, &str) {
    let digits = seg.chars().take_while(|c| c.is_ascii_digit()).count();
    let num: u64 = seg[..digits].parse().unwrap_or(0);
    (num, &seg[digits..])
}

/// Join PATH entries, dropping duplicates (first occurrence wins).
pub fn dedup_path(parts: &[String]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<&str> = Vec::new();
    for p in parts {
        if p.is_empty() {
            continue;
        }
        if seen.insert(p.clone()) {
            out.push(p.as_str());
        }
    }
    out.join(":")
}

/// Read a file to `String` if it exists.
pub fn read_file_if_exists(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
