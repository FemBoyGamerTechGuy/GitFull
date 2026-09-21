# CONFIG.md — the `/etc/gitfull.conf` reference

gitfull's system-wide configuration lives at `/etc/gitfull.conf`
(per-invocation override: `gitfull --config <path> …`). The format is
TOML, parsed with `serde` + `toml` — the only crates gitfull depends on.

The file is **optional**: gitfull runs on built-in defaults until it
exists (and warns that it is doing so). An annotated example ships at
`config/gitfull.conf.example`.

Design principles:

* **Forges are data.** New forges are added with a `[forge.<name>]`
  entry — no code changes (§2).
* **Fixed sections reject unknown keys** (typos fail loudly), but forge
  entries and top-level *sections* are forward-compatible: unknown keys
  inside a forge entry are accepted, unknown top-level sections warn.

---

## 1. `[core]` — global settings

| key | type | default | meaning |
|---|---|---|---|
| `root` | path | `/var/lib/gitfull` | everything gitfull writes lives under here (apps, toolchains, cache, logs, audit.log) |
| `bin_dir` | path | `/usr/local/bin` | destination of the single sandbox-escape path (final binary copy) |
| `jobs` | int | CPU count | build parallelism (`-j`) |
| `host_tool_path` | string | `/usr/bin:/bin` | POSIX utilities available to in-sandbox builds (see docs/AUDIT.md §4; point at a busybox toolchain for full strictness) |
| `color` | string | `"auto"` | progress-bar colors: `auto` \| `always` \| `never` |

**Privilege note:** while either `root` or `bin_dir` keeps its system
default, all state-changing commands (`install`, `update`, `remove`,
`toolchain … --execute`) require root — `sudo gitfull …` (see
privilege.rs and docs/AUDIT.md §2.0). Overriding **both** away from the
system defaults switches to an explicit dev mode where mutating commands
run unprivileged (this is what the test suite uses).

## 2. `[forge]` — the forge registry

```toml
[forge]
default = "github"          # forge used for bare owner/repo specs

[forge.<name>]
kind           = "github"   # github | gitlab | gitea | forgejo | cgit | generic
host           = "github.com"
scheme         = "https"    # default https
port           = 3000       # optional
clone_template = "https://{host}/src/{owner}/{repo}.git"   # optional
api_base       = "https://git.corp.example.com/api/v1"      # optional: search API base
                            # (derived from kind when absent; cgit/generic forges
                            #  without one are skipped by ranked search)
token_env      = "GITFULL_TOKEN"    # optional: env var name for a read token
```

* **Built-ins**: `github`, `gitlab`, `codeberg` are pre-registered; a
  config entry with the same name *customizes* the built-in (the example
  config adds `token_env` to `github`).
* **Any other name defines a new forge.** For kinds gitfull has no URL
  rule for, or to override URL shape entirely, set `clone_template`.
  Valid template variables: `{name}` `{kind}` `{scheme}` `{host}`
  `{port}` `{owner}` `{repo}`. Unknown variables are configuration
  errors (fail loudly).
* **`generic` + template covers any forge** — cgit, cgit-fe, private
  mirrors, future forges: zero code changes, now or later.
* `api_base` (optional) is the REST base used by **ranked search** — the
  `install <name>` bare-name path. It is derived from `kind` for the
  known kinds (`https://api.github.com`, `https://<host>/api/v4`,
  `https://<host>/api/v1`); set it explicitly for forges behind a proxy
  or with a nonstandard layout. Queries are unauthenticated read-only
  GETs — `token_env` is deliberately NOT used for search. A forge
  without a derivable `api_base` (cgit, generic without one) is skipped
  with a visible note when a bare name is installed.
* Unknown keys inside a forge entry are accepted and ignored
  (forward compatibility for future forge features).
* The name `default` is reserved (it collides with the scalar
  `forge.default` key).
* **Tokens are never values in this file.** `token_env` names an
  environment variable; if set (and non-empty) it is embedded into the
  clone URL for that forge only, and redacted from every log. Example
  placeholder format (fake — never commit a real token):
  `github_pat_AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKKLLLLMMMMNNNNOOOO`

### Adding a forge without code changes — walkthrough

Your company runs Gitea at `git.corp.example.com:3000`:

```toml
[forge.corp]
kind = "gitea"
host = "git.corp.example.com"
port = 3000
token_env = "CORP_GIT_TOKEN"     # optional, for private repos
```

Now `gitfull install corp:team/tool` clones from
`https://git.corp.example.com:3000/team/tool.git`.

A cgit instance with a nonstandard path layout:

```toml
[forge.kernel-mirror]
kind = "generic"
host = "mirror.example.com"
clone_template = "https://{host}/cgit/{owner}/{repo}.git"
```

## 3. `[repo."owner/name"]` — per-repository overrides

Keys are quoted (they contain `/`). Works for any forge's repos; the
lookup key is the plain `owner/repo` (for URL installs, the owner/repo
the URL resolves to).

| key | type | meaning |
|---|---|---|
| `forge` | string | route this repo through a different forge (e.g. an internal mirror) |
| `ref` | string | pin a branch/tag/commit (CLI `@ref` wins over this) |
| `build_system` | string | override auto-detection: `autotools` \| `make` \| `meson` \| `cmake` \| `cargo` |
| `jobs` | int | per-repo build parallelism |
| `packages` | [string] | additional package deps (`"owner/repo"` specs), built inside this app's sandbox |
| `toolchains` | [string] | additional toolchain constraints, e.g. `"gcc>=13"` |
| `bins` | [string] | explicit final binaries to install (overrides staging-area scanning) |

Version constraint syntax: `component`, `component=V`,
`component>V`, `component>=V`, `component<V`, `component<=V`
(e.g. `gcc>=13.3.0`, `python=3.12`). Versions compare numerically per
dot segment (`13.9 < 13.10`).

## 4. `[dep.<name>]` — per-dependency resolution overrides

Keyed by the name a build manifest declares — meson
`dependency('name')`, cmake `find_package(Name)`, autotools
`AC_CHECK_LIB([name])`, a Makefile `pkg-config` module. Matching is
exact key first, then case-insensitive (manifest names arrive in their
native case: `find_package(ZLIB)` vs `[dep.zlib]`).

```toml
[dep.zlib]
source = "gitlab:madler/zlib"    # any spec form: forge:owner/repo,
ref = "develop"                  # owner/repo, a URL, or a local path
skip = false                     # never provision this dependency
```

| key | type | meaning |
|---|---|---|
| `source` | string | pin where this dependency comes from — use it when ranked search picks a repo you do not want, or to point at a fork, mirror, or local checkout (validated at config load) |
| `ref` | string | pin a branch/tag/commit; requires `source` |
| `skip` | bool | deliberate opt-out: never provision this dependency (e.g. a project declares a dependency it does not actually need) |

Without an override, dependency names discovered from manifests resolve
through generic layers only — shared library cache, meson wraps, then
ranked forge search (see docs/ARCHITECTURE.md, "Dependency-graph
discovery"). These overrides are *user configuration*, mirroring
`[toolchain.sources]`; gitfull itself contains no per-repo or
per-library name tables.

## 5. `[paths]` — directory overrides

All optional; each defaults to `<root>/<name>`:

| key | default |
|---|---|
| `apps` | `<root>/apps` |
| `toolchains` | `<root>/toolchains` |
| `libs` | `<root>/libs` |
| `cache` | `<root>/cache` |
| `logs` | `<root>/logs` |

`paths.libs` is the **shared library cache**: library dependencies
discovered from build manifests are built from source and registered
here (identity-keyed entries with `meta.toml` provenance and link
closure). Every later install needing the same library reuses the entry
instead of rebuilding; entries are never removed with an app
(toolchain-style sharing).

## 6. `[toolchain]` — toolchain management

| key | type | default | meaning |
|---|---|---|---|
| `seed_gcc_version` | string | *(unset)* | **There is no built-in default.** Unset means gitfull auto-detects the latest stable GCC release tag from the GCC git repo when bootstrap runs. Pin an older version here when needed. |
| `preferences.<component>` | string | *(unset)* | preferred version per component (used by `gitfull toolchain build`) |
| `sources.<component>` | string | *(catalog)* | source override per component — git URL, or tarball URL containing `{version}` |

Catalog components and default sources: `gcc` (gcc.gnu.org git),
`python` (github.com/python/cpython), `meson`
(github.com/mesonbuild/meson), `ninja` (github.com/ninja-build/ninja),
`cmake` (gitlab.com/cmake/cmake), `vala` (gitlab.gnome.org/GNOME/vala),
`rust` (static.rust-lang.org tarball).

## 7. `[policy]` — enforcement policy

| key | type | default | meaning |
|---|---|---|---|
| `extra_forbidden_programs` | [string] | `[]` | additional programs denied at the exec chokepoint (on top of the built-in package-manager + privilege-escalator denylist) |
| `allow_copyleft_targets` | bool | `true` | target apps may build copyleft code inside their own sandboxes (gitfull never links it — see docs/AUDIT.md §6) |

## 8. `[clone]` — clone behavior

| key | type | default | meaning |
|---|---|---|---|
| `depth` | int | *(unset)* | shallow clone depth (unset = full history) |
| `single_branch` | bool | `true` | clone only the target branch |
| `recurse_submodules` | bool | `false` | initialize submodules after clone |

## 9. TOML subset notes

* comments (`#`), `[section]` tables, quoted keys
  (`[repo."a/b"]`), strings, integers, booleans, arrays of strings are
  used;
* inline tables and multi-line strings are not used by the schema;
* unknown top-level sections produce **warnings**, not errors;
* unknown keys in `[core]`, `[paths]`, `[toolchain]`, `[policy]`,
  `[clone]`, and `[repo.*]` entries are **errors** (typo protection);
* unknown keys inside `[forge.*]` entries are accepted (extensibility).

## 10. Command-line overrides

`--config <path>` selects the file; `--root <path>` overrides
`core.root` (and re-derives the `[paths]` defaults). Global options go
**before** the command; command options (e.g. `--execute`, `--version`)
go after it.
