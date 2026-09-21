//! Shared library cache — `<root>/libs/`.
//!
//! Library dependencies discovered from build manifests
//! ([`crate::depgraph`]) are built from source with the
//! toolchain-managed compiler, and the built install prefix is then
//! **moved into this shared cache** so a second app needing the same
//! library does not rebuild it — mirroring how toolchain components
//! are shared under `<root>/toolchains/`.
//!
//! ```text
//! <root>/libs/<key>/
//! ├── bin/ include/ lib/ ...   (whatever the library installed)
//! └── meta.toml                (name, provides, source, commit, built_by)
//! ```
//!
//! **Identity and lookup.** The cache key is derived from the resolved
//! source identity (clone URL + ref), so it is deterministic before the
//! build runs. Lookup by *name* matches against `provides` — the names
//! the built tree actually provides, discovered from its own files
//! (`*.pc` pkg-config modules, `*Config.cmake` packages, `lib*.a/.so`
//! members). That way an app declaring `dependency('zlib')` reuses an
//! entry registered as `zlib` no matter which repo built it, and the
//! same physical library requested under an alias name is reused too.
//!
//! Like toolchain components, entries are content-addressed by source
//! and never mutated after registration; `gitfull remove` never touches
//! them (other apps may reference them — the same sharing semantics as
//! `<root>/toolchains/`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{GitfullError, Result};
use crate::sha256::sha256_hex;
use crate::util::sanitize_component;

/// Provenance record for one cached library.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibMeta {
    /// Name the dependency was declared as in the requesting build.
    pub name: String,
    /// Names the built tree provides (normalized, lowercase).
    pub provides: Vec<String>,
    /// Source the library was fetched from (clone URL / tarball URL /
    /// local path).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Cache keys of entries this library itself was built against (its
    /// link closure). When a later app reuses THIS entry by name, those
    /// entries must be on its link line too — e.g. a static `libfoo.a`
    /// whose objects reference `libbar.a` members. Expanded transitively
    /// by [`LibCache::closure_dirs`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    pub built_by: String,
    pub date_epoch: u64,
}

pub struct LibCache {
    pub dir: PathBuf,
}

impl LibCache {
    pub fn new(dir: PathBuf) -> LibCache {
        LibCache { dir }
    }

    /// Deterministic cache key for a resolved source identity:
    /// `<slug>-<8 hex of sha256(identity)>`. Same identity → same key,
    /// computed before anything is built. The slug is sanitized and
    /// stripped of leading dots/dashes so a key is always one plain,
    /// non-traversal directory component.
    pub fn identity_key(name: &str, identity: &str) -> String {
        let digest = sha256_hex(identity.as_bytes());
        let slug = sanitize_component(name)
            .trim_matches(|c| c == '.' || c == '-')
            .to_ascii_lowercase();
        let slug = if slug.is_empty() { "lib" } else { &slug };
        format!("{}-{}", slug, &digest[..8.min(digest.len())])
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Read the meta of an entry, if present.
    pub fn entry_meta(&self, key: &str) -> Option<LibMeta> {
        let meta_path = self.entry_dir(key).join("meta.toml");
        let text = fs::read_to_string(meta_path).ok()?;
        toml::from_str(&text).ok()
    }

    /// Find an existing entry whose `provides` covers `name_norm`
    /// (case-insensitive). Returns `(key, prefix_dir)`. Deterministic:
    /// entries are visited in sorted key order.
    pub fn find_providing(&self, name_norm: &str) -> Option<(String, PathBuf)> {
        let want = name_norm.to_ascii_lowercase();
        for key in self.keys() {
            if let Some(meta) = self.entry_meta(&key) {
                if meta
                    .provides
                    .iter()
                    .any(|p| p.to_ascii_lowercase() == want)
                {
                    let dir = self.entry_dir(&key);
                    return Some((key, dir));
                }
            }
        }
        None
    }

    /// Find the entry built from exactly `identity` (the planner's
    /// `"<clone url>|<ref>"` / local path / tarball URL), regardless of
    /// which *name* an earlier install requested it under — the same
    /// physical library fetched under an alias name must not rebuild.
    /// `meta.source` + `meta.git_ref` reconstruct the identity exactly.
    pub fn find_by_identity(&self, identity: &str) -> Option<(String, PathBuf)> {
        for key in self.keys() {
            if let Some(meta) = self.entry_meta(&key) {
                let reconstructed =
                    format!("{}|{}", meta.source, meta.git_ref.unwrap_or_default());
                if &reconstructed == identity {
                    let dir = self.entry_dir(&key);
                    return Some((key, dir));
                }
            }
        }
        None
    }

    /// All registered entry keys (sorted).
    pub fn keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().join("meta.toml").is_file() {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    /// The entry dir for `key` plus the dirs of every entry it
    /// transitively `requires` (cycle-safe, best-effort: a missing or
    /// unreadable entry is skipped rather than fatal — its provider may
    /// have been pruned or hand-removed).
    ///
    /// This is what makes a *reused* library linkable: a static archive
    /// whose members reference symbols from the libraries it was built
    /// against needs those on the link line too, and the build system's
    /// own `.pc` `Requires:` chain resolves only if every closure
    /// member's pkgconfig dir is on `PKG_CONFIG_PATH`.
    pub fn closure_dirs(&self, key: &str) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = vec![key.to_string()];
        while let Some(k) = stack.pop() {
            if !seen.insert(k.clone()) {
                continue;
            }
            let dir = self.entry_dir(&k);
            if !dir.join("meta.toml").is_file() {
                continue;
            }
            if let Some(meta) = self.entry_meta(&k) {
                for r in &meta.requires {
                    if !seen.contains(r) {
                        stack.push(r.clone());
                    }
                }
            }
            out.push(dir);
        }
        out
    }

    /// Register a freshly built library: move its install prefix into
    /// the cache entry and write provenance. `from_prefix` must live on
    /// the same filesystem as the cache (both sit under `<root>/`).
    pub fn register(
        &self,
        key: &str,
        from_prefix: &Path,
        meta: &LibMeta,
    ) -> Result<PathBuf> {
        let dest = self.entry_dir(key);
        if dest.exists() {
            // already registered by a concurrent/prior pass with the same
            // identity: keep the existing entry (content-addressed)
            return Ok(dest);
        }
        fs::create_dir_all(&self.dir)?;
        fs::rename(from_prefix, &dest).map_err(|e| {
            GitfullError::Sandbox(format!(
                "moving built library {} into the shared cache {}: {e} \
                 (both must live under <root>)",
                from_prefix.display(),
                dest.display()
            ))
        })?;
        fs::write(dest.join("meta.toml"), toml::to_string(meta)?)?;
        Ok(dest)
    }
}

/// Discover which dependency names a built install prefix *provides*,
/// from the tree's own files:
///
/// * `lib/pkgconfig/*.pc` / `share/pkgconfig/*.pc` → module stems
///   (the authority for pkg-config/meson `dependency()` users);
/// * `lib*/cmake/**/<X>Config.cmake` / `<X>-config.cmake` → `X`
///   (CMake `find_package(X)` users);
/// * `lib*/lib<X>.a` / `lib<X>.so*` → `X` (autotools `-l<X>` users).
///
/// Everything is normalized to lowercase.
pub fn provided_names(prefix: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };

    for pc_dir in ["lib/pkgconfig", "share/pkgconfig", "lib64/pkgconfig"] {
        if let Ok(rd) = fs::read_dir(prefix.join(pc_dir)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(stem) = name.strip_suffix(".pc") {
                    add(stem);
                }
            }
        }
    }

    // cmake package configs (any lib dir, one or two levels of nesting)
    for lib_dir in ["lib", "lib64"] {
        let cmake_dir = prefix.join(lib_dir).join("cmake");
        if let Ok(rd) = fs::read_dir(&cmake_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if let Ok(rd2) = fs::read_dir(&p) {
                        for e2 in rd2.flatten() {
                            collect_cmake_config_name(&e2.file_name().to_string_lossy(), &mut add);
                        }
                    }
                } else {
                    collect_cmake_config_name(&e.file_name().to_string_lossy(), &mut add);
                }
            }
        }
    }

    for lib_dir in ["lib", "lib64"] {
        if let Ok(rd) = fs::read_dir(prefix.join(lib_dir)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(rest) = name.strip_prefix("lib") {
                    for suffix in [".so", ".a"] {
                        if let Some(stem) = rest.strip_suffix(suffix) {
                            // libfoo.so.1.2 → foo; skip version tails and
                            // static-only internal artifacts
                            let stem = stem
                                .split('.')
                                .next()
                                .unwrap_or(stem)
                                .to_string();
                            if !stem.is_empty() {
                                add(&stem);
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn collect_cmake_config_name(name: &str, add: &mut dyn FnMut(&str)) {
    for pattern in ["Config.cmake", "-config.cmake"] {
        if let Some(stem) = name.strip_suffix(pattern) {
            // <X>Config.cmake → X (also strip the common <X><X> redundancy)
            add(stem);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gitfull-libcache-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn identity_keys_are_deterministic_and_distinct() {
        let a = LibCache::identity_key("zlib", "https://github.com/madler/zlib.git|main");
        let b = LibCache::identity_key("zlib", "https://github.com/madler/zlib.git|main");
        let c = LibCache::identity_key("zlib", "https://example.com/other.git|main");
        assert_eq!(a, b, "same identity must map to the same key");
        assert_ne!(a, c, "different identities must not collide");
        assert!(a.starts_with("zlib-"));
        assert_eq!(a.rsplit('-').next().unwrap().len(), 8);
        // ugly names are sanitized to a single flat directory component —
        // no path separators, no "."/".." traversal component
        let evil = LibCache::identity_key("../evil", "x");
        assert!(!evil.contains('/'), "{evil}");
        let evil_component = evil.split('-').next().unwrap();
        assert!(evil_component != "." && evil_component != "..", "{evil}");
    }

    #[test]
    fn register_then_find_by_provides() {
        let d = tmpdir("register");
        let cache = LibCache::new(d.join("libs"));

        // a built prefix that looks like a real library install
        let prefix = d.join("built/zlib-prefix");
        fs::create_dir_all(prefix.join("lib/pkgconfig")).unwrap();
        fs::create_dir_all(prefix.join("include")).unwrap();
        fs::write(prefix.join("lib/pkgconfig/zlib.pc"), "Name: zlib\n").unwrap();
        fs::write(prefix.join("lib/libz.a"), b"").unwrap();
        fs::create_dir_all(prefix.join("lib/cmake/zlib")).unwrap();
        fs::write(prefix.join("lib/cmake/zlib/ZLIBConfig.cmake"), "").unwrap();

        let key = LibCache::identity_key("zlib", "https://example.com/zlib.git|main");
        let meta = LibMeta {
            name: "zlib".into(),
            provides: provided_names(&prefix),
            source: "https://example.com/zlib.git".into(),
            git_ref: Some("main".into()),
            commit: None,
            requires: Vec::new(),
            built_by: "gitfull toolchain (toolchain-managed gcc)".into(),
            date_epoch: 0,
        };
        assert!(meta.provides.contains(&"zlib".to_string()), "{:?}", meta.provides);
        cache.register(&key, &prefix, &meta).unwrap();

        // prefix was MOVED into the cache
        assert!(!prefix.exists());
        assert!(cache.entry_dir(&key).join("lib/pkgconfig/zlib.pc").is_file());

        // find by a differently-cased declared name
        let hit = cache.find_providing("ZLIB");
        assert!(hit.is_some());
        let (k, dir) = hit.unwrap();
        assert_eq!(k, key);
        assert!(dir.join("meta.toml").is_file());

        // find by an alias name only present in provides
        assert!(cache.find_providing("zlib").is_some());
        assert!(cache.find_providing("other-lib").is_none());

        // register is idempotent for the same identity: a second call
        // keeps the existing entry (content-addressed), never clobbers
        let meta2 = LibMeta { ..meta.clone() };
        let again = cache.register(&key, &d.join("nowhere"), &meta2);
        assert!(again.is_ok(), "existing entry must short-circuit, not error");
        assert_eq!(again.unwrap(), cache.entry_dir(&key));
        assert!(cache.entry_dir(&key).join("lib/pkgconfig/zlib.pc").is_file());
        assert!(cache.keys() == vec![key.clone()]);

        // but registering a genuinely new key from a missing dir errors
        let missing = cache.register(
            &LibCache::identity_key("ghost", "ghost-identity"),
            &d.join("nowhere"),
            &meta2,
        );
        assert!(missing.is_err(), "missing source dir must error, not create");

        // identity lookup: meta.source + meta.git_ref reconstruct the
        // identity, including under an alias key name
        assert_eq!(
            cache.find_by_identity("https://example.com/zlib.git|main"),
            Some((key.clone(), cache.entry_dir(&key)))
        );
        assert_eq!(cache.find_by_identity("https://example.com/zlib.git|"), None);

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn closure_dirs_expand_transitively_and_survive_cycles() {
        let d = tmpdir("closure");
        let cache = LibCache::new(d.join("libs"));

        // three entries: c requires b, b requires a, a requires nothing —
        // and a deliberately bogus self-requiring entry
        let mk = |name: &str, requires: Vec<String>| {
            let key = LibCache::identity_key(name, &format!("id-{name}"));
            let prefix = d.join(format!("built-{name}"));
            fs::create_dir_all(prefix.join("lib")).unwrap();
            fs::write(prefix.join("lib/.keep"), b"").unwrap();
            let meta = LibMeta {
                name: name.into(),
                provides: vec![name.into()],
                source: format!("id-{name}"),
                git_ref: None,
                commit: None,
                requires,
                built_by: "t".into(),
                date_epoch: 0,
            };
            cache.register(&key, &prefix, &meta).unwrap();
            key
        };
        let ka = mk("a", vec![]);
        let kb = mk("b", vec![ka.clone()]);
        let kc = mk("c", vec![kb.clone()]);
        // self-cycle + dangling reference: must be survived, not hang
        let _kz = mk("z", vec![
            LibCache::identity_key("z", "id-z"),
            "ghost-key-that-does-not-exist".into(),
        ]);

        let closure = cache.closure_dirs(&kc);
        assert_eq!(closure.len(), 3, "{closure:?}");
        assert!(closure.contains(&cache.entry_dir(&ka)));
        assert!(closure.contains(&cache.entry_dir(&kb)));
        assert!(closure.contains(&cache.entry_dir(&kc)));
        // order: the entry itself first
        assert_eq!(closure[0], cache.entry_dir(&kc));

        // self-cycle terminates with just the one real entry
        let zc = cache.closure_dirs(&LibCache::identity_key("z", "id-z"));
        assert_eq!(zc, vec![cache.entry_dir(&LibCache::identity_key("z", "id-z"))]);

        // unknown key: empty, not an error
        assert!(cache.closure_dirs("nope").is_empty());

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn provided_names_from_a_realistic_tree() {
        let d = tmpdir("provides");
        // mimics what `make install` of a typical library produces
        for f in [
            "lib/pkgconfig/libpng.pc",
            "lib/pkgconfig/libpng16.pc",
            "share/pkgconfig/harfbuzz.pc",
        ] {
            let p = d.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "Name: x\n").unwrap();
        }
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("lib/libpng16.so.16.42.0"), b"").unwrap();
        fs::write(d.join("lib/libpng16.a"), b"").unwrap();
        fs::create_dir_all(d.join("lib/cmake/harfbuzz")).unwrap();
        fs::write(d.join("lib/cmake/harfbuzz/harfbuzz-config.cmake"), "").unwrap();

        let names = provided_names(&d);
        assert!(names.contains(&"libpng".to_string()), "{names:?}");
        assert!(names.contains(&"libpng16".to_string()), "{names:?}");
        assert!(names.contains(&"harfbuzz".to_string()), "{names:?}");
        let _ = fs::remove_dir_all(&d);
    }
}
