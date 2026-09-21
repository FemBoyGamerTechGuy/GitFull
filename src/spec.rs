//! Package specs — how users name packages on the command line.
//!
//! Accepted forms:
//!
//! * `owner/repo` — default forge
//! * `forge:owner/repo` — explicit forge by config name
//! * `name` — **search form**: rank matching repos across all configured
//!   forges and auto-select the top result (see [`crate::search`]); the
//!   resolution is always printed before anything is built
//! * `forge:name` — search form scoped to one forge
//! * `owner/repo@ref` — pinned branch/tag/commit (combine: `forge:o/r@v1`)
//! * `https://forge.example/owner/repo.git` — full clone URL (matched
//!   against configured forge hosts)
//! * `/abs/path` or `./rel/path` — local source tree (no forge, no clone)
//!
//! A single-token name without `/` is a search term, not an error: an
//! explicit `forge:owner/repo` reference bypasses ranking entirely, while
//! a bare name goes through forge search + ranking first. Owners may
//! contain `/` (GitLab-style subgroups); the final path segment is always
//! the repo name.

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
    /// A search term: bare `name` (search all configured forges) or
    /// `forge:name` (search one forge). Resolved by [`crate::search`] into
    /// a concrete `Forge` spec before install; never reaches the planner.
    Search {
        forge: Option<String>,
        term: String,
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

        // Search form: a single slug with no '/' — `name` or `forge:name`.
        // (An explicit `forge:owner/repo` still bypasses ranking.)
        if !rest.contains('/') {
            if rest.is_empty() {
                return Err(GitfullError::Spec(format!(
                    "`{input}`: empty search term"
                )));
            }
            if !valid_slug(rest) {
                return Err(GitfullError::Spec(format!(
                    "`{input}`: invalid search term `{rest}`"
                )));
            }
            return Ok(PkgSpec {
                source: Source::Search {
                    forge,
                    term: rest.to_string(),
                },
                git_ref,
            });
        }

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
    /// the local path. Search specs have no key until resolved.
    pub fn key(&self) -> String {
        match &self.source {
            Source::Forge { owner, repo, .. } => format!("{owner}/{repo}"),
            Source::Search { forge, term } => match forge {
                Some(f) => format!("{f}:{term}"),
                None => term.clone(),
            },
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
    fn search_forms() {
        // bare name → search across all configured forges
        let p = PkgSpec::parse("hello").unwrap();
        assert_eq!(
            p.source,
            Source::Search {
                forge: None,
                term: "hello".into()
            }
        );
        // forge-scoped search
        let p = PkgSpec::parse("codeberg:hello").unwrap();
        assert_eq!(
            p.source,
            Source::Search {
                forge: Some("codeberg".into()),
                term: "hello".into()
            }
        );
        // search + pinned ref carries the ref through resolution
        let p = PkgSpec::parse("hello@v1.2").unwrap();
        assert_eq!(p.git_ref.as_deref(), Some("v1.2"));
        assert!(matches!(p.source, Source::Search { .. }));
        // case sensitivity: forge names lowercased by callers, but parsing
        // keeps them verbatim (registry lookup handles it)
        assert!(PkgSpec::parse("no-such").is_ok());
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
        // note: "/repo" is NOT invalid — leading '/' means a local path;
        // "repo" (no slash) is likewise valid now — it is a SEARCH term
        for bad in [
            "",
            "owner/",
            "o/r@bad/ref",
            "o/r@",
            "forge!:o/r",
            "own er/repo",
            "a//b",
            "forge:",
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
