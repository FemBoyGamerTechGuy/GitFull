//! Dependency resolution — performed entirely inside gitfull.
//!
//! gitfull never delegates dependency resolution to an OS package manager
//! (structurally impossible: those binaries are denied at the exec
//! chokepoint). Instead:
//!
//! * **toolchain needs** are derived from the detected build system
//!   (e.g. meson → gcc + python + meson + ninja) plus any
//!   `[repo."owner/name"] toolchains = ["gcc>=13", ...]` constraints;
//! * **package needs** are `[repo] packages = ["owner/repo", ...]` specs —
//!   other forge repositories, resolved recursively in the planner, each
//!   built inside the *requesting app's* sandbox under `deps/`.
//!
//! Version constraints: `gcc`, `gcc>=13.3.0`, `python=3.12`,
//! `ninja>1.11`, `meson<2`. Missing components are built from source by
//! gitfull itself (see [`crate::toolchain`]).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::RepoOverride;
use crate::error::{GitfullError, Result};
use crate::manifest::{load_manifest, BuildSystem};
use crate::spec::PkgSpec;
use crate::util::version_cmp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Any,
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A version constraint on a toolchain component.
#[derive(Debug, Clone)]
pub struct Constraint {
    pub op: CmpOp,
    pub version: Option<String>,
}

impl Constraint {
    pub fn parse_any() -> Constraint {
        Constraint {
            op: CmpOp::Any,
            version: None,
        }
    }

    pub fn parse_op(op: CmpOp, version: String) -> Constraint {
        Constraint {
            op,
            version: Some(version),
        }
    }

    pub fn satisfies(&self, v: &str) -> bool {
        let Some(want) = &self.version else {
            return true;
        };
        match self.op {
            CmpOp::Any => true,
            CmpOp::Eq => version_cmp(v, want) == Ordering::Equal,
            CmpOp::Gt => version_cmp(v, want) == Ordering::Greater,
            CmpOp::Gte => version_cmp(v, want) != Ordering::Less,
            CmpOp::Lt => version_cmp(v, want) == Ordering::Less,
            CmpOp::Lte => version_cmp(v, want) != Ordering::Greater,
        }
    }

    /// Merge two constraints on the same component (tighter wins; lower
    /// bounds take the max, upper bounds the min, Eq dominates).
    pub fn merge(&self, other: &Constraint) -> Constraint {
        use CmpOp::*;
        if self.op == Any {
            return other.clone();
        }
        if other.op == Any {
            return self.clone();
        }
        let (a, b) = (self, other);
        // lower bounds: keep the higher requirement
        if matches!(a.op, Gte) && matches!(b.op, Gte)
            || matches!(a.op, Gt) && matches!(b.op, Gt)
            || matches!(a.op, Gte) && matches!(b.op, Gt)
            || matches!(a.op, Gt) && matches!(b.op, Gte)
        {
            let pick = if version_cmp(
                a.version.as_deref().unwrap_or("0"),
                b.version.as_deref().unwrap_or("0"),
            ) != Ordering::Less
            {
                a
            } else {
                b
            };
            return pick.clone();
        }
        // upper bounds: keep the lower ceiling
        if matches!(a.op, Lt | Lte) && matches!(b.op, Lt | Lte) {
            let pick = if version_cmp(
                a.version.as_deref().unwrap_or("0"),
                b.version.as_deref().unwrap_or("0"),
            ) != Ordering::Greater
            {
                a
            } else {
                b
            };
            return pick.clone();
        }
        // mixed or equalities: keep the explicit pin
        if a.op == Eq {
            a.clone()
        } else {
            b.clone()
        }
    }
}

/// Parse `"gcc>=13.3.0"` → `("gcc", Constraint)`.
pub fn parse_component_constraint(s: &str) -> Result<(String, Constraint)> {
    let s = s.trim();
    for (i, c) in s.char_indices() {
        if matches!(c, '=' | '<' | '>') {
            let comp = s[..i].trim();
            let mut op_str = String::new();
            let mut rest = i;
            while rest < s.len() {
                let ch = s[rest..].chars().next().unwrap();
                if matches!(ch, '=' | '<' | '>') {
                    op_str.push(ch);
                    rest += ch.len_utf8();
                } else {
                    break;
                }
            }
            let version = s[rest..].trim().to_string();
            if comp.is_empty()
                || !comp
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(GitfullError::Config(format!(
                    "invalid toolchain constraint `{s}` (bad component name)"
                )));
            }
            if version.is_empty() {
                return Err(GitfullError::Config(format!(
                    "invalid toolchain constraint `{s}` (missing version)"
                )));
            }
            let op = match op_str.as_str() {
                "=" => CmpOp::Eq,
                ">" => CmpOp::Gt,
                ">=" => CmpOp::Gte,
                "<" => CmpOp::Lt,
                "<=" => CmpOp::Lte,
                _ => {
                    return Err(GitfullError::Config(format!(
                        "invalid operator `{op_str}` in `{s}` (use = > >= < <=)"
                    )))
                }
            };
            return Ok((
                comp.to_string(),
                Constraint {
                    op,
                    version: Some(version),
                },
            ));
        }
    }
    if s.is_empty() {
        return Err(GitfullError::Config("empty toolchain constraint".into()));
    }
    Ok((s.to_string(), Constraint::parse_any()))
}

/// Implicit toolchain needs per build system (what gitfull must have
/// available to build a repo of that kind).
pub fn implicit_toolchain_needs(bs: BuildSystem) -> Vec<(String, Constraint)> {
    match bs {
        BuildSystem::Meson => vec![
            ("gcc".into(), Constraint::parse_any()),
            (
                "python".into(),
                Constraint::parse_op(CmpOp::Gte, "3.8".into()),
            ),
            ("meson".into(), Constraint::parse_any()),
            ("ninja".into(), Constraint::parse_any()),
        ],
        BuildSystem::Cmake => vec![
            ("gcc".into(), Constraint::parse_any()),
            ("cmake".into(), Constraint::parse_any()),
            ("ninja".into(), Constraint::parse_any()),
        ],
        BuildSystem::Autotools => vec![("gcc".into(), Constraint::parse_any())],
        BuildSystem::Make => vec![("gcc".into(), Constraint::parse_any())],
        BuildSystem::Cargo => vec![("rust".into(), Constraint::parse_any())],
    }
}

/// Everything gitfull knows about one resolved repository.
#[derive(Debug, Clone)]
pub struct ResolvedRepo {
    /// Lookup key (`owner/repo`, URL, or local path).
    pub key: String,
    pub build: BuildSystem,
    /// Merged toolchain requirements (implicit + config-declared).
    pub toolchains: BTreeMap<String, Constraint>,
    /// Package dependencies (other forge repos), not yet resolved.
    pub packages: Vec<PkgSpec>,
    /// Explicit final binaries (config `[repo] bins` or repo gitfull.toml).
    pub bins: Vec<String>,
    /// Per-repo build parallelism override.
    pub jobs: Option<usize>,
}

/// Resolve one checkout: auto-detect its build system, merge toolchain
/// constraints and package deps from the `[repo]` config override.
pub fn resolve_repo(src: &Path, key: &str, ov: Option<&RepoOverride>) -> Result<ResolvedRepo> {
    let manifest = load_manifest(src)?;
    let build = match ov.and_then(|o| o.build_system.as_deref()) {
        Some(s) => BuildSystem::parse(s)?,
        None => manifest.build,
    };

    let mut toolchains: BTreeMap<String, Constraint> = BTreeMap::new();
    for (comp, c) in implicit_toolchain_needs(build) {
        toolchains.insert(comp, c);
    }
    if let Some(o) = ov {
        for spec in &o.toolchains {
            let (comp, c) = parse_component_constraint(spec)?;
            let merged = toolchains.get(&comp).map(|e| e.merge(&c)).unwrap_or(c);
            toolchains.insert(comp, merged);
        }
    }

    let mut packages = Vec::new();
    if let Some(o) = ov {
        for p in &o.packages {
            packages.push(PkgSpec::parse(p)?);
        }
    }

    let bins = ov
        .filter(|o| !o.bins.is_empty())
        .map(|o| o.bins.clone())
        .unwrap_or(mmanifest_bins(manifest.bins));

    Ok(ResolvedRepo {
        key: key.to_string(),
        build,
        toolchains,
        packages,
        bins,
        jobs: ov.and_then(|o| o.jobs),
    })
}

fn mmanifest_bins(bins: Vec<String>) -> Vec<String> {
    bins
}

/// Closure detection helper used by the planner's dependency walk.
pub fn detect_cycle(chain: &[String], next: &str) -> Option<String> {
    if chain.iter().any(|c| c == next) {
        let mut path = chain.to_vec();
        path.push(next.to_string());
        return Some(path.join(" -> "));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraint_parsing() {
        let (c, k) = parse_component_constraint("gcc>=13.3.0").unwrap();
        assert_eq!(c, "gcc");
        assert_eq!(k.op, CmpOp::Gte);
        assert_eq!(k.version.as_deref(), Some("13.3.0"));

        let (c, k) = parse_component_constraint("python=3.12").unwrap();
        assert_eq!(c, "python");
        assert_eq!(k.op, CmpOp::Eq);

        let (c, k) = parse_component_constraint("meson").unwrap();
        assert_eq!(c, "meson");
        assert_eq!(k.op, CmpOp::Any);

        for bad in ["", ">=1", "gcc>", "gcc>=", "gc c>=1"] {
            assert!(
                parse_component_constraint(bad).is_err(),
                "expected err for `{bad}`"
            );
        }
    }

    #[test]
    fn constraint_satisfaction() {
        let gte = |v: &str| Constraint::parse_op(CmpOp::Gte, v.into());
        assert!(gte("13").satisfies("13.3.0"));
        assert!(gte("13").satisfies("14.2.0"));
        assert!(!gte("14").satisfies("13.9.9"));
        let eq = Constraint::parse_op(CmpOp::Eq, "3.12".into());
        assert!(eq.satisfies("3.12"));
        assert!(!eq.satisfies("3.12.1"));
        let any = Constraint::parse_any();
        assert!(any.satisfies("anything"));
        let lt = Constraint::parse_op(CmpOp::Lt, "14".into());
        assert!(lt.satisfies("13.3.0"));
        assert!(!lt.satisfies("14.0.0"));
    }

    #[test]
    fn constraint_merging() {
        let a = Constraint::parse_op(CmpOp::Gte, "13".into());
        let b = Constraint::parse_op(CmpOp::Gte, "14".into());
        assert_eq!(a.merge(&b).version.as_deref(), Some("14"));
        let any = Constraint::parse_any();
        assert_eq!(any.merge(&b).version.as_deref(), Some("14"));
        let eq = Constraint::parse_op(CmpOp::Eq, "13.3.0".into());
        assert_eq!(eq.merge(&a).op, CmpOp::Eq);
    }

    #[test]
    fn implicit_needs() {
        let meson = implicit_toolchain_needs(BuildSystem::Meson);
        assert!(meson.iter().any(|(c, _)| c == "python"));
        let cargo = implicit_toolchain_needs(BuildSystem::Cargo);
        assert_eq!(cargo.len(), 1);
        assert_eq!(cargo[0].0, "rust");
    }

    #[test]
    fn cycles_detected() {
        let chain = vec!["a".to_string(), "b".to_string()];
        assert_eq!(detect_cycle(&chain, "a"), Some("a -> b -> a".to_string()));
        assert!(detect_cycle(&chain, "c").is_none());
    }

    #[test]
    fn resolve_local_repo() {
        let dir = std::env::temp_dir().join(format!("gitfull-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
        let r = resolve_repo(&dir, "test/repo", None).unwrap();
        assert_eq!(r.build, BuildSystem::Make);
        assert!(r.toolchains.contains_key("gcc"));
        assert!(r.packages.is_empty());

        let ov = RepoOverride {
            toolchains: vec!["gcc>=13".into(), "ninja".into()],
            packages: vec!["some/dep".into()],
            bins: vec!["custom-bin".into()],
            ..RepoOverride::default()
        };
        let r2 = resolve_repo(&dir, "test/repo", Some(&ov)).unwrap();
        assert_eq!(
            r2.toolchains.get("gcc").unwrap().version.as_deref(),
            Some("13")
        );
        assert!(r2.toolchains.contains_key("ninja"));
        assert_eq!(r2.packages.len(), 1);
        assert_eq!(r2.bins, vec!["custom-bin".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
