//! Config schema tests, including the shipped example configuration.

use gitfull::config::{Config, FileConfig};
use std::fs;
use std::path::{Path, PathBuf};

fn tmpfile(name: &str, content: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("gitfull-cfg-{name}-{}.toml", std::process::id()));
    fs::write(&p, content).unwrap();
    p
}

#[test]
fn example_config_parses_and_validates() {
    let text = include_str!("../config/gitfull.conf.example");
    let path = tmpfile("example", text);
    let (cfg, warnings) = Config::load(&path).unwrap();

    // forges: builtins + example-defined, no code changes needed
    for name in ["github", "gitlab", "codeberg", "mirror", "gitea-selfhost"] {
        cfg.forges
            .get(name)
            .unwrap_or_else(|_| panic!("forge {name} missing"));
    }
    assert_eq!(cfg.forges.default_name(), "github");
    // example customizes the builtin github with a token_env
    assert_eq!(
        cfg.forges
            .get("github")
            .unwrap()
            .token_env()
            .map(|s| s.to_string()),
        Some("GITFULL_TOKEN".to_string())
    );
    // generic forge renders from its template
    let mirror = cfg.forges.get("mirror").unwrap();
    assert_eq!(
        mirror.clone_url("acme", "widgets").unwrap(),
        "https://git.example.com/src/acme/widgets.git"
    );

    // per-repo overrides
    let ov = cfg.repo_override_for("acme", "secret-tool").unwrap();
    assert_eq!(ov.forge.as_deref(), Some("mirror"));
    assert_eq!(ov.git_ref.as_deref(), Some("v1.2.0"));
    assert_eq!(ov.toolchains.len(), 1);
    assert_eq!(ov.packages.len(), 1);

    // policy extensions
    assert!(cfg
        .policy
        .extra_forbidden_programs
        .iter()
        .any(|p| p == "company-pm"));

    // warnings only for genuinely unknown top-level sections
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let _ = fs::remove_file(&path);
}

#[test]
fn defaults_when_missing() {
    let (cfg, warnings) = Config::load(Path::new("/nonexistent/gitfull.conf")).unwrap();
    assert!(!warnings.is_empty());
    assert_eq!(cfg.root, PathBuf::from("/var/lib/gitfull"));
    assert_eq!(cfg.bin_dir, PathBuf::from("/usr/local/bin"));
    assert!(cfg.jobs >= 1);
    assert_eq!(cfg.forges.default_name(), "github");
    assert!(cfg.repos.is_empty());
}

#[test]
fn unknown_keys_in_fixed_sections_are_errors() {
    // typo in [core] must fail loudly
    let e = toml::from_str::<FileConfig>("[core]\nrot = \"/x\"\n");
    assert!(e.is_err(), "expected deny_unknown_fields to reject `rot`");
    // typo in [policy]
    let e = toml::from_str::<FileConfig>("[policy]\nextra_forbiden = []\n");
    assert!(e.is_err());
    // unknown repo override key
    let e = toml::from_str::<FileConfig>("[repo.\"a/b\"]\nforge = \"x\"\nrefs = \"v1\"\n");
    assert!(e.is_err());
}

#[test]
fn unknown_forge_keys_are_accepted() {
    // extensibility: future forge features must not break older gitfull
    let text = r#"
[forge.future]
kind = "generic"
host = "forge.example.com"
some_future_key = "whatever"
clone_template = "https://{host}/{owner}/{repo}.git"
"#;
    let path = tmpfile("future", text);
    let (cfg, _) = Config::load(&path).unwrap();
    assert!(cfg.forges.get("future").is_ok());
    let _ = fs::remove_file(&path);
}

#[test]
fn unknown_top_level_section_warns_not_errors() {
    let text = "[brandnewsection]\nfoo = 1\n";
    let path = tmpfile("unknown", text);
    let (_cfg, warnings) = Config::load(&path).unwrap();
    assert!(warnings.iter().any(|w| w.contains("brandnewsection")));
    let _ = fs::remove_file(&path);
}

#[test]
fn bad_config_values_fail() {
    let path = tmpfile("badcolor", "[core]\ncolor = \"purple\"\n");
    assert!(Config::load(&path).is_err());
    let _ = fs::remove_file(&path);

    let path = tmpfile("badforge", "[forge]\ndefault = \"nope\"\n");
    assert!(Config::load(&path).is_err());
    let _ = fs::remove_file(&path);

    let path = tmpfile("repoforge", "[repo.\"a/b\"]\nforge = \"missing\"\n");
    assert!(Config::load(&path).is_err());
    let _ = fs::remove_file(&path);
}
