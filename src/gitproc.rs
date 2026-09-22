//! The ONE place gitfull spawns subprocesses.
//!
//! Every external program execution — git, curl, compilers, make, ... —
//! goes through [`run`] / [`run_stream_stderr`] here, which guarantees:
//!
//! 1. **Forbidden programs are hard-denied** regardless of caller: host
//!    package managers (`pacman`, `apt`, `dnf`, `xbps`, ...) and privilege
//!    escalators (`sudo`, `doas`, ...). This is the structural enforcement
//!    of "gitfull never shells out to a system package manager".
//! 2. **The child environment is exactly what the caller specifies**
//!    (`env_clear()` + explicit envs): nothing leaks from the host, and
//!    builds cannot accidentally see host tools beyond the configured
//!    POSIX-utility PATH.
//! 3. **Every invocation is appended to the audit log** (`<root>/audit.log`)
//!    with its exec class, resolved program path, redacted arguments, and
//!    exit status.
//!
//! Exec classes (see docs/AUDIT.md):
//!
//! | class                | who                                        | example |
//! |----------------------|--------------------------------------------|---------|
//! | `SeedHostCompiler`   | the ONE sanctioned host-compiler use       | cc for seed GCC |
//! | `FetchTool`          | source fetchers, sealed env, cache-only writes | git, curl |
//! | `ForgeApi`           | read-only forge REST queries (search/rank) | curl GET |
//! | `HostUtility`        | POSIX utilities on sandbox paths           | sh, tar |
//! | `Toolchain`          | toolchain-managed compilers/build tools    | gcc, meson, ninja |

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use crate::error::{GitfullError, Result};
use crate::util::{basename, epoch, find_in_path};

/// Built-in forbidden programs.
///
/// Host package managers are explicitly out of bounds for gitfull, as are
/// privilege escalators (build recipes must never escalate). Users can
/// extend this list with `[policy] extra_forbidden_programs`.
pub const BUILTIN_FORBIDDEN: &[&str] = &[
    // package managers
    "pacman",
    "pacman-g2",
    "apt",
    "apt-get",
    "aptitude",
    "dpkg",
    "dnf",
    "dnf5",
    "yum",
    "microdnf",
    "rpm",
    "zypper",
    "urpmi",
    "xbps-install",
    "xbps-query",
    "xbps-remove",
    "xbps-src",
    "apk",
    "emerge",
    "nix",
    "nix-env",
    "nix-shell",
    "nix-build",
    "guix",
    "flatpak",
    "snap",
    "brew",
    "port",
    "fink",
    "opkg",
    "swupd",
    "tazpkg",
    "kiss",
    "cards",
    // privilege escalation
    "sudo",
    "doas",
    "pkexec",
    "su",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecClass {
    /// The single sanctioned use of the host's system compiler: building
    /// the seed GCC toolchain (once). Never valid afterwards.
    SeedHostCompiler,
    /// Source fetchers (`git`, `curl`): sealed environment, write only
    /// inside the cache.
    FetchTool,
    /// Read-only forge REST API queries (`curl -i GET`) used by the
    /// search/ranking resolver. Network reads only, **no tokens are ever
    /// attached**, no writes anywhere.
    ForgeApi,
    /// POSIX utilities (`sh`, `tar`, ...) operating on sandbox paths.
    HostUtility,
    /// Toolchain-managed compilers and build tools (everything after the
    /// seed bootstrap).
    Toolchain,
}

impl ExecClass {
    pub fn label(self) -> &'static str {
        match self {
            ExecClass::SeedHostCompiler => "seed-host-compiler",
            ExecClass::FetchTool => "fetch-tool",
            ExecClass::ForgeApi => "forge-api",
            ExecClass::HostUtility => "host-utility",
            ExecClass::Toolchain => "toolchain",
        }
    }
}

/// Execution context: audit log target, policy extensions, secrets to
/// redact from logs, and the PATH used to resolve bare program names (the
/// child's hermetic PATH — never implicitly the host's).
///
/// `interactive_override` is the prompt-policy seam: `None` auto-detects
/// from the real terminal state, `Some(_)` forces a deterministic answer
/// (tests simulate a non-interactive session no matter what fds they
/// inherited from the test runner).
#[derive(Debug, Clone, Default)]
pub struct ExecCtx {
    pub audit_log: Option<PathBuf>,
    pub extra_forbidden: Vec<String>,
    pub redactions: Vec<String>,
    pub resolve_path: String,
    /// `None` = detect interactivity from the real terminal state;
    /// `Some(false)` = this session may never prompt (tests, embedders);
    /// `Some(true)` = prompts allowed regardless of ambient fds.
    pub interactive_override: Option<bool>,
}

impl ExecCtx {
    pub fn forbidden(&self, program: &str) -> bool {
        let base = basename(program);
        BUILTIN_FORBIDDEN.contains(&base) || self.extra_forbidden.iter().any(|x| x == base)
    }

    /// May this session show an interactive prompt and read the answer?
    ///
    /// Auto-detection requires BOTH ends of the prompt to be real
    /// terminals: the question is written to stdout, the answer read from
    /// stdin (see [`crate::util::is_interactive`]). Either end not a TTY —
    /// piped stdout, captured test output, `/dev/null` stdin, an inherited
    /// but unserviced pty — means the prompt can never be seen and/or
    /// answered.
    ///
    /// Callers MUST treat `false` as "take the non-interactive path
    /// immediately": print nothing, attempt NO blocking stdin read. A
    /// blocking read under a false-positive TTY check is exactly the
    /// packaging-pipeline hang (`cargo test` inside `makepkg`/CI inheriting
    /// a terminal on fd 0 that nobody is typing into).
    pub fn interactive(&self) -> bool {
        self.interactive_override
            .unwrap_or_else(crate::util::is_interactive)
    }
}

/// Deny-by-name check applied to every spawn.
pub fn ensure_allowed(program: &str, ctx: &ExecCtx) -> Result<()> {
    if ctx.forbidden(program) {
        return Err(GitfullError::Policy {
            program: basename(program).to_string(),
            reason: "host package manager / privilege escalator".to_string(),
        });
    }
    Ok(())
}

fn status_text(status: &ExitStatus) -> String {
    match status.code() {
        Some(0) => "ok".to_string(),
        Some(c) => format!("exit {c}"),
        None => format!("terminated by signal"),
    }
}

fn redact_str(s: &str, secrets: &[String]) -> String {
    let mut out = s.to_string();
    for sec in secrets {
        if !sec.is_empty() {
            out = out.replace(sec.as_str(), "REDACTED");
        }
    }
    out
}

/// Append one event to the audit log. Format (tab-separated):
/// `<epoch>\texec\t<class>\t<program>\t<args>\t<cwd>\t<status>`
pub fn audit_exec(
    ctx: &ExecCtx,
    class: ExecClass,
    program: &Path,
    args: &[String],
    cwd: &Path,
    status: &str,
) {
    let Some(log) = &ctx.audit_log else { return };
    if let Some(parent) = log.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let args_s = redact_str(&args.join(" "), &ctx.redactions);
    let line = format!(
        "{}\texec\t{}\t{}\t{}\t{}\t{}\n",
        epoch(),
        class.label(),
        program.display(),
        args_s,
        cwd.display(),
        status
    );
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// Append a non-exec audit event (binary installs/removals — the sandbox
/// escape path).
pub fn audit_event(ctx: &ExecCtx, kind: &str, detail: &str) {
    let Some(log) = &ctx.audit_log else { return };
    if let Some(parent) = log.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = format!("{}\t{}\t{}\n", epoch(), kind, detail);
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

fn resolve_program(program: &str, ctx: &ExecCtx) -> Result<PathBuf> {
    find_in_path(program, &ctx.resolve_path).ok_or_else(|| GitfullError::Exec {
        program: program.to_string(),
        status: "not found in hermetic PATH".to_string(),
        log: None,
        tail: format!("PATH={}", ctx.resolve_path),
    })
}

fn build_command(
    ctx: &ExecCtx,
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(PathBuf, Command)> {
    assert!(!argv.is_empty(), "empty argv");
    ensure_allowed(&argv[0], ctx)?;
    let prog = resolve_program(&argv[0], ctx)?;
    let mut cmd = Command::new(&prog);
    cmd.args(&argv[1..]);
    cmd.env_clear();
    cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.current_dir(cwd);
    Ok((prog, cmd))
}

/// Run a command, capturing stdout/stderr. Returns stdout on success; on
/// failure the error carries the last stderr lines and the log file path.
pub fn run(
    ctx: &ExecCtx,
    argv: &[String],
    class: ExecClass,
    cwd: &Path,
    env: &[(String, String)],
    log_file: Option<&Path>,
) -> Result<String> {
    let (prog, mut cmd) = build_command(ctx, argv, cwd, env)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let _t0 = Instant::now();
    let out = cmd.output()?;
    let st = status_text(&out.status);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if let Some(lf) = log_file {
        write_step_log(lf, argv, &stdout, &stderr, &ctx.redactions)?;
    }
    audit_exec(ctx, class, &prog, &argv[1..], cwd, &st);

    if !out.status.success() {
        let tail: Vec<&str> = stderr
            .lines()
            .rev()
            .take(25)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(GitfullError::Exec {
            program: argv[0].clone(),
            status: st,
            log: log_file.map(|p| p.to_path_buf()),
            tail: tail.join("\n"),
        });
    }
    Ok(stdout)
}

/// Run a command, streaming stderr line-by-line (used for clone progress);
/// stdout is discarded.
pub fn run_stream_stderr(
    ctx: &ExecCtx,
    argv: &[String],
    class: ExecClass,
    cwd: &Path,
    env: &[(String, String)],
    mut on_line: impl FnMut(&str),
) -> Result<()> {
    let (prog, mut cmd) = build_command(ctx, argv, cwd, env)?;
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut tail_lines: Vec<String> = Vec::new();
    loop {
        let n = stderr_pipe.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Extract complete lines (git uses \r for progress updates).
        while let Some(pos) = buf.iter().position(|&b| b == b'\r' || b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            if line.is_empty() {
                continue;
            }
            if tail_lines.len() >= 25 {
                tail_lines.remove(0);
            }
            tail_lines.push(line.clone());
            on_line(&line);
        }
    }
    let status = child.wait()?;
    let st = status_text(&status);
    audit_exec(ctx, class, &prog, &argv[1..], cwd, &st);
    if !status.success() {
        return Err(GitfullError::Exec {
            program: argv[0].clone(),
            status: st,
            log: None,
            tail: tail_lines.join("\n"),
        });
    }
    Ok(())
}

fn write_step_log(
    path: &Path,
    argv: &[String],
    stdout: &str,
    stderr: &str,
    redactions: &[String],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let argv_s = redact_str(&argv.join(" "), redactions);
    let mut text = format!(
        "$ {}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        argv_s, stdout, stderr
    );
    if !text.ends_with('\n') {
        text.push('\n');
    }
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sealed git operations
// ---------------------------------------------------------------------------

/// A clone URL with optional embedded authentication. `clean` is used for
/// all human-facing output; `authed` (token embedded) exists only in the
/// child process arguments and is redacted from every log.
#[derive(Debug, Clone)]
pub struct RemoteUrl {
    pub clean: String,
    pub authed: String,
    pub token: Option<String>,
}

pub fn authed_url(url: &str, token: Option<&str>) -> RemoteUrl {
    let token = token.filter(|t| !t.is_empty());
    match (&token, url.strip_prefix("https://")) {
        (Some(t), Some(rest)) if !rest.contains('@') => RemoteUrl {
            clean: url.to_string(),
            authed: format!("https://x-access-token:{t}@{rest}"),
            token: Some(t.to_string()),
        },
        _ => RemoteUrl {
            clean: url.to_string(),
            authed: url.to_string(),
            token: None,
        },
    }
}

/// The sealed environment every git invocation runs under. `home` is a
/// gitfull-controlled directory (sandbox or cache); host gitconfig and SSH
/// configuration are invisible to the child.
pub fn git_env(home: &Path, host_tool_path: &str) -> Vec<(String, String)> {
    let gitconfig = home.join(".gitconfig");
    let _ = fs::create_dir_all(home);
    let _ = fs::write(&gitconfig, b""); // empty global config

    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("PATH".into(), host_tool_path.to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), gitconfig.display().to_string()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("LC_ALL".into(), "C".into()),
    ];
    if let Some(t) = find_in_path("true", host_tool_path) {
        env.push(("GIT_ASKPASS".into(), t.display().to_string()));
    }
    // Proxy settings are the one host setting passed through, because
    // sealed clones still need to reach the network in proxied
    // environments. Documented in docs/AUDIT.md.
    for k in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.to_string(), v));
        }
    }
    env
}

/// Does a failed clone's stderr tail look like an authentication
/// rejection? Matches the server-side and git-side phrasings observed in
/// the wild, including the GitLab **"HTTP Basic: Access denied"** page —
/// which a fully anonymous, credential-less request can receive (the
/// gitlab.freedesktop.org/appstream incident): GitLab serves that canned
/// auth-error page whenever it decides git-HTTP needs authentication,
/// regardless of whether the client offered credentials. A plain tail
/// match is the honest classification — the *anonymous vs tokened*
/// distinction (and the right remedy) is decided by the caller from
/// whether a token was actually attached.
pub fn clone_tail_is_auth_rejection(tail: &str) -> bool {
    let t = tail.to_ascii_lowercase();
    t.contains("access denied")
        || t.contains("authentication failed")
        || t.contains("could not read username")
        || t.contains("terminal prompts disabled")
}

/// Clone `url` into `dest` with gitfull's live progress UI.
///
/// `git` runs as a **sealed fetch tool**: disabled credential helpers, no
/// system/global gitconfig, redirected HOME, no terminal prompts. Its only
/// writes go to `dest` (inside a sandbox or the cache).
///
/// On failure, a tail that looks like an authentication rejection is
/// reclassified into [`GitfullError::CloneAuth`] — anonymous clones then
/// fail with an error that states, affirmatively, that **no credentials
/// were sent** (the server refused anonymous access), instead of the raw
/// remote message that reads exactly like a leaked-credential bug.
pub fn git_clone(
    ctx: &ExecCtx,
    url: &RemoteUrl,
    dest: &Path,
    git_ref: Option<&str>,
    clone: &crate::config::CloneSection,
    host_tool_path: &str,
    git_home: &Path,
    mut progress: Option<&mut crate::progress::ProgressUi>,
) -> Result<()> {
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        "credential.helper=".to_string(),
        "clone".to_string(),
        "--progress".to_string(),
    ];
    if let Some(r) = git_ref {
        argv.push("--branch".into());
        argv.push(r.to_string());
    }
    if let Some(d) = clone.depth {
        argv.push("--depth".into());
        argv.push(d.to_string());
    }
    if clone.single_branch_on() {
        argv.push("--single-branch".into());
    }
    if clone.recurse_submodules {
        argv.push("--recurse-submodules".into());
    }
    argv.push(url.authed.clone());
    argv.push(dest.display().to_string());

    // secrets must never reach the audit log
    let mut ctx = ctx.clone();
    if let Some(t) = &url.token {
        ctx.redactions.push(t.clone());
    }

    let env = git_env(git_home, host_tool_path);
    let res = run_stream_stderr(
        &ctx,
        &argv,
        ExecClass::FetchTool,
        dest.parent().unwrap_or(Path::new(".")),
        &env,
        |line| {
            if let Some(ui) = progress.as_deref_mut() {
                if let Some(sample) = crate::progress::parse_git_progress(line) {
                    ui.update(&sample);
                }
            }
        },
    );
    if let Err(GitfullError::Exec { tail, .. }) = &res {
        if clone_tail_is_auth_rejection(tail) {
            return Err(GitfullError::CloneAuth {
                url: url.clean.clone(),
                anonymous: url.token.is_none(),
                tail: tail.clone(),
            });
        }
    }
    res
}

/// `git ls-remote --tags <url> <pattern>` — used to auto-resolve the
/// latest release tag (e.g. GCC) without any forge API.
pub fn git_ls_remote_tags(
    ctx: &ExecCtx,
    url: &str,
    pattern: &str,
    host_tool_path: &str,
    git_home: &Path,
) -> Result<Vec<String>> {
    let argv = vec![
        "git".to_string(),
        "ls-remote".to_string(),
        "--tags".to_string(),
        url.to_string(),
        pattern.to_string(),
    ];
    let env = git_env(git_home, host_tool_path);
    let out = run(ctx, &argv, ExecClass::FetchTool, Path::new("."), &env, None)?;
    let mut refs = Vec::new();
    for line in out.lines() {
        if let Some((_sha, r)) = line.split_once('\t') {
            if r.ends_with("^{}") {
                continue; // peeled annotation entries
            }
            refs.push(r.to_string());
        }
    }
    Ok(refs)
}

/// `git rev-parse HEAD` in an existing checkout (sealed env).
pub fn git_rev_parse_head(
    ctx: &ExecCtx,
    repo: &Path,
    host_tool_path: &str,
    git_home: &Path,
) -> Result<String> {
    let argv = vec![
        "git".to_string(),
        "-C".to_string(),
        repo.display().to_string(),
        "rev-parse".to_string(),
        "HEAD".to_string(),
    ];
    let env = git_env(git_home, host_tool_path);
    let out = run(ctx, &argv, ExecClass::FetchTool, repo, &env, None)?;
    Ok(out.trim().to_string())
}

/// Download a tarball with `curl` (FetchTool). Only used for toolchain
/// components whose upstream is not a git repository (e.g. rustc dist).
pub fn curl_download(ctx: &ExecCtx, url: &str, dest: &Path, host_tool_path: &str) -> Result<()> {
    let argv = vec![
        "curl".to_string(),
        "--fail".into(),
        "--location".into(),
        "--silent".into(),
        "--show-error".into(),
        "--output".into(),
        dest.display().to_string(),
        url.to_string(),
    ];
    let env: Vec<(String, String)> = vec![
        ("PATH".into(), host_tool_path.to_string()),
        ("LC_ALL".into(), "C".into()),
    ];
    run(ctx, &argv, ExecClass::FetchTool, Path::new("."), &env, None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Forge REST queries (search / ranking)
// ---------------------------------------------------------------------------

/// One `curl -i GET` response, split into status line, headers, and body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u32,
    /// Header names lowercased (`link`, `x-ratelimit-remaining`, ...).
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Read-only HTTP GET via `curl -i`, classified as `ForgeApi`.
///
/// * never attaches any authentication (public endpoints only);
/// * response headers are kept so paginated counts can be read from the
///   `Link` header (`rel="last"`);
/// * every call is audit-logged with its URL — the URL is the only input
///   and contains no secrets.
pub fn http_get(
    ctx: &ExecCtx,
    url: &str,
    host_tool_path: &str,
    accept: Option<&str>,
    max_time_secs: u64,
) -> Result<HttpResponse> {
    let mut argv = vec![
        "curl".to_string(),
        "--silent".into(),
        "--show-error".into(),
        "--include".into(),
        "--max-time".to_string(),
        max_time_secs.to_string(),
        "-A".into(),
        format!("gitfull/{}", crate::VERSION),
    ];
    if let Some(acc) = accept {
        argv.push("-H".into());
        argv.push(format!("Accept: {acc}"));
    }
    argv.push(url.to_string());
    let env: Vec<(String, String)> = vec![
        ("PATH".into(), host_tool_path.to_string()),
        ("LC_ALL".into(), "C".into()),
    ];
    let out = run(ctx, &argv, ExecClass::ForgeApi, Path::new("."), &env, None)?;
    Ok(parse_http_include(&out))
}

/// Fetch a plain-text/TOML resource as a String (FetchTool — same class as
/// other toolchain source fetches; used for the rust dist channel file).
pub fn curl_text(ctx: &ExecCtx, url: &str, host_tool_path: &str) -> Result<String> {
    let argv = vec![
        "curl".to_string(),
        "--fail".into(),
        "--location".into(),
        "--silent".into(),
        "--show-error".into(),
        "--max-time".into(),
        "60".into(),
        "-A".into(),
        format!("gitfull/{}", crate::VERSION),
        url.to_string(),
    ];
    let env: Vec<(String, String)> = vec![
        ("PATH".into(), host_tool_path.to_string()),
        ("LC_ALL".into(), "C".into()),
    ];
    run(ctx, &argv, ExecClass::FetchTool, Path::new("."), &env, None)
}

/// Split `curl -i` output (ONE header block + body — http_get never passes
/// `--location`, so redirects are returned as-is, not followed) into
/// status, headers, and body. A body that itself starts with `HTTP/` is
/// therefore left verbatim in the body.
fn parse_http_include(raw: &str) -> HttpResponse {
    let mut status = 0u32;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body = raw;
    if raw.starts_with("HTTP/") {
        let (block_end, sep_len) = match raw.find("\r\n\r\n") {
            Some(i) => (i, 4),
            None => match raw.find("\n\n") {
                Some(i) => (i, 2),
                None => (raw.len(), 0), // malformed: treat everything as headers
            },
        };
        let block = &raw[..block_end];
        for (i, line) in block.lines().enumerate() {
            if i == 0 {
                // "HTTP/1.1 200 OK"
                if let Some(code) = line.split_whitespace().nth(1) {
                    status = code.parse().unwrap_or(0);
                }
            } else if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        body = &raw[block_end + sep_len..];
    }
    HttpResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

#[cfg(test)]
mod sealed_clone_tests {
    use super::*;
    use crate::config::CloneSection;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Run a test body under a HARD deadline: a regression that turns a
    /// clone into a blocking read (the packaging-pipeline hang failure
    /// mode) fails loudly here instead of hanging the runner.
    fn with_deadline<T: Send + 'static>(secs: u64, body: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .spawn(move || {
                let _ = tx.send(body());
            })
            .expect("spawn watchdog worker");
        rx.recv_timeout(Duration::from_secs(secs)).unwrap_or_else(|_| {
            panic!(
                "test body did not finish within {secs}s — a blocking \
                 operation (network read, prompt) hung; tests must never hang"
            )
        })
    }

    /// A fake `git` that records the exact argv and environment it was
    /// executed with into `$HOME/shim-argv` / `$HOME/shim-env` and exits 0.
    /// `$HOME` is gitfull's sealed git home, so the dumps land where the
    /// test can read them — this observes the REAL spawned child, not a
    /// reconstruction of it.
    fn write_git_shim(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let shim = dir.join("git");
        fs::write(
            &shim,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/shim-argv\"\n/usr/bin/env > \"$HOME/shim-env\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn temp_case(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gitfull-gitproc-{}-{}-{name}",
            std::process::id(),
            epoch()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// THE exact-invocation proof for the incident report: an anonymous
    /// generic-remote clone must carry **zero credentials** on either side
    /// of the exec — no token in argv, no ambient secret in the child
    /// environment, no credential helper, empty global config, prompts
    /// off — while a decoy GitHub PAT sits in gitfull's own environment
    /// (the "stale credential / leaked PAT" hypothesis). What the shim
    /// records is what git would have seen; nothing else exists.
    #[test]
    fn anonymous_generic_clone_sends_no_credentials() {
        let dir = temp_case("anon");
        let shim_dir = dir.join("shim");
        let git_home = dir.join("githome");
        let dest_parent = dir.join("sbx");
        fs::create_dir_all(&git_home).unwrap();
        fs::create_dir_all(&dest_parent).unwrap();
        write_git_shim(&shim_dir);

        // the decoy: a PAT-looking secret in gitfull's OWN environment
        let sentinel = "ghp_DecoySentinelNotARealToken";
        std::env::set_var("GITHUB_TOKEN", sentinel);
        std::env::set_var("GITFULL_TEST_SENTINEL_PAT", sentinel);

        let ctx = ExecCtx {
            audit_log: Some(dir.join("audit.log")),
            redactions: Vec::new(),
            resolve_path: format!("{}:/usr/bin:/bin", shim_dir.display()),
            interactive_override: Some(false),
            ..ExecCtx::default()
        };
        let url = "https://gitlab.freedesktop.org/appstream/appstream";
        let remote = authed_url(url, None);

        let ctx2 = ctx.clone();
        let remote2 = remote.clone();
        let dest = dest_parent.join("repo");
        let tool_path = format!("{}:/usr/bin:/bin", shim_dir.display());
        let git_home2 = git_home.clone();
        let res = with_deadline(30, move || {
            git_clone(
                &ctx2,
                &remote2,
                &dest,
                None,
                &CloneSection::default(),
                &tool_path,
                &git_home2,
                None,
            )
        });
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("GITFULL_TEST_SENTINEL_PAT");
        res.expect("shim git exits 0");

        // ---- the argv git was actually executed with -------------------
        let argv = fs::read_to_string(git_home.join("shim-argv")).unwrap();
        let args: Vec<&str> = argv.lines().collect();
        assert!(
            args.contains(&"credential.helper="),
            "credential helpers must be reset on the command line: {args:?}"
        );
        assert!(
            args.contains(&"--progress"),
            "clone progress expected: {args:?}"
        );
        assert!(
            args.contains(&url),
            "the CLEAN url must be the clone target: {args:?}"
        );
        assert!(
            !argv.contains("x-access-token:"),
            "no token may be embedded in the URL: {argv}"
        );
        assert!(
            !argv.contains(sentinel),
            "the ambient PAT must not appear anywhere in argv: {argv}"
        );

        // ---- the environment git was actually executed under -----------
        let env_dump = fs::read_to_string(git_home.join("shim-env")).unwrap();
        let mandatory = [
            ("GIT_TERMINAL_PROMPT=0", "terminal prompts disabled"),
            ("GIT_CONFIG_NOSYSTEM=1", "system gitconfig invisible"),
        ];
        for (needle, why) in mandatory {
            assert!(env_dump.contains(needle), "{why} missing: {env_dump}");
        }
        let global = format!("GIT_CONFIG_GLOBAL={}", git_home.join(".gitconfig").display());
        assert!(
            env_dump.contains(&global),
            "global config must be the sealed empty file: {env_dump}"
        );
        let home_line = format!("HOME={}", git_home.display());
        assert!(env_dump.contains(&home_line), "HOME must be redirected: {env_dump}");
        assert!(
            env_dump.contains("GIT_ASKPASS=/usr/bin/true"),
            "askpass must answer every credential prompt with nothing: {env_dump}"
        );
        assert!(
            !env_dump.contains(sentinel),
            "the ambient PAT must not leak into the child env: {env_dump}"
        );
        assert!(
            !env_dump.contains("GITHUB_TOKEN="),
            "no ambient token var may pass env_clear: {env_dump}"
        );
        // the allowlist: git_env's own keys, proxy pass-through, and the
        // shell's own bookkeeping — NOTHING else may cross env_clear()
        let allowed: &[&str] = &[
            "HOME", "PATH", "LC_ALL", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL",
            "GIT_TERMINAL_PROMPT", "GIT_ASKPASS", "PWD", "SHLVL", "OLDPWD", "IFS",
            "PS1", "PS2", "_", "http_proxy", "https_proxy", "all_proxy", "no_proxy",
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
        ];
        for line in env_dump.lines() {
            let key = line.split('=').next().unwrap_or("");
            assert!(
                allowed.contains(&key),
                "unexpected env key `{key}` crossed env_clear(): {env_dump}"
            );
        }
        // the sealed global config itself: present and EMPTY
        let gitconfig = git_home.join(".gitconfig");
        assert!(gitconfig.is_file(), "sealed gitconfig must exist");
        assert_eq!(
            fs::read_to_string(&gitconfig).unwrap(),
            "",
            "sealed gitconfig must be empty"
        );

        // ---- audit log: the invocation, redacted -------------------------
        let audit = fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(audit.contains("fetch-tool"), "class logged: {audit}");
        assert!(
            !audit.contains(sentinel),
            "secrets must never reach the audit log: {audit}"
        );
    }

    /// The contrast arm: a clone of a forge-configured host DOES embed the
    /// token in the child argv (its only legitimate channel) — and the
    /// audit log then shows the token REDACTED. Scoping: token attached
    /// exactly when a forge supplied one, never otherwise.
    #[test]
    fn forge_token_lives_only_in_child_argv_and_is_redacted_in_logs() {
        let dir = temp_case("authed");
        let shim_dir = dir.join("shim");
        let git_home = dir.join("githome");
        let dest_parent = dir.join("sbx");
        fs::create_dir_all(&git_home).unwrap();
        fs::create_dir_all(&dest_parent).unwrap();
        write_git_shim(&shim_dir);

        let ctx = ExecCtx {
            audit_log: Some(dir.join("audit.log")),
            redactions: Vec::new(),
            resolve_path: format!("{}:/usr/bin:/bin", shim_dir.display()),
            interactive_override: Some(false),
            ..ExecCtx::default()
        };
        let token = "ghp_SentinelForgeToken42";
        let remote = authed_url("https://github.com/acme/widgets.git", Some(token));
        assert_eq!(remote.clean, "https://github.com/acme/widgets.git");
        assert_eq!(
            remote.authed,
            format!("https://x-access-token:{token}@github.com/acme/widgets.git")
        );

        git_clone(
            &ctx,
            &remote,
            &dest_parent.join("repo"),
            None,
            &CloneSection::default(),
            &format!("{}:/usr/bin:/bin", shim_dir.display()),
            &git_home,
            None,
        )
        .expect("shim git exits 0");

        let argv = fs::read_to_string(git_home.join("shim-argv")).unwrap();
        assert!(
            argv.contains(&remote.authed),
            "the authed URL is the ONE place the token exists: {argv}"
        );
        let audit = fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(
            audit.contains("REDACTED"),
            "the audit log must redact the token: {audit}"
        );
        assert!(!audit.contains(token), "token leaked into audit log: {audit}");
    }

    /// The classifier against the exact stderr captured from the incident
    /// (gitlab.freedesktop.org/appstream/appstream, sealed anonymous env,
    /// zero credentials — reproduced 2026-09) plus the other auth-rejection
    /// phrasings git and the forges produce.
    #[test]
    fn clone_failure_tail_is_classified_as_auth_rejection() {
        // verbatim incident capture: the GitLab page body relayed as
        // `remote:` lines, then git's own fatal line
        let incident = "Cloning into '/tmp/astream-clone'...\n\
            remote: HTTP Basic: Access denied. If a password was provided for \
            Git authentication, the password was incorrect or you're required \
            to use a token instead of a password. If a token was provided, it \
            was either incorrect, expired, or improperly scoped.\n\
            fatal: Authentication failed for \
            'https://gitlab.freedesktop.org/appstream/appstream.git/'";
        assert!(clone_tail_is_auth_rejection(incident));

        // the other real phrasings
        assert!(clone_tail_is_auth_rejection(
            "fatal: could not read Username for 'https://x': No such device or address"
        ));
        assert!(clone_tail_is_auth_rejection(
            "fatal: could not read Username for 'https://x': terminal prompts disabled"
        ));
        assert!(clone_tail_is_auth_rejection(
            "remote: HTTP Basic: Access denied\nfatal: Authentication failed for 'https://x'"
        ));

        // non-auth failures must NOT be classified as credential problems
        assert!(!clone_tail_is_auth_rejection(
            "fatal: unable to access 'https://x/': gnutls_handshake() failed"
        ));
        assert!(!clone_tail_is_auth_rejection(
            "error: RPC failed; curl 56 Recv failure: Connection was reset"
        ));
        assert!(!clone_tail_is_auth_rejection(
            "fatal: destination path '/x' already exists and is not an empty directory."
        ));
        assert!(!clone_tail_is_auth_rejection(""));
    }

    /// End-to-end through the REAL git binary: a server that answers
    /// anonymous git-HTTP with GitLab's auth-gate (401 + Basic challenge +
    /// the "HTTP Basic: Access denied" body) must produce the dedicated
    /// CloneAuth error for an anonymous generic remote — affirmatively
    /// stating that NO credentials were sent, with the two remedies —
    /// instead of the raw remote message that reads like a credential
    /// leak. Localhost only; hard deadline; never prompts (prompting is
    /// disabled in the sealed env, and GIT_ASKPASS answers empty).
    #[test]
    fn auth_rejected_anonymous_clone_explains_that_no_credentials_were_sent() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let dir = temp_case("deny");
        let git_home = dir.join("githome");
        fs::create_dir_all(&git_home).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = match conn {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match conn.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = "HTTP Basic: Access denied. If a password was \
                            provided for Git authentication, the password was \
                            incorrect or you're required to use a token instead \
                            of a password.";
                let resp = format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic \
                     realm=\"GitLab\"\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = conn.write_all(resp.as_bytes());
                let _ = conn.flush();
            }
        });

        let ctx = ExecCtx {
            audit_log: Some(dir.join("audit.log")),
            redactions: Vec::new(),
            resolve_path: "/usr/bin:/bin".to_string(),
            interactive_override: Some(false),
            ..ExecCtx::default()
        };
        let url = format!("http://127.0.0.1:{port}/appstream/appstream.git");
        let remote = authed_url(&url, None);

        let ctx2 = ctx.clone();
        let remote2 = remote.clone();
        let dest = dir.join("repo");
        let git_home2 = git_home.clone();
        let err = with_deadline(90, move || {
            git_clone(
                &ctx2,
                &remote2,
                &dest,
                None,
                &CloneSection::default(),
                "/usr/bin:/bin",
                &git_home2,
                None,
            )
            .unwrap_err()
        });
        match &err {
            GitfullError::CloneAuth {
                url: u,
                anonymous,
                tail,
            } => {
                assert_eq!(u, &url, "the error reports the credential-free URL");
                assert!(*anonymous, "this clone attached no token");
                assert!(
                    tail.to_ascii_lowercase().contains("authentication failed")
                        || tail.to_ascii_lowercase().contains("denied"),
                    "git's own tail kept for diagnosis: {tail}"
                );
            }
            other => panic!("expected CloneAuth, got {other:?}"),
        }
        let msg = format!("{err}");
        assert!(msg.contains("NO credentials"), "{msg}");
        assert!(msg.contains("[forge."), "{msg}");
        assert!(msg.contains("[dep."), "{msg}");
        // and it must NOT look like the raw leak-style message anymore
        assert!(!msg.starts_with("remote: HTTP Basic"), "{msg}");
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    #[test]
    fn parses_single_block() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nLink: <https://x?page=2>; rel=\"last\"\r\n\r\n{\"a\":1}";
        let r = parse_http_include(raw);
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert!(r.header("link").unwrap().contains("page=2"));
        assert_eq!(r.body, "{\"a\":1}");
        assert!(r.ok());
    }

    #[test]
    fn redirects_are_returned_not_followed() {
        // http_get never passes --location: a 302 surfaces as status 302
        // and everything after the first header block stays body verbatim
        let raw = "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\n\r\nHTTP/2 200\r\nX-Thing: y\r\n\r\nbody-text";
        let r = parse_http_include(raw);
        assert_eq!(r.status, 302);
        assert_eq!(r.header("location"), Some("/elsewhere"));
        assert!(r.body.starts_with("HTTP/2 200"));
        assert!(r.body.ends_with("body-text"));
        assert!(!r.ok());
    }

    #[test]
    fn handles_missing_body_and_lf_only() {
        let r = parse_http_include("HTTP/1.1 404 Not Found\nServer: x\n\n");
        assert_eq!(r.status, 404);
        assert!(!r.ok());
        assert_eq!(r.body, "");
    }

    #[test]
    fn body_with_header_like_content_is_not_split() {
        let raw = "HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 not really\r\n\r\ninner";
        let r = parse_http_include(raw);
        // only the FIRST block is headers; the rest stays body verbatim
        assert_eq!(r.status, 200);
        assert!(r.body.starts_with("HTTP/1.1 not really"));
    }
}
