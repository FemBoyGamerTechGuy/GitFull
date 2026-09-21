//! End-to-end ranked-search resolution against a local fake forge API.
//!
//! No external network: the test stands up a tiny std-only HTTP/1.1 server
//! (TcpListener + hand-written responses) and points all three forge
//! entries at it via `api_base` overrides. This exercises the real path
//! `install` takes for a bare package name:
//!
//!   curl (ForgeApi, exec chokepoint) -> response parsing (json.rs)
//!   -> candidate ranking -> GitHub enrichment via Link headers
//!   -> visible resolution -> concrete forge:owner/repo spec
//!
//! The fake server records every request path so the test can assert which
//! endpoints were consulted (search on all forge kinds + detail queries).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gitfull::config::Config;
use gitfull::gitproc::ExecCtx;
use gitfull::search;
use gitfull::spec::{PkgSpec, Source};

// ---------------------------------------------------------------------------
// the fake forge API
// ---------------------------------------------------------------------------

struct FakeForgeApi {
    addr: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl FakeForgeApi {
    fn start() -> FakeForgeApi {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = match conn {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let hits = hits2.clone();
                std::thread::spawn(move || {
                    // read request head (GET: no body)
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match conn.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    if let Ok(mut h) = hits.lock() {
                        h.push(path.clone());
                    }

                    let (status, extra_headers, body) = route(&path);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n{extra_headers}\
                         \r\n{body}",
                        body.len()
                    );
                    let _ = conn.write_all(resp.as_bytes());
                    let _ = conn.flush();
                });
            }
        });
        FakeForgeApi { addr, hits }
    }

    fn hit_paths(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn hit_count(&self, needle: &str) -> usize {
        self.hit_paths()
            .iter()
            .filter(|p| p.contains(needle))
            .count()
    }
}

fn route(path: &str) -> (&'static str, String, String) {
    // ---- GitHub search -----------------------------------------------------
    if path.starts_with("/search/repositories") {
        if path.contains("q=missing") {
            return (
                "200 OK",
                String::new(),
                r#"{"total_count": 0, "items": []}"#.to_string(),
            );
        }
        let body = r#"{
          "total_count": 2,
          "items": [
            {"full_name": "octocat/Hello-World",
             "stargazers_count": 142,
             "pushed_at": "2026-08-30T08:21:00Z"},
            {"full_name": "someone/hello",
             "stargazers_count": 7,
             "pushed_at": "2019-02-01T00:00:00Z"}
          ]
        }"#;
        return ("200 OK", String::new(), body.to_string());
    }
    // ---- GitHub detail: contributors / commits (Link-header pagination) ---
    if path.contains("/repos/octocat/Hello-World/contributors") {
        return (
            "200 OK",
            format!(
                "Link: <{}/repos/octocat/Hello-World/contributors?per_page=1&page=2>; rel=\"next\", \
                 <{}/repos/octocat/Hello-World/contributors?per_page=1&page=89>; rel=\"last\"\r\n",
                "https://api.github.com", "https://api.github.com"
            ),
            r#"[{"login":"a"}]"#.to_string(),
        );
    }
    if path.contains("/repos/octocat/Hello-World/commits") {
        return (
            "200 OK",
            format!(
                "Link: <{}/repos/octocat/Hello-World/commits?per_page=1&page=2>; rel=\"next\", \
                 <{}/repos/octocat/Hello-World/commits?per_page=1&page=1400>; rel=\"last\"\r\n",
                "https://api.github.com", "https://api.github.com"
            ),
            r#"[{"sha":"a"}]"#.to_string(),
        );
    }
    if path.contains("/repos/someone/hello/contributors") {
        // single page, no Link header → 1 contributor
        return ("200 OK", String::new(), r#"[{"login":"a"}]"#.to_string());
    }
    if path.contains("/repos/someone/hello/commits") {
        return ("200 OK", String::new(), r#"[{"sha":"a"}]"#.to_string());
    }
    // ---- GitLab projects (api_base = http://…, path /projects) ----------
    if path.starts_with("/projects") {
        if path.contains("search=missing") {
            return ("200 OK", String::new(), "[]".to_string());
        }
        let body = r#"[
          {"id": 7, "path_with_namespace": "gnome/hello",
           "star_count": 31, "last_activity_at": "2026-06-02T11:00:00Z"}
        ]"#;
        return ("200 OK", String::new(), body.to_string());
    }
    // ---- Gitea/Forgejo search (api_base = http://…, path /repos/search) --
    if path.starts_with("/repos/search") {
        if path.contains("q=missing") {
            return ("200 OK", String::new(), r#"{"ok": true, "data": []}"#.to_string());
        }
        let body = r#"{"ok": true, "data": [
          {"full_name": "tools/hello", "stars_count": 12,
           "updated_at": "2026-01-05T09:00:00Z"}
        ]}"#;
        return ("200 OK", String::new(), body.to_string());
    }
    ("404 Not Found", String::new(), "{}".to_string())
}

// ---------------------------------------------------------------------------
// fixture config: all three built-in forges pointed at the fake API
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str, api_addr: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "gitfull-search-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("root")).unwrap();
        std::fs::write(
            root.join("gitfull.conf"),
            format!(
                "[core]\nroot = \"{}\"\nbin_dir = \"{}\"\njobs = 2\n\n\
                 [forge.github]\nkind = \"github\"\nhost = \"fake.invalid\"\napi_base = \"{api}\"\n\n\
                 [forge.gitlab]\nkind = \"gitlab\"\nhost = \"fake.invalid\"\napi_base = \"{api}\"\n\n\
                 [forge.codeberg]\nkind = \"forgejo\"\nhost = \"fake.invalid\"\napi_base = \"{api}\"\n",
                root.join("root").display(),
                root.join("bin").display(),
                api = api_addr
            ),
        )
        .unwrap();
        Fixture { root }
    }

    fn cfg(&self) -> Config {
        let (cfg, _) = Config::load(&self.root.join("gitfull.conf")).unwrap();
        cfg
    }

    fn ctx(&self, cfg: &Config) -> ExecCtx {
        ExecCtx {
            audit_log: None,
            extra_forbidden: Vec::new(),
            redactions: Vec::new(),
            resolve_path: cfg.host_tool_path.clone(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn have_curl() -> bool {
    std::path::Path::new("/usr/bin/curl").is_file() || std::path::Path::new("/bin/curl").is_file()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn bare_name_resolves_to_top_ranked_repo() {
    if !have_curl() {
        eprintln!("skipping search e2e: curl not present");
        return;
    }
    let api = FakeForgeApi::start();
    let fx = Fixture::new("ranked", &api.addr);
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);

    let spec = PkgSpec::parse("hello").unwrap();
    assert!(matches!(spec.source, Source::Search { .. }));

    let resolved = search::resolve(&cfg, &ctx, &spec).unwrap();

    // top-ranked candidate: octocat/Hello-World (142 stars, enriched with
    // 89 contributors / 1400 commits) beats gnome/hello (31) and tools/hello (12)
    match &resolved.source {
        Source::Forge { forge, owner, repo } => {
            assert_eq!(forge.as_deref(), Some("github"));
            assert_eq!(owner, "octocat");
            assert_eq!(repo, "Hello-World");
        }
        other => panic!("expected a forge source, got {other:?}"),
    }
    assert_eq!(resolved.git_ref, None);

    // all three forge kinds were searched
    assert_eq!(api.hit_count("/search/repositories"), 1, "{:?}", api.hit_paths());
    assert_eq!(api.hit_count("/projects"), 1);
    assert_eq!(api.hit_count("/repos/search"), 1);

    // GitHub enrichment ran for the top candidates (contributors + commits)
    assert!(api.hit_count("/repos/octocat/Hello-World/contributors") >= 1);
    assert!(api.hit_count("/repos/octocat/Hello-World/commits") >= 1);
}

#[test]
fn search_spec_carries_ref_into_resolution() {
    if !have_curl() {
        eprintln!("skipping search e2e: curl not present");
        return;
    }
    let api = FakeForgeApi::start();
    let fx = Fixture::new("ref", &api.addr);
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);

    let spec = PkgSpec::parse("hello@v1.2").unwrap();
    let resolved = search::resolve(&cfg, &ctx, &spec).unwrap();
    assert_eq!(resolved.git_ref.as_deref(), Some("v1.2"));
    assert_eq!(resolved.source, Source::Forge {
        forge: Some("github".into()),
        owner: "octocat".into(),
        repo: "Hello-World".into(),
    });
}

#[test]
fn scoped_search_queries_only_that_forge() {
    if !have_curl() {
        eprintln!("skipping search e2e: curl not present");
        return;
    }
    let api = FakeForgeApi::start();
    let fx = Fixture::new("scoped", &api.addr);
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);

    let spec = PkgSpec::parse("codeberg:hello").unwrap();
    let resolved = search::resolve(&cfg, &ctx, &spec).unwrap();
    match &resolved.source {
        Source::Forge { forge, owner, repo } => {
            assert_eq!(forge.as_deref(), Some("codeberg"));
            assert_eq!(owner, "tools");
            assert_eq!(repo, "hello");
        }
        other => panic!("expected a forge source, got {other:?}"),
    }
    // ONLY the forgejo search endpoint was hit — no github/gitlab queries,
    // and no github detail enrichment
    let paths = api.hit_paths();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(paths[0].starts_with("/repos/search"), "{paths:?}");
}

#[test]
fn unknown_forge_scope_is_a_clear_error() {
    let api = FakeForgeApi::start();
    let fx = Fixture::new("badscope", &api.addr);
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);
    let spec = PkgSpec::parse("nosuchforge:hello").unwrap();
    let err = search::resolve(&cfg, &ctx, &spec).unwrap_err();
    assert!(format!("{err}").contains("nosuchforge"), "{err}");
}

#[test]
fn no_matches_anywhere_is_an_error_with_alternatives() {
    if !have_curl() {
        eprintln!("skipping search e2e: curl not present");
        return;
    }
    let api = FakeForgeApi::start();
    let fx = Fixture::new("nomatch", &api.addr);
    let cfg = fx.cfg();
    let ctx = fx.ctx(&cfg);

    // all three forges return zero candidates for `missing`
    let spec = PkgSpec::parse("missing").unwrap();
    let err = search::resolve(&cfg, &ctx, &spec).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no repository matching `missing`"), "{msg}");
    assert!(msg.contains("forge:owner/repo"), "{msg}");
}
