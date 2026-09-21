# ARCHITECTURE.md — module map and design

## Module map

```
src/
├── main.rs        CLI: verb commands, arg parsing, doctor, exit codes
├── lib.rs         crate root + the isolation-model doc comment
├── error.rs       GitfullError (Policy/Privilege violations are loud and typed)
├── config.rs      /etc/gitfull.conf schema (serde), defaults, validation
├── forge.rs       forge registry: data-driven forges, URL rendering,
│                  URL→forge reverse matching, clone templates,
│                  search-API bases
├── spec.rs        package spec parsing (owner/repo, forge:o/r, @ref,
│                  URLs, local paths, bare names → Search specs)
├── gitproc.rs     THE exec chokepoint: forbidden-program denylist,
│                  hermetic child envs, audit log, sealed git ops
│                  (clone w/ progress streaming, ls-remote, rev-parse),
│                  curl download, curl forge-API GETs
├── progress.rs    git progress-line parsing + live bar/ETA/MB renderer
├── sandbox.rs     per-app sandbox layout + build environment assembly
├── privilege.rs   root-privilege model: euid check, per-command gating
├── search.rs      ranked forge search: candidate fetch, enrichment,
│                  scoring, visible resolution (bare-name installs)
├── json.rs        hand-written JSON parser (search responses; no crates)
├── toolchain.rs   shared versioned toolchains: catalog, scanning,
│                  constraint-aware selection, seed-GCC plan,
│                  latest-tag auto-detection
├── bootstrap.rs   seed-GCC execution (the single host touch), from-source
│                  component builds, and automatic provisioning
│                  (ensure_components — used by install)
├── manifest.rs    build-system auto-detection (+ optional gitfull.toml)
├── resolver.rs    version constraints, implicit toolchain needs,
│                  per-repo requirement merging, cycle detection
├── planner.rs     install pipeline: resolve (search?) → clone → detect →
│                  dep walk → toolchain select (+ auto-provision) →
│                  build → stage → collect → install_binaries (the
│                  single sandbox escape) → meta
└── sha256.rs      dependency-free SHA-256 (provenance hashes)
```

## Install pipeline

```
 spec ─▶ resolve_source ─▶ sandbox create ─▶ clone (progress UI)
   │            ▲
   │            └─ bare `name` spec? ─▶ search::resolve first:
   │               query every configured forge's search API, rank by
   │               stars/contributors/commits/recency, PRINT the ranked
   │               table + chosen forge:owner/repo, then continue with
   │               that concrete spec (explicit specs skip this)
   │
                                     │
                        ┌────────────┴─────────────┐
                        ▼                          ▼
                 resolve_repo               dependency walk
              (auto-detect build           (cycle-safe, deps cloned
               system, merge needs)         into this app's sandbox)
                        └────────────┬─────────────┘
                                     ▼
                        toolchain selection (shared, constraint-aware)
                        │ missing components → bootstrap::ensure_components
                        │   (AUTO-provision from source — seed GCC included;
                        │    never a package manager, never a manual step)
                        ▼
              build deps (Toolchain class, hermetic env)
                        ▼
              build app  ──▶ stage ──▶ collect final binaries
                        ▼
         install_binaries  ← the single sandbox-escape path
           (copy + chmod 0755 + SHA-256 + audit event)
                        ▼
                  meta.toml (install record)
```

`--dry-run` runs everything read-only up to and including the plan
printout (search resolution still happens and is printed; missing
toolchains are *planned*, not provisioned), then stops before any build.

## Privilege model (privilege.rs)

gitfull writes `<root>` (default `/var/lib/gitfull`) and copies final
binaries into a system-wide bin dir (default `/usr/local/bin`) — both
root-owned on a normal Linux install. **Every state-changing operation —
`install`, `update`, `remove`, toolchain builds with `--execute`, and the
auto-provisioning that install triggers — therefore requires root**
(`sudo gitfull …`). The check is enforced twice: fail-fast in the CLI
(before any search/clone/network work) and inside the library entry
points (`planner::install/update/remove`,
`bootstrap::ensure_components`/`bootstrap_seed_gcc`/`build_component`),
so the API cannot bypass it. euid is read from `/proc/self/status` — no
libc dependency. Refusal exits with code 3 and prints the exact
`sudo gitfull …` line to re-run.

Read-only commands (`list`, `info`, `doctor`, `config`, `audit`) have no
gate: they create nothing and only read existing state.

**Dev/test carve-out (explicit, not silent):** when *both* `core.root`
and `core.bin_dir` are overridden away from the system defaults, gitfull
is deliberately operating on user-writable paths (throwaway sandboxes,
the test suite) and mutating commands are allowed without root. Keeping
either system path keeps the requirement — writing `/var/lib/gitfull`
OR copying into `/usr/local/bin` each need root on their own.

## Ranked forge search (search.rs + json.rs)

A bare `name` (no forge prefix, no `owner/repo` path) is parsed as a
`Source::Search` spec and resolved before anything is cloned:

1. **Query** — every configured forge with a search API is asked for up
   to 5 candidates (GitHub `/search/repositories?q=…+in:name`, GitLab
   `/projects?search=…`, Gitea/Forgejo `/repos/search?q=…`; forges
   without an `api_base` are skipped with a visible note). HTTP is
   `curl` under the `ForgeApi` exec class at the chokepoint:
   unauthenticated read-only GETs, no tokens ever attached, 20 s
   timeout, fully audit-logged. Responses are parsed by the
   hand-written JSON parser (`src/json.rs`) — the crate budget stays
   exactly `serde` + `toml`.
2. **Enrich** — for the top GitHub candidates only (≤3, to respect
   unauthenticated rate limits), contributor and commit counts are read
   from `Link`-header pagination (`?per_page=1` → `rel="last"` page
   number). Rate limits degrade softly: the candidate keeps `None` and
   is scored on its other signals.
3. **Rank** — each signal is log-normalized and weighted: stars 0.40
   (`log10(1+n)/6`), contributors 0.25 (`log10(1+n)/4`), commits 0.25
   (`log10(1+n)/6`), recency 0.10 (`2^(−days/365)`). Absent signals have
   their weights re-normalized across present ones, so a GitLab repo is
   not structurally punished for its forge's terser API. Ordering is
   deterministic: score desc, stars desc, full name asc.
4. **Resolve visibly** — the full ranked table and the chosen
   `forge:owner/repo` are printed **before anything is cloned or
   built**: auto-selection is visible, never silent. The explicit
   `forge:owner/repo` form bypasses ranking entirely.

`forge:name` scopes the same flow to one forge. `gitfull info <name>`
reuses the same resolution read-only.

## Filesystem layout

```
<root>/                              (default /var/lib/gitfull)
├── apps/<forge>-<owner>-<repo>/     one sandbox per installed app
│   ├── src/        git checkout (clone destination)
│   ├── build/      out-of-tree build dir
│   ├── stage/      DESTDIR staging (make/meson/cmake installs land here)
│   ├── prefix/     the app's in-sandbox install prefix
│   ├── deps/       dependency repos cloned + built inside THIS sandbox
│   ├── env/        HOME for build processes (no host HOME leaks in)
│   ├── tmp/        TMPDIR for build processes
│   ├── logs/       per-step build logs (01-configure.log, …)
│   └── meta.toml   install record: source, commit, toolchains used,
│                   binaries + hashes, timestamps
├── toolchains/<component>-<version>/   SHARED toolchains (gcc, python,
│   ├── bin/                            meson, ninja, cmake, vala, rust)
│   └── meta.toml    provenance (source, commit, built_by, date)
├── cache/           cloned sources, downloaded tarballs
├── logs/            toolchain build logs
└── audit.log        append-only: every exec + every binary copy
```

Apps **reference** shared toolchains rather than duplicating them;
toolchain selection is constraint-aware (`gcc>=13` picks the newest
installed 13.x+).

## The build environment (sandbox::build_env)

Every in-sandbox build runs with exactly this environment:

* `PATH` = `toolchains/*/bin` → dep prefixes' `bin` → this sandbox's
  `prefix/bin` → `core.host_tool_path`. The host compiler can never be
  resolved implicitly.
* `HOME`/`TMPDIR` inside the sandbox; `LC_ALL=C`; `SHELL=/bin/sh`.
* `CC`/`CXX` point at the toolchain-managed gcc/g++ (when required).
* Dep prefixes are exported via `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH`,
  `CPPFLAGS`, `LDFLAGS`.
* `DESTDIR` points at the sandbox staging dir.

Programs resolve against this PATH **only** — there is no host-PATH
fallback in `gitproc::resolve_program`.

## Build-system auto-detection (manifest.rs)

Detection priority, no extra files required in target repos:

| file present | build system | build commands |
|---|---|---|
| `meson.build` | meson | `meson setup build --prefix …` → `ninja -C build` → `ninja -C build install` (DESTDIR) |
| `CMakeLists.txt` | cmake | `cmake -S -B -GNinja` → `cmake --build` → `cmake --install` (DESTDIR) |
| `Cargo.toml` | cargo | `cargo build --release` (CARGO_TARGET_DIR in sandbox) |
| `configure` | autotools | `src/configure --prefix …` (out-of-tree) → `make -j` → `make install` (DESTDIR) |
| `Makefile` | make | `make -j` → `make install PREFIX=… DESTDIR=…` |

An *optional* `gitfull.toml` in a repo (or `[repo]` overrides in
gitfull.conf) can force the build system or declare explicit binaries —
never required.

## Dependency resolution (resolver.rs + planner dep walk)

* **Toolchain needs** are implied by the detected build system (meson →
  gcc + python≥3.8 + meson + ninja; cargo → rust; …) and merged with
  `[repo] toolchains = ["gcc>=13", …]` constraints (tighter bound wins).
* **Bare names** (`install meson`) are `Source::Search` specs — resolved
  through ranked forge search (see above) *before* the pipeline runs;
  `owner/repo` and `forge:owner/repo` specs never trigger a search.
* **Package needs** are `[repo] packages = ["owner/repo", …]` — other
  forge repos. The planner walks them breadth-first with a visited set
  and cycle detection (`a -> b -> a` is a hard error), cloning each into
  the requesting app's sandbox `deps/`, building it there, and exporting
  its prefix to later builds via the build env.
* Missing toolchains are never obtained from a host package manager
  (impossible — see the denylist); gitfull tells you the exact
  from-source command instead.

## Toolchain catalog & bootstrap

Components and sources are data (`toolchain::CATALOG`), overridable via
`[toolchain.sources]`:

| component | source | built with |
|---|---|---|
| gcc | git, gcc.gnu.org | **host cc, exactly once** (seed; `--disable-bootstrap`) |
| python | git, cpython | toolchain gcc |
| meson | git, mesonbuild | toolchain python (wrapper script, no compile) |
| ninja | git, ninja-build | toolchain python + gcc (`configure.py --bootstrap`) |
| cmake | git, kitware | toolchain gcc (autotools-style) |
| vala | git, gnome | toolchain gcc (needs glib at build time — roadmap: glib catalog entry) |
| rust | tarball, rust-lang.org | none (official dist, curl + tar) |

Seed-GCC version resolution order: `--version` flag →
`toolchain.seed_gcc_version` → `toolchain.preferences.gcc` → **latest**
release tag auto-detected via `git ls-remote` (no hardcoded default).

### Automatic provisioning (bootstrap::ensure_components)

The normal path never runs a manual bootstrap: during `install`, after
toolchain selection, every *missing* component is provisioned
automatically — cloned/fetched, built from source, and installed into
`toolchains/<comp>-<version>/` — before the app build starts. Ordering
respects build dependencies (seed GCC → python → meson/ninja/cmake/
vala…), the seed GCC uses the host compiler exactly once, and every
later component is built with already-provisioned toolchain tools.
`--dry-run` prints the provisioning plan instead. The `gitfull
toolchain build|bootstrap-gcc` subcommands remain as optional manual /
advanced overrides (version pinning, pre-warming) and share the same
code path.

## Exec classes (gitproc.rs)

Every spawn is classified, enforced, and audited:

| class | used for | guarantee |
|---|---|---|
| `SeedHostCompiler` | the seed-GCC build window only | the ONE host-compiler use |
| `FetchTool` | git, curl, download_prerequisites | sealed env, writes confined to cache/sandbox |
| `ForgeApi` | curl: read-only forge search/ranking GETs | unauthenticated, no tokens, writes nothing |
| `HostUtility` | POSIX utilities on sandbox paths | sandbox-path operands only |
| `Toolchain` | all post-seed builds | toolchain-managed compilers, hermetic env |

Cross-cutting, for **every** class: package-manager/escalator denylist,
fully-specified child env, audit-log append with secret redaction.

## Testing strategy

* unit: TOML/schema, forge URLs + templates + reverse matching, spec
  parsing (incl. bare-name → Search specs), progress-line parsing +
  rendering, constraint math, denylist, SHA-256 vectors, version
  comparison, catalog/scanning, JSON parser vectors, search-response
  parsing + Link-header pagination + RFC 3339 dates + scoring
  determinism, privilege decision core;
* integration: example-config parse, dependency-posture enforcement
  (exactly serde+toml, publish=false, proprietary LICENSE);
* e2e (no external network): local-path fixture + faked shared
  toolchain → full pipeline → binary crosses the sandbox escape → runs
  → hash-verified remove; reinstall wipe; dry-run; declared-bins;
  ranked search against a local fake forge API (std-only HTTP server:
  all three forge kinds queried, enrichment via Link headers,
  resolution printed, `forge:name` scoping, no-match errors);
* privilege: root required for install/update/remove on system paths,
  both-paths-overridden dev mode allowed, read-only commands ungated;
* packaging (tests/packaging.rs): distro recipes stay in sync — version
  strings match Cargo.toml across PKGBUILD / RPM spec / debian
  changelog / xbps template / man page, git+curl declared as the only
  runtime deps, no recipe code references `/var/lib` (packages create
  no runtime state at install time — gitfull's first state-changing
  run does), no maintainer scripts / RPM scriptlets, and the shipped
  example conf carries each format's no-clobber semantics
  (docs/PACKAGING.md);
* NOT tested here by design: the seed-GCC *execution* and component
  builds (they need a full Linux machine) and live forge APIs; their
  plan generation, version resolution, classification, URL building,
  and response parsing are tested.
