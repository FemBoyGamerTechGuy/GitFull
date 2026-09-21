//! gitfull — a package manager for forge-hosted Git repositories.
//!
//! gitfull treats any Git-forge-hosted repository (GitHub, GitLab, Gitea,
//! Codeberg, self-hosted forges, ...) as an installable package. GitHub is
//! the default forge, but forges are data defined in `/etc/gitfull.conf`, so
//! new forges can be added without code changes.
//!
//! # Isolation model (docs/AUDIT.md is the authoritative write-up)
//!
//! * Every installed app lives in its own sandbox folder under
//!   `<root>/apps/<name>/`; sources, builds, and intermediate files never
//!   leave it.
//! * The **single host-touching path**: building the initial seed GCC
//!   toolchain, which uses the host's system compiler exactly once
//!   ([`toolchain`], `ExecClass::SeedHostCompiler`). After that, every build
//!   — dependencies and target apps alike — uses toolchain-managed
//!   compilers.
//! * The **single sandbox-escape path**: when a build produces final
//!   binaries, [`planner::install_binaries`] copies exactly those binaries
//!   out to the configured bin directory. Nothing else crosses the sandbox
//!   boundary.
//! * gitfull never invokes host package managers (`pacman`, `dnf`, `xbps`,
//!   `apt`, ...) — enforced structurally in [`gitproc`], not by convention.
//!
//! # Dependency posture
//!
//! gitfull itself depends on exactly two crates — `serde` and `toml`
//! (MIT OR Apache-2.0) — for its TOML configuration. No copyleft code, no
//! Red Hat-associated system software is linked into gitfull's own binary.
//! Licenses of *target* packages gitfull builds for users are irrelevant to
//! gitfull itself; see docs/AUDIT.md.

#![cfg_attr(not(unix), doc = "gitfull currently targets Unix/Linux.")]
#[cfg(not(unix))]
compile_error!("gitfull currently targets Linux/Unix only.");

pub mod bootstrap;
pub mod config;
pub mod depgraph;
pub mod error;
pub mod forge;
pub mod gitproc;
pub mod json;
pub mod libcache;
pub mod manifest;
pub mod planner;
pub mod privilege;
pub mod progress;
pub mod resolver;
pub mod sandbox;
pub mod search;
pub mod sha256;
pub mod spec;
pub mod toolchain;
pub mod util;

/// Crate version (used by `gitfull --version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
