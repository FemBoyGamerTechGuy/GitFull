//! Forge abstraction — forges are DATA, not code.
//!
//! gitfull ships built-in definitions for GitHub (the default), GitLab, and
//! Codeberg. Any other forge — Gitea, Forgejo, cgit, a private mirror, a
//! future forge nobody has written code for yet — is a `[forge.<name>]`
//! entry in `/etc/gitfull.conf`:
//!
//! ```toml
//! [forge.mirror]
//! kind = "generic"                      # or github/gitlab/gitea/forgejo/cgit
//! host = "git.example.com"
//! clone_template = "https://{host}/{owner}/{repo}.git"   # {scheme} {port} ...
//! ```
//!
//! No code changes required; unknown keys inside an entry are accepted for
//! forward compatibility.

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::config::ForgeSection;
use crate::error::{GitfullError, Result};

/// Variables allowed in `clone_template`.
pub const TEMPLATE_VARS: &[&str] = &["name", "kind", "scheme", "host", "port", "owner", "repo"];

/// One forge entry. Every field is optional; resolution applies sensible
/// defaults (`kind = "generic"`, `scheme = "https"`).
///
/// Unknown keys inside a forge entry are accepted and ignored — forge
/// definitions must stay extensible without code changes.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ForgeDef {
    /// `github` | `gitlab` | `gitea` | `forgejo` | `cgit` | `generic`.
    /// `generic` + `clone_template` covers any forge with predictable URLs.
    #[serde(default)]
    pub kind: Option<String>,
    /// Hostname, e.g. `github.com`.
    #[serde(default)]
    pub host: Option<String>,
    /// URL scheme. Default `https`.
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Clone URL template, e.g. `https://{host}/{owner}/{repo}.git`.
    /// Required for kinds gitfull has no built-in URL rule for.
    #[serde(default)]
    pub clone_template: Option<String>,
    /// REST API base override for search/ranking queries (see
    /// [`Forge::api_base`]). Optional — derived from the kind when absent.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Name of the environment variable holding an optional read token for
    /// cloning private repos (gitfull never stores token values in config).
    #[serde(default)]
    pub token_env: Option<String>,
}

/// A resolved forge (name + definition + whether it is built-in).
#[derive(Debug, Clone)]
pub struct Forge {
    pub name: String,
    pub def: ForgeDef,
    pub builtin: bool,
}

impl Forge {
    pub fn kind(&self) -> &str {
        self.def.kind.as_deref().unwrap_or("generic")
    }
    pub fn host(&self) -> &str {
        self.def.host.as_deref().unwrap_or("")
    }
    pub fn scheme(&self) -> &str {
        self.def.scheme.as_deref().unwrap_or("https")
    }
    pub fn port(&self) -> Option<u16> {
        self.def.port
    }
    /// `host` or `host:port` — the URL authority.
    pub fn authority(&self) -> String {
        match self.port() {
            Some(p) if p != 443 => format!("{}:{p}", self.host()),
            _ => self.host().to_string(),
        }
    }
    pub fn token_env(&self) -> Option<&str> {
        self.def.token_env.as_deref()
    }

    /// Base URL of the forge's REST API, used ONLY for the read-only
    /// search/ranking queries in [`crate::search`].
    ///
    /// * explicit `api_base` in the forge entry wins (also lets tests
    ///   point a forge at a local fixture server);
    /// * else derived from the kind: GitHub → `https://api.github.com`,
    ///   GitLab → `https://<host>/api/v4`, Gitea/Forgejo →
    ///   `https://<host>/api/v1`;
    /// * `cgit`/unknown kinds without an `api_base` have no search API →
    ///   `None` (that forge is skipped during ranked search).
    pub fn api_base(&self) -> Option<String> {
        if let Some(base) = &self.def.api_base {
            let base = base.trim_end_matches('/');
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
        match self.kind() {
            "github" => Some("https://api.github.com".to_string()),
            "gitlab" => Some(format!("https://{}/api/v4", self.authority())),
            "gitea" | "forgejo" => Some(format!("https://{}/api/v1", self.authority())),
            _ => None,
        }
    }

    /// Does this forge support ranked search? (api_base derivable)
    pub fn searchable(&self) -> bool {
        self.api_base().is_some()
    }

    /// Clone URL for `owner/repo` on this forge.
    pub fn clone_url(&self, owner: &str, repo: &str) -> Result<String> {
        if let Some(t) = &self.def.clone_template {
            return render_template(
                t,
                &self.name,
                self.kind(),
                self.scheme(),
                self.host(),
                self.port(),
                owner,
                repo,
            );
        }
        let path = match self.kind() {
            "github" | "gitlab" | "gitea" | "forgejo" | "generic" => {
                format!("{owner}/{repo}.git")
            }
            "cgit" => format!("git/{owner}/{repo}.git"),
            other => {
                return Err(GitfullError::Config(format!(
                    "forge `{}` has unknown kind `{other}` and no clone_template. \
                     Set clone_template (valid vars: {TEMPLATE_VARS:?}) to teach \
                     gitfull this forge — no code changes needed",
                    self.name
                )))
            }
        };
        Ok(format!("{}://{}/{}", self.scheme(), self.authority(), path))
    }
}

/// Render a clone URL template. `{var}` substitution; unknown vars are
/// configuration errors (fail loudly, not silently).
pub fn render_template(
    t: &str,
    name: &str,
    kind: &str,
    scheme: &str,
    host: &str,
    port: Option<u16>,
    owner: &str,
    repo: &str,
) -> Result<String> {
    let port_s = port.map(|p| p.to_string()).unwrap_or_default();
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('}').ok_or_else(|| {
            GitfullError::Config(format!("unclosed '{{' in clone_template `{t}`"))
        })?;
        let var = &after[..end];
        let val = match var {
            "name" => name,
            "kind" => kind,
            "scheme" => scheme,
            "host" => host,
            "port" => port_s.as_str(),
            "owner" => owner,
            "repo" => repo,
            other => {
                return Err(GitfullError::Config(format!(
                    "clone_template uses unknown variable `{{{other}}}` \
                     (valid: {TEMPLATE_VARS:?})"
                )))
            }
        };
        out.push_str(val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A URL matched against a configured forge.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedUrl {
    pub forge: String,
    pub owner: String,
    pub repo: String,
}

/// The forge registry: built-ins + config entries (config wins on name
/// collisions, so built-ins can be customized).
#[derive(Debug, Clone)]
pub struct Registry {
    forges: BTreeMap<String, Forge>,
    default_name: String,
}

impl Registry {
    pub fn new(section: &ForgeSection) -> Result<Registry> {
        let mut forges: BTreeMap<String, Forge> = BTreeMap::new();
        for (name, kind, host) in [
            ("github", "github", "github.com"),
            ("gitlab", "gitlab", "gitlab.com"),
            ("codeberg", "forgejo", "codeberg.org"),
        ] {
            forges.insert(
                name.to_string(),
                Forge {
                    name: name.to_string(),
                    def: ForgeDef {
                        kind: Some(kind.to_string()),
                        host: Some(host.to_string()),
                        ..ForgeDef::default()
                    },
                    builtin: true,
                },
            );
        }
        for (name, def) in &section.entries {
            if name == "default" {
                // [forge.default] would collide with the scalar `default`
                // key; catch it with a clear message.
                return Err(GitfullError::Config(
                    "forge entry name `default` is reserved \
                     (it collides with `forge.default = \"<name>\"`)"
                        .to_string(),
                ));
            }
            let needs_host = def.host.is_none() && def.clone_template.is_none();
            if needs_host {
                return Err(GitfullError::Config(format!(
                    "forge `{name}` needs at least a `host` (or a \
                     `clone_template` that does not use `{{host}}`)"
                )));
            }
            forges.insert(
                name.clone(),
                Forge {
                    name: name.clone(),
                    def: def.clone(),
                    builtin: false,
                },
            );
        }
        let default_name = section
            .default
            .clone()
            .unwrap_or_else(|| "github".to_string());
        if !forges.contains_key(&default_name) {
            return Err(GitfullError::Config(format!(
                "forge.default = `{default_name}` matches no configured forge \
                 (available: {})",
                forges.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        Ok(Registry {
            forges,
            default_name,
        })
    }

    pub fn get(&self, name: &str) -> Result<&Forge> {
        self.forges.get(name).ok_or_else(|| {
            GitfullError::Config(format!(
                "unknown forge `{name}` (configured: {})",
                self.names().join(", ")
            ))
        })
    }

    pub fn default(&self) -> &Forge {
        &self.forges[&self.default_name]
    }

    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    pub fn names(&self) -> Vec<String> {
        self.forges.keys().cloned().collect()
    }

    pub fn all(&self) -> impl Iterator<Item = &Forge> {
        self.forges.values()
    }

    /// Match a clone URL against configured forge hosts (reverse lookup).
    ///
    /// `https://gitlab.com/group/sub/project.git` → forge `gitlab`,
    /// owner `group/sub`, repo `project`.
    pub fn match_url(&self, url: &str) -> Option<ResolvedUrl> {
        let (host, path) = split_url(url)?;
        let host = host.to_ascii_lowercase();
        for forge in self.forges.values() {
            if forge.host().to_ascii_lowercase() == host {
                let mut segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                if segs.len() < 2 {
                    return None;
                }
                let repo = segs.pop().unwrap();
                let repo = repo.strip_suffix(".git").unwrap_or(repo);
                if repo.is_empty() {
                    return None;
                }
                return Some(ResolvedUrl {
                    forge: forge.name.clone(),
                    owner: segs.join("/"),
                    repo: repo.to_string(),
                });
            }
        }
        None
    }
}

/// Split `scheme://host[:port]/path` (or `git@host:path`) into
/// `(host, path)`.
fn split_url(url: &str) -> Option<(String, String)> {
    if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "http" | "https" | "git" | "ssh") {
            return None;
        }
        let (auth, path) = rest.split_once('/')?;
        let host = auth.rsplit('@').next().unwrap_or(auth);
        // strip :port
        let host = host.split(':').next().unwrap_or(host);
        if host.is_empty() {
            return None;
        }
        return Some((host.to_string(), path.to_string()));
    }
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some((host.to_string(), path.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(entries: &[(&str, ForgeDef)]) -> Registry {
        let mut section = ForgeSection::default();
        for (name, def) in entries {
            section.entries.insert(name.to_string(), def.clone());
        }
        Registry::new(&section).unwrap()
    }

    #[test]
    fn builtin_clone_urls() {
        let r = registry(&[]);
        assert_eq!(
            r.default()
                .clone_url("femboygamertechguy", "GitFull")
                .unwrap(),
            "https://github.com/femboygamertechguy/GitFull.git"
        );
        let cb = r.get("codeberg").unwrap();
        assert_eq!(
            cb.clone_url("org", "repo").unwrap(),
            "https://codeberg.org/org/repo.git"
        );
    }

    #[test]
    fn generic_forge_from_config() {
        let mut def = ForgeDef::default();
        def.kind = Some("generic".into());
        def.host = Some("git.example.com".into());
        def.clone_template = Some("https://{host}/src/{owner}/{repo}.git".into());
        let r = registry(&[("mirror", def)]);
        let f = r.get("mirror").unwrap();
        assert_eq!(
            f.clone_url("acme", "widgets").unwrap(),
            "https://git.example.com/src/acme/widgets.git"
        );
    }

    #[test]
    fn port_and_vars() {
        let mut def = ForgeDef::default();
        def.kind = Some("gitea".into());
        def.host = Some("gitea.internal".into());
        def.scheme = Some("https".into());
        def.port = Some(3000);
        let r = registry(&[("internal", def)]);
        assert_eq!(
            r.get("internal").unwrap().clone_url("o", "r").unwrap(),
            "https://gitea.internal:3000/o/r.git"
        );
    }

    #[test]
    fn url_matching() {
        let r = registry(&[]);
        let m = r.match_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(
            (m.forge.as_str(), m.owner.as_str(), m.repo.as_str()),
            ("github", "owner", "repo")
        );
        let m = r
            .match_url("https://gitlab.com/group/sub/project.git")
            .unwrap();
        assert_eq!(m.owner, "group/sub");
        assert_eq!(m.repo, "project");
        assert!(r.match_url("https://unknown.host/o/r.git").is_none());
        assert!(r.match_url("https://github.com/onlyrepo").is_none());
    }

    #[test]
    fn unknown_kind_requires_template() {
        let mut def = ForgeDef::default();
        def.kind = Some("sourcehut".into());
        def.host = Some("sr.ht".into());
        let r = registry(&[("sh", def)]);
        assert!(r.get("sh").unwrap().clone_url("o", "r").is_err());
    }

    #[test]
    fn api_base_derivation() {
        let r = registry(&[]);
        assert_eq!(
            r.get("github").unwrap().api_base().as_deref(),
            Some("https://api.github.com")
        );
        assert_eq!(
            r.get("gitlab").unwrap().api_base().as_deref(),
            Some("https://gitlab.com/api/v4")
        );
        assert_eq!(
            r.get("codeberg").unwrap().api_base().as_deref(),
            Some("https://codeberg.org/api/v1")
        );
        // searchable() follows api_base
        assert!(r.get("github").unwrap().searchable());

        // explicit override wins (trailing '/' tolerated)
        let mut def = ForgeDef::default();
        def.kind = Some("gitea".into());
        def.host = Some("gitea.internal".into());
        def.api_base = Some("http://127.0.0.1:8080/api/v1/".into());
        let r2 = registry(&[("internal", def)]);
        assert_eq!(
            r2.get("internal").unwrap().api_base().as_deref(),
            Some("http://127.0.0.1:8080/api/v1")
        );

        // cgit has no REST search API → not searchable
        let mut def = ForgeDef::default();
        def.kind = Some("cgit".into());
        def.host = Some("cgit.example.com".into());
        let r3 = registry(&[("cz", def)]);
        assert!(!r3.get("cz").unwrap().searchable());
    }
}
