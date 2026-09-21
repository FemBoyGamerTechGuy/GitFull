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
    Toolchain {
        component: String,
        message: String,
    },
    Sandbox(String),
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
            GitfullError::Toolchain { component, message } => {
                write!(f, "toolchain[{component}]: {message}")
            }
            GitfullError::Sandbox(m) => write!(f, "sandbox error: {m}"),
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
