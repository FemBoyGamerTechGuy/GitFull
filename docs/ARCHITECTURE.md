# ARCHITECTURE.md — module map and design

## Module map

```
src/
├── main.rs        CLI: verb commands, arg parsing, doctor, exit codes
├── lib.rs         crate root + the isolation-model doc comment
├── error.rs       GitfullError (Policy violations are loud and typed)
├── config.rs      /etc/gitfull.conf schema (serde), defaults, validation
├── forge.rs       forge registry: data-driven forges, URL rendering,
│                  URL→forge reverse matching, clone templates
├── spec.rs        package spec parsing (owner/repo, forge:o/r, @ref,
│                  URLs, local paths)
├── gitproc.rs     THE exec chokepoint: forbidden-program denylist,
│                  hermetic child envs, audit log, sealed git ops
│                  (clone w/ progress streaming, ls-remote, rev-parse),
│                  curl download
├── progress.rs    git progress-line parsing + live bar/ETA/MB renderer
├── sandbox.rs     per-app sandbox layout + build environment assembly
├── toolchain.rs   shared versioned toolchains: catalog, scanning,
│                  constraint-aware selection, seed-GCC plan,
│                  latest-tag auto-detection
├── bootstrap.rs   seed-GCC execution (the single host touch) and
│                  from-source component builds
├── manifest.rs    build-system auto-detection (+ optional gitfull.toml)
├── resolver.rs    version constraints, implicit toolchain needs,
│                  per-repo requirement merging, cycle detection
├── planner.rs     install pipeline: resolve → clone → detect → dep walk
│                  → toolchain select → build → stage → collect →
│                  install_binaries (the single sandbox escape) → meta
└── sha256.rs      dependency-free SHA-256 (provenance hashes)
```

## Install pipeline

```
 spec ─▶ resolve_source ─▶ sandbox create ─▶ clone (progress UI)
                                     │
                        ┌────────────┴─────────────┐
                        ▼                          ▼
                 resolve_repo               dependency walk
              (auto-detect build           (cycle-safe, deps cloned
               system, merge needs)         into this app's sandbox)
                        └────────────┬─────────────┘
                                     ▼
                        toolchain selection (shared, constraint-aware)
                        │ missing gcc → "run bootstrap-gcc" (never a PM)
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
printout, then stops before any build.

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

## Exec classes (gitproc.rs)

Every spawn is classified, enforced, and audited:

| class | used for | guarantee |
|---|---|---|
| `SeedHostCompiler` | the seed-GCC build window only | the ONE host-compiler use |
| `FetchTool` | git, curl, download_prerequisites | sealed env, writes confined to cache/sandbox |
| `HostUtility` | POSIX utilities on sandbox paths | sandbox-path operands only |
| `Toolchain` | all post-seed builds | toolchain-managed compilers, hermetic env |

Cross-cutting, for **every** class: package-manager/escalator denylist,
fully-specified child env, audit-log append with secret redaction.

## Testing strategy

* unit: TOML/schema, forge URLs + templates + reverse matching, spec
  parsing, progress-line parsing + rendering, constraint math, denylist,
  SHA-256 vectors, version comparison, catalog/scanning;
* integration: example-config parse, dependency-posture enforcement
  (exactly serde+toml, publish=false, proprietary LICENSE);
* e2e (no network): local-path fixture + faked shared toolchain → full
  pipeline → binary crosses the sandbox escape → runs → hash-verified
  remove; reinstall wipe; dry-run; declared-bins;
* NOT tested here by design: the seed-GCC *execution* and component
  builds (they need a full Linux machine); their plan generation,
  version resolution, and classification are tested.
