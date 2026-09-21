//! Per-app sandbox environments.
//!
//! Every installed app gets its own folder under `<root>/apps/<name>/`:
//!
//! ```text
//! <root>/apps/<forge>-<owner>-<repo>/
//! ├── src/      git checkout (clone target)
//! ├── build/    out-of-tree build directory
//! ├── stage/    DESTDIR staging area for `make install`-style steps
//! ├── prefix/   the app's in-sandbox install prefix (dep of later steps)
//! ├── deps/     dependency packages cloned + built inside THIS sandbox
//! ├── env/      HOME for build processes (no host HOME ever leaks in)
//! ├── tmp/      TMPDIR for build processes
//! ├── logs/     per-step build logs
//! └── meta.toml install record (see planner::InstallRecord)
//! ```
//!
//! Nothing inside the sandbox writes outside it; the ONLY sanctioned
//! crossing is [`crate::planner::install_binaries`] copying final binaries
//! to the bin dir. Shared toolchains live in `<root>/toolchains/` so apps
//! reference common versions instead of duplicating them.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::util::{dedup_path, sanitize_name};

pub const SANDBOX_SUBDIRS: &[&str] = &[
    "src", "build", "stage", "prefix", "deps", "env", "tmp", "logs",
];

#[derive(Debug, Clone)]
pub struct Sandbox {
    /// The sandbox root directory (`<root>/apps/<name>` or a dep dir).
    pub dir: PathBuf,
}

impl Sandbox {
    pub fn for_app(apps_dir: &Path, name: &str) -> Sandbox {
        Sandbox {
            dir: apps_dir.join(sanitize_component_path(name)),
        }
    }

    pub fn name_for(forge: &str, owner: &str, repo: &str) -> String {
        sanitize_name(forge, owner, repo)
    }

    /// Create the sandbox layout (idempotent).
    pub fn create(&self) -> Result<()> {
        for d in SANDBOX_SUBDIRS {
            fs::create_dir_all(self.dir.join(d))?;
        }
        Ok(())
    }

    pub fn sub(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
    pub fn src(&self) -> PathBuf {
        self.sub("src")
    }
    pub fn build(&self) -> PathBuf {
        self.sub("build")
    }
    pub fn stage(&self) -> PathBuf {
        self.sub("stage")
    }
    pub fn prefix(&self) -> PathBuf {
        self.sub("prefix")
    }
    pub fn deps(&self) -> PathBuf {
        self.sub("deps")
    }
    pub fn env(&self) -> PathBuf {
        self.sub("env")
    }
    pub fn tmp(&self) -> PathBuf {
        self.sub("tmp")
    }
    pub fn logs(&self) -> PathBuf {
        self.sub("logs")
    }
    pub fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.toml")
    }

    /// The environment every in-sandbox build runs under.
    ///
    /// * `PATH` starts with toolchain bins, then dependency prefixes, then
    ///   this sandbox's prefix, and ends with the configured POSIX-utility
    ///   path — the host compiler can never be picked up implicitly.
    /// * `HOME`/`TMPDIR` point inside the sandbox.
    /// * Dependency prefixes are exported via `PKG_CONFIG_PATH`,
    ///   `LD_LIBRARY_PATH`, and `-I`/`-L` flags.
    pub fn build_env(
        &self,
        toolchain_bins: &[PathBuf],
        toolchain_libs: &[PathBuf],
        dep_prefixes: &[PathBuf],
        host_tool_path: &str,
        extra: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut path_parts: Vec<String> = Vec::new();
        for b in toolchain_bins {
            path_parts.push(b.display().to_string());
        }
        for p in dep_prefixes {
            path_parts.push(p.join("bin").display().to_string());
        }
        path_parts.push(self.prefix().join("bin").display().to_string());
        for p in host_tool_path.split(':') {
            path_parts.push(p.to_string());
        }

        let mut pc_paths: Vec<String> = Vec::new();
        let mut ld_paths: Vec<String> = Vec::new();
        let mut cflags: Vec<String> = Vec::new();
        let mut ldflags: Vec<String> = Vec::new();
        for p in dep_prefixes {
            pc_paths.push(p.join("lib/pkgconfig").display().to_string());
            pc_paths.push(p.join("share/pkgconfig").display().to_string());
            ld_paths.push(p.join("lib").display().to_string());
            cflags.push(format!("-I{}", p.join("include").display()));
            ldflags.push(format!("-L{}", p.join("lib").display()));
        }
        pc_paths.push(self.prefix().join("lib/pkgconfig").display().to_string());
        ld_paths.push(self.prefix().join("lib").display().to_string());
        for l in toolchain_libs {
            ld_paths.push(l.display().to_string());
        }

        let mut env: Vec<(String, String)> = vec![
            ("PATH".into(), dedup_path(&path_parts)),
            ("HOME".into(), self.env().display().to_string()),
            ("TMPDIR".into(), self.tmp().display().to_string()),
            ("LC_ALL".into(), "C".into()),
            ("LANG".into(), "C".into()),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "PKG_CONFIG_PATH".into(),
                pc_paths
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(":"),
            ),
            (
                "LD_LIBRARY_PATH".into(),
                ld_paths
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(":"),
            ),
            ("CPPFLAGS".into(), cflags.join(" ")),
            ("LDFLAGS".into(), ldflags.join(" ")),
            ("DESTDIR".into(), self.stage().display().to_string()),
        ];
        for (k, v) in extra {
            if let Some(slot) = env.iter_mut().find(|(ek, _)| ek == k) {
                slot.1 = v.clone();
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
        env
    }
}

fn sanitize_component_path(name: &str) -> String {
    // names are pre-sanitized via sanitize_name; just guard against '/'
    name.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_env() {
        let sb = Sandbox::for_app(Path::new("/tmp/apps"), "github-acme-widgets");
        sb.create().unwrap();
        assert!(sb.src().is_dir());
        assert!(sb.meta_path().ends_with("meta.toml"));

        let env = sb.build_env(
            &[PathBuf::from("/t/gcc-13/bin")],
            &[PathBuf::from("/t/gcc-13/lib")],
            &[PathBuf::from("/t/apps/dep/prefix")],
            "/usr/bin:/bin",
            &[("CC".to_string(), "/t/gcc-13/bin/gcc".to_string())],
        );
        let get = |k: &str| {
            env.iter()
                .find(|(ek, _)| ek == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert!(get("PATH").starts_with("/t/gcc-13/bin"));
        assert!(get("PATH").contains("/t/apps/dep/prefix/bin"));
        assert!(get("PATH").ends_with("/usr/bin:/bin"));
        assert_eq!(get("CC"), "/t/gcc-13/bin/gcc");
        assert_eq!(get("HOME"), sb.env().display().to_string());
        assert!(get("DESTDIR").starts_with(&sb.dir.display().to_string()));
        let _ = fs::remove_dir_all("/tmp/apps/github-acme-widgets");
    }

    #[test]
    fn names() {
        assert_eq!(
            Sandbox::name_for("github", "FemBoyGamerTechGuy", "GitFull"),
            "github-femboygamertechguy-gitfull"
        );
        assert_eq!(
            Sandbox::name_for("gitlab", "group/sub", "project"),
            "gitlab-group-sub-project"
        );
    }
}
