//! Package specs — how users name packages on the command line.
//!
//! Accepted forms:
//!
//! * `owner/repo` — default forge
//! * `forge:owner/repo` — explicit forge by config name
//! * `owner/repo@ref` — pinned branch/tag/commit (combine: `forge:o/r@v1`)
//! * `https://forge.example/owner/repo.git` — full clone URL (matched
//!   against configured forge hosts)
//! * `/abs/path` or `./rel/path` — local source tree (no forge, no clone)
//!
//! Owners may contain `/` (GitLab-style subgroups). The final path segment
//! is always the repo name.

use std::path::PathBuf;

use crate::error::{GitfullError, Result};
use crate::util::valid_slug;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Forge {
        forge: Option<String>,
        owner: String,
        repo: String,
    },
    Url(String),
    Local(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PkgSpec {
    pub source: Source,
    pub git_ref: Option<String>,
}

impl PkgSpec {
    pub fn parse(input: &str) -> Result<PkgSpec> {
        let s = input.trim();
        if s.is_empty() {
            return Err(GitfullError::Spec("empty spec".into()));
        }

        // Local path?
        if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") {
            return Ok(PkgSpec {
                source: Source::Local(PathBuf::from(s)),
                git_ref: None,
            });
        }

        // Full URL?
        if s.contains("://") || s.starts_with("git@") {
            return Ok(PkgSpec {
                source: Source::Url(s.to_string()),
                git_ref: None,
            });
        }

        // [forge:]owner/repo[@ref]
        let (body, git_ref) = match s.split_once('@') {
            Some((b, r)) => {
                if r.is_empty() || r.contains('/') || r.chars().any(|c| c.is_whitespace()) {
                    return Err(GitfullError::Spec(format!(
                        "`{input}`: invalid ref `{r}` (after '@'; must be a \
                         branch/tag/commit name without '/' or spaces)"
                    )));
                }
                (b, Some(r.to_string()))
            }
            None => (s, None),
        };

        let (forge, rest) = match body.split_once(':') {
            Some((f, r)) if !f.is_empty() && !f.contains('/') => {
                if !valid_slug(f) {
                    return Err(GitfullError::Spec(format!(
                        "`{input}`: invalid forge name `{f}`"
                    )));
                }
                (Some(f.to_string()), r)
            }
            _ => (None, body),
        };

        let (owner, repo) = rest.rsplit_once('/').ok_or_else(|| {
            GitfullError::Spec(format!(
                "`{input}`: expected owner/repo (got `{rest}`); forms: \
                 owner/repo, forge:owner/repo, owner/repo@ref, URL, or local path"
            ))
        })?;

        if !valid_slug(repo) {
            return Err(GitfullError::Spec(format!(
                "`{input}`: invalid repo name `{repo}`"
            )));
        }
        for seg in owner.split('/') {
            if !valid_slug(seg) {
                return Err(GitfullError::Spec(format!(
                    "`{input}`: invalid owner segment `{seg}`"
                )));
            }
        }
        if owner.is_empty() {
            return Err(GitfullError::Spec(format!("`{input}`: missing owner")));
        }

        Ok(PkgSpec {
            source: Source::Forge {
                forge,
                owner: owner.to_string(),
                repo: repo.to_string(),
            },
            git_ref,
        })
    }

    /// Lookup key for `[repo."..."]` overrides: `owner/repo`, the URL, or
    /// the local path.
    pub fn key(&self) -> String {
        match &self.source {
            Source::Forge { owner, repo, .. } => format!("{owner}/{repo}"),
            Source::Url(u) => u.clone(),
            Source::Local(p) => p.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_specs() {
        let p = PkgSpec::parse("femboygamertechguy/GitFull").unwrap();
        assert_eq!(
            p.source,
            Source::Forge {
                forge: None,
                owner: "femboygamertechguy".into(),
                repo: "GitFull".into()
            }
        );
        assert_eq!(p.key(), "femboygamertechguy/GitFull");

        let p = PkgSpec::parse("codeberg:org/project@v1.2").unwrap();
        assert_eq!(
            p.source,
            Source::Forge {
                forge: Some("codeberg".into()),
                owner: "org".into(),
                repo: "project".into()
            }
        );
        assert_eq!(p.git_ref.as_deref(), Some("v1.2"));

        // GitLab-style subgroups
        let p = PkgSpec::parse("gitlab:group/sub/team/project").unwrap();
        match p.source {
            Source::Forge { owner, repo, .. } => {
                assert_eq!(owner, "group/sub/team");
                assert_eq!(repo, "project");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn url_and_local() {
        let p = PkgSpec::parse("https://gitlab.com/g/s/p.git").unwrap();
        assert_eq!(p.source, Source::Url("https://gitlab.com/g/s/p.git".into()));
        let p = PkgSpec::parse("/home/x/src/app").unwrap();
        assert!(matches!(p.source, Source::Local(_)));
    }

    #[test]
    fn invalid_specs() {
        // note: "/repo" is NOT invalid — leading '/' means a local path
        for bad in [
            "",
            "repo",
            "owner/",
            "o/r@bad/ref",
            "o/r@",
            "forge!:o/r",
            "own er/repo",
            "a//b",
        ] {
            assert!(PkgSpec::parse(bad).is_err(), "expected error for `{bad}`");
        }
        assert!(PkgSpec::parse("/repo").is_ok());
        assert!(matches!(
            PkgSpec::parse("/repo").unwrap().source,
            Source::Local(_)
        ));
    }
}
