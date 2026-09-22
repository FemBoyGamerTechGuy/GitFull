//! Central error type for gitfull.
//!
//! Every fallible operation funnels into [`GitfullError`]; policy violations
//! (attempting to execute a forbidden host package manager, for example) are
//! deliberately loud and explicit so they are impossible to miss in logs.

use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T, E = GitfullError> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum GitfullError {
    Io(io::Error),
    Toml(String),
    Usage(String),
    Spec(String),
    Config(String),
    /// Executing a forbidden program (host package manager / privilege
    /// escalator). This is the hard enforcement point of the
    /// "never shell out to a package manager" policy.
    Policy {
        program: String,
        reason: String,
    },
    /// A state-changing operation was attempted without root while gitfull
    /// operates on system paths (`/var/lib/gitfull` and/or a system-wide
    /// bin dir). Mutating commands must run as root (sudo); read-only
    /// commands (list, info, doctor, config, audit) stay unprivileged.
    Privilege {
        cmd: String,
        root: PathBuf,
        bin_dir: PathBuf,
    },
    Toolchain {
        component: String,
        message: String,
    },
    Sandbox(String),
    /// A git clone was rejected by the remote server as an authentication
    /// failure. `anonymous` distinguishes the two very different causes:
    /// gitfull's generic-remote clone sends **no credentials at all** (no
    /// credential helper, empty gitconfig, prompts disabled — a token is
    /// attached only to hosts with a `[forge.<name>]` entry), so an
    /// anonymous rejection means the *server* gates that repository (the
    /// GitLab "HTTP Basic: Access denied" page is exactly this case — it
    /// reads like a client sent a bad password, when in fact nothing was
    /// sent). A token rejection means a configured forge's token was
    /// declined.
    CloneAuth {
        /// The credential-free URL (for display; never embeds a token).
        url: String,
        /// True when no token was attached to this clone.
        anonymous: bool,
        /// git's own stderr tail, for diagnosis.
        tail: String,
    },
    Exec {
        program: String,
        status: String,
        log: Option<PathBuf>,
        tail: String,
    },
    Unsupported(String),
    NotInstalled(String),
}

impl fmt::Display for GitfullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitfullError::Io(e) => write!(f, "I/O error: {e}"),
            GitfullError::Toml(e) => write!(f, "TOML error: {e}"),
            GitfullError::Usage(m) => write!(f, "usage error: {m}"),
            GitfullError::Spec(m) => write!(f, "invalid package spec: {m}"),
            GitfullError::Config(m) => write!(f, "configuration error: {m}"),
            GitfullError::Policy { program, reason } => write!(
                f,
                "POLICY VIOLATION: gitfull refused to execute `{program}` ({reason}). \
                 gitfull never invokes host package managers or privilege escalators; \
                 all dependency resolution is performed internally by gitfull. \
                 See docs/AUDIT.md."
            ),
            GitfullError::Privilege { cmd, root, bin_dir } => write!(
                f,
                "root required: `{cmd}` writes to {r} and installs binaries \
                 into {b}, which a normal user cannot do. Re-run it as root: \
                 `sudo gitfull {cmd} ...`. Read-only commands (list, info, \
                 doctor, config, audit) do not need root.",
                r = root.display(),
                b = bin_dir.display(),
            ),
            GitfullError::Toolchain { component, message } => {
                write!(f, "toolchain[{component}]: {message}")
            }
            GitfullError::Sandbox(m) => write!(f, "sandbox error: {m}"),
            GitfullError::CloneAuth {
                url,
                anonymous,
                tail,
            } => {
                if *anonymous {
                    write!(
                        f,
                        "clone of `{url}` was rejected by the server as an \
                         authentication failure — but gitfull sent NO credentials \
                         for this clone. Generic-remote clones are sealed \
                         anonymous (credential helper disabled, empty gitconfig, \
                         prompts off); a token is attached only to hosts with a \
                         [forge.<name>] entry, and this host has none. The server \
                         itself is refusing anonymous access to this repository \
                         (auth-gated, moved, or gone). Remedies: pin an \
                         anonymously-clonable upstream with [dep.<name>] \
                         source = \"…\" in gitfull.conf, or add a [forge.<name>] \
                         entry with this host and a token_env to clone it \
                         authenticated.\ngit said:\n{tail}"
                    )
                } else {
                    write!(
                        f,
                        "clone of `{url}` was rejected by the server: the token \
                         attached for its configured forge was declined \
                         (wrong value, expired, revoked, or missing scope for \
                         this repository). Check the forge's token_env variable.\
                         \ngit said:\n{tail}"
                    )
                }
            }
            GitfullError::Exec {
                program,
                status,
                log,
                tail,
            } => {
                let logref = log
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                write!(
                    f,
                    "`{program}` failed ({status}).\n{tail}\nfull log: {logref}"
                )
            }
            GitfullError::Unsupported(m) => write!(f, "unsupported: {m}"),
            GitfullError::NotInstalled(m) => write!(f, "not installed: {m}"),
        }
    }
}

impl std::error::Error for GitfullError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GitfullError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for GitfullError {
    fn from(e: io::Error) -> Self {
        GitfullError::Io(e)
    }
}

impl From<toml::de::Error> for GitfullError {
    fn from(e: toml::de::Error) -> Self {
        GitfullError::Toml(e.to_string())
    }
}

impl From<toml::ser::Error> for GitfullError {
    fn from(e: toml::ser::Error) -> Self {
        GitfullError::Toml(e.to_string())
    }
}
