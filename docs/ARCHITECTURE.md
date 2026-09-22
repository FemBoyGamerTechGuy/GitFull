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
│                  scoring, visible resolution (bare-name installs);
│                  dependency-name candidates are returned UNCONFIRMED
├── libmap.rs      curated upstream map: well-known pkg-config module
│                  names -> their correct upstream repositories (the
│                  primary strategy for resolving module names; ranked
│                  search is only a flagged, never-auto-built fallback)
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
├── depgraph.rs    build-system-native dependency discovery: parses each
│                  build system's OWN manifest format (meson
│                  dependency()/wraps in their conditional context,
│                  cmake find_package()/find_library
│                  /pkg_check_modules, Cargo.toml, configure.ac,
│                  Makefile pkg-config calls) into declared deps —
│                  generic across repos, no per-repo name tables
├── mesoneval.rs   meson conditional-structure evaluation for depgraph:
│                  if/elif/else reachability per platform (machine
│                  system/cpu_family/endian), tri-state and/or/not,
│                  subdir() scope sharing — provably-dead branches only
├── libcache.rs   shared library cache <root>/libs/: content-addressed
│                  entries for built library deps (provides-matching,
│                  identity lookup, transitive link closure)
├── planner.rs     install pipeline: resolve (search?) → clone → detect →
│                  dep-graph discovery + walk (cycle-safe) → toolchain
│                  select (+ auto-provision) → build deps leaves-first
│                  (registering each in <root>/libs/) → build app →
│                  stage → collect → install_binaries (the single
│                  sandbox escape) → meta
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
                 resolve_repo           dep-graph discovery
              (auto-detect build      (depgraph.rs parses the repo's OWN
               system, merge needs)    manifests; every dep resolved,
                                        fetched w/ progress UI, cycle-safe)
                        └────────────┬─────────────┘
                                     ▼
                        toolchain selection (shared, constraint-aware)
                        │ missing components → bootstrap::ensure_components
                        │   (AUTO-provision from source — seed GCC included;
                        │    never a package manager, never a manual step)
                        ▼
              build deps leaves-first, registering each in the
              shared library cache <root>/libs/ (identity-keyed;
              a second app needing the same library never rebuilds)
              (Toolchain class, hermetic env)
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
├── libs/<slug>-<hash>/           SHARED library cache: built library
│   ├── bin/ include/ lib/ …      dependencies (provides-matching,
│   └── meta.toml                 identity-keyed, link closure) — never
│                                 removed with an app, reused by every
│                                 later install needing the same lib
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
* Dep prefixes — every shared-cache entry in the app's link closure —
  are exported via `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH`, `CPPFLAGS`,
  `LDFLAGS`, so the build system's own dependency resolution (meson
  `dependency()`, cmake `find_package()`, pkg-config `Requires:` chains)
  resolves against provisioned libraries.
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

## Dependency resolution (resolver.rs + depgraph.rs + planner)

* **Toolchain needs** are implied by the detected build system (meson →
  gcc + python≥3.8 + meson + ninja; cargo → rust; …) and merged with
  `[repo] toolchains = ["gcc>=13", …]` constraints (tighter bound wins).
* **Bare names** (`install meson`) are `Source::Search` specs — resolved
  through ranked forge search (see above) *before* the pipeline runs;
  `owner/repo` and `forge:owner/repo` specs never trigger a search.
* **Package needs** are `[repo] packages = ["owner/repo", …]` — other
  forge repos.
* **Library needs** are discovered from the repo's own build manifests
  by [`depgraph.rs`](#dependency-graph-discovery-depgraphrs) — the fix
  for "the build tool was provisioned but its dependency resolution
  still failed": gitfull now parses what the project *itself declares*
  and provisions every declared library, transitively.

The planner walks the full graph breadth-first with an identity-keyed
visited set. A back-edge onto an ancestor is a cycle (hard error); an
edge onto an already-visited non-ancestor is a diamond (deduplicated,
linked once). Each dependency is cloned into the requesting app's
sandbox `deps/` **with the same live progress UI as every other clone**
(main repo, toolchain components, dependencies — one mechanism), built
with the toolchain-managed compiler, and its install prefix is moved
into the shared library cache. Builds run leaves-first (topological
order), so a dependency's own dependencies are linkable while it builds.

## Dependency-graph discovery (depgraph.rs)

gitfull does **not** stop at "this project uses meson": it parses the
build system's **own dependency declarations** in the cloned source tree
and provisions every declared library dependency from source, exactly
like it provisions toolchain components.

| build system | files parsed | declarations recognized |
|---|---|---|
| meson | every `meson.build`, plus `subprojects/*.wrap` | `dependency('name', …)` calls (incl. multi-line, `required: false`, `version:`) **in their conditional context** — see the next section; `[wrap-git]`/`[wrap-file]` subprojects (with `[provide] dependency_names`) |
| cmake | every `CMakeLists.txt` and `*.cmake` | `find_package(Name [ver] [REQUIRED])`, `find_library(VAR [NAMES] x …)`, `pkg_check_modules(PREFIX … module…)` |
| cargo | `Cargo.toml` (workspace members too) | `[dependencies]` / `[build-dependencies]` / `[target.'cfg(…)'.dependencies]`; git deps carry their URL; registry deps are fetched by cargo itself |
| autotools | `configure.ac` | `PKG_CHECK_MODULES([V], [mod >= ver …])`, `AC_CHECK_LIB`, `AC_SEARCH_LIBS` |
| make | `Makefile` | `pkg-config … <module>` invocations (incl. `$(shell …)`) |

Each declaration carries its kind, required/optional flag (meson
`required: false`, cmake non-`REQUIRED`, autoconf 4-arg
`PKG_CHECK_MODULES` soft form), version constraint (report-only — the
build system's own dependency check stays the version authority), and a
`file:line` origin. Build-system **built-ins** are skipped as
build-system semantics, never fetched: meson `threads`/`gtest`/…,
cmake `Threads`/`PkgConfig`/…, autotools libc pieces (`m`, `dl`,
`pthread`, `resolv`, …). Vendored meson subprojects (checked-in trees
under `subprojects/`) resolve in-tree — nothing is fetched for them.

### Conditional context (meson, via mesoneval.rs)

`dependency()` calls are only declarations of *some* build: meson's
`if`/`elif`/`else` blocks gate them on platform checks, build options
and compiler probes. A call inside `if host_machine.system() ==
'darwin'` is not a dependency of a Linux build at all — surfacing it
(as gitfull once did: a macOS framework was flagged for a Linux
ProtonPlus build through GLib's `if glib_have_cocoa` blocks) sends the
resolver hunting for a platform it is not building for.

`mesoneval.rs` evaluates the conditional structure of every
`meson.build` for the platform being scanned (gitfull builds natively,
so host/build/target machine all equal the machine running gitfull;
macOS is `darwin` in meson's naming). The rules are strictly
one-sided — a branch is excluded **only when its condition provably
evaluates to false** for the scan platform:

* understood: `host_machine.system()` / `build_machine` /
  `target_machine` `.system()/.cpu_family()/.endian()` comparisons,
  variables holding them (`host_system = host_machine.system()`),
  boolean/string/array literals, `and`/`or`/`not` (three-valued:
  `False and …` is false; anything undecidable is *possibly true*),
  `in`/`not in` and `.contains()` over arrays, ternaries
  (`cond ? a : b` — libxml2's `want_x` chains), `if`/`elif`/`else`/
  `endif` nesting, comments, multi-line `'''…'''` probe strings, and
  `\` line-continuations;
* **project options resolve against their declared defaults** —
  `meson.options`/`meson_options.txt` option definitions plus
  `project(default_options: […])` overlays are loaded per project, and
  `get_option('x')` evaluates to the option's *default* (the
  0-configuration build gitfull provisions): booleans to `true`/`false`,
  feature options to `'enabled'`/`'disabled'`/`'auto'` (with the
  `.enabled()/.disabled()/.auto()/.allowed()` predicates and
  `.disable_auto()/.enable_auto()/.require()` coercions meson defines
  on them), string/combo options to their default. A default-`false`
  option gate (`if get_option('docs')` … `subdir('docs/')`) is a
  provably-dead branch: the subtree is excluded. Builtin meson options
  (`prefix`, `warning_level`, …) and undeclared names stay unknown →
  reachable (the one-sided rule);
* `subdir()` runs in the caller's variable scope in call order (meson
  semantics) — the evaluator follows statically-known `subdir()`
  targets with one shared scope, which is how real projects gate
  subdir-file deps on root-computed platform flags (GLib's
  `glib_have_cocoa`, computed `false` on Linux, killing its
  `appleframeworks` calls two files away). Assignments on dead lines
  never take effect — dead-branch reassignments cannot leak
  (`host_system = 'darwin'` inside `if host_system == 'ios'` stays
  Linux on Linux, and applies on iOS, exactly like meson);
* **`foreach VAR : [literal list]` loops are unrolled per item** —
  backend/plugin dispatch (GTK's
  `foreach backend : […]
  if get_variable('@0@_enabled'.format(backend)) → subdir(backend)`)
  evaluates per item: live backends are entered, provably-disabled
  ones are not. `get_variable('name')` resolves against the live
  variable scope; `'@0@'.format(x)` performs the string interpolation
  the dispatch idiom composes names with;
* a `subdir()` inside a provably-dead branch records its whole
  **directory** as never-entered: the fallback scan (below) skips
  everything under it — an option-gated component
  (`if get_option('compose') subdir('compose/')` with a default-off
  option) or a dead foreach-dispatch item drops out of the scan
  entirely, instead of being resurrected by the fallback;
* everything statically undecidable — compiler probes, dynamic subdir
  paths, computed option values — counts as reachable (both sides of
  an undecided branch). Under-provisioning a real dependency breaks
  the build; over-including an undecided one costs a report line.
  Files not reachable through static `subdir()` calls (dynamic paths)
  are still scanned with an isolated scope.

The exclusion is provably **not name-based**: the test suite scans
the same fixtures as several platforms and asserts a gated dependency
vanishes when its platform does not match and reappears when it does
(the macOS framework dep comes back on a `darwin` scan of the very
fixture it is excluded from on Linux).

### Generality constraint (by design, enforced by tests)

Discovery is **generic across arbitrary repositories**. It is driven
exclusively by parsing each build system's own manifest format at
install time. `depgraph.rs` contains **no per-repo, per-project or
per-library name tables** — the only name lists in it are
build-system-semantic skip sets (built-ins, libc pieces), which describe
build-system semantics rather than any particular project. (Resolution —
"where does this module name come from" — is a different question from
discovery, answered by the curated upstream map + flagged search
fallback; see the resolution layers above.) This
constraint is deliberate and load-bearing: the implementation **must
generalize across repositories and must never be tuned to any specific
test case, fixture, or example project**. The test suite enforces it by
scanning several unrelated repository shapes with different dependency
sets through the *same* parsers (`discovery_is_generic_across_unrelated_
shapes`, plus per-build-system fixtures that share no names with each
other), and the end-to-end tests drive discovery through fixture repos
whose names are chosen to be obviously synthetic.

### Required vs optional — meson's own semantics, read structurally

A `dependency()` call carries meson's own optionality verdicts, and the
scanner reads them from the call site (never from names):

* **`required:` keyword** — `required: false` is best-effort (the
  project checks `.found()` and carries on); `required: true` or no
  `required:` (the default) is mandatory. The value is evaluated as a
  full meson expression: literals, `required: get_option('x')`
  resolved against the option's declared default, feature coercions
  (`get_option('x').disable_auto()`), and variables assigned just
  above the call (`gssapi_opt = get_option('gssapi')` → `required:
  gssapi_opt`, evaluated against the live-scope snapshot at the call
  site). A feature option resolving `'disabled'` **skips the lookup
  entirely** — the call is not a dependency of this configuration at
  all (same category as a platform-gated call in a dead branch);
  `'auto'` maps to best-effort; `'enabled'` to mandatory. Undecidable
  values stay mandatory (the one-sided rule).
* **option gates** — a call inside `if get_option('docs')` where the
  option defaults off is in a provably-dead branch: not surfaced at
  all (see conditional context above).
* **`meson.override_dependency('mod', dep)`** — the mirror image of a
  dependency declaration: the scanned tree's own build *provides* the
  module (GLib's tree provides `girepository-2.0`). The scanner
  records it as an in-tree provide, never as a requirement; a sibling
  `dependency('girepository-2.0')` call is satisfied by building the
  same tree. `declare_dependency(...)` and other
  `X.something_dependency(...)` method-call forms are never read as
  the global `dependency()` declaration, and `dependency(` inside
  string literals or `#` comments is string content, not a call.
* **autotools equivalents** — `PKG_CHECK_MODULES(VAR, mods, found,
  not-found)` and `AC_CHECK_LIB(lib, func, found, not-found)` with a
  non-empty not-found handler are the optional forms; m4 `dnl`
  comments inside multi-line argument lists are stripped; the classic
  libc socket fallbacks (`nsl`, `socket`, `inet`) and `iconv` resolve
  against the toolchain's C library.

The planner treats the two classes differently: **required**
dependencies run the full resolution layer stack including the flagged
search fallback (an unconfirmed candidate stops the install for a pin
or a TTY confirmation). **Optional** dependencies run the same *sound*
layers silently — user pin, wrap, vendored tree, in-tree provide,
shared cache, curated map — and a resolved optional dependency is
provisioned opportunistically ("use it if present" is exactly what the
manifest asked for); an unresolved one is logged
(`optional dependency <name> not available — continuing without it`)
and the install continues. The ranked-search fallback is never entered
for an optional dependency: its result would be unconfirmed, and an
optional dependency must never prompt, block, or auto-build an
unconfirmed match.

### Resolution layers (per declared dependency)

Every discovered dependency name is resolved through the same generic
layers, in order:

1. **cargo registry deps** — the cargo resolver fetches these into the
   app's sandbox at build time (isolation-compliant by design); gitfull
   reports them but does not provision them.
2. **user override** — `[dep.<name>]` in gitfull.conf: `skip = true`, or
   a pinned `source` (+ optional `ref`). This is *user configuration*
   (mirroring `[toolchain.sources]`), not a code-side name table.
3. **identity-level cache check** — a pinned source (override or meson
   wrap) whose exact identity (`url|ref` / path / tarball URL) was
   already built by an earlier install is reused outright: no clone, no
   rebuild.
4. **meson wraps** — the manifest's own pin files: `[wrap-git]` clones
   the pinned URL/revision, `[wrap-file]` downloads + extracts the
   pinned tarball (+ optional patch, falling back to the wrap's own
   `source_fallback_url` when the primary refuses — exactly what meson
   does). Wraps no `dependency()` call references are still honored
   (`meson subprojects download` policy — over-provide, never
   under-provide). A wrap matches a declared name by its stem, its
   `[provide]` module names (both wrap-db conventions:
   `dependency_names = a,b` and `a = a_dep` per line), or the
   dependency's own `fallback: ['<subproject>', …]` kwarg — meson's
   fallback semantics name the subproject explicitly.
5. **vendored subprojects and in-tree provides** — checked-in trees:
   satisfied in-tree; modules the scanned tree's own build declares via
   `meson.override_dependency` (GLib provides `girepository-2.0`):
   satisfied by building that same tree, nothing fetched.
6. **shared library cache** — `<root>/libs/` entries matched by the
   names the built library actually *provides* (`.pc` module stems,
   `*Config.cmake` packages, `lib*.a/.so` members).
7. **the curated upstream map** ([`libmap.rs`](#curated-upstream-map-
   libmaprs)) — well-known pkg-config module names → their correct
   upstream repository. This is the *primary* strategy for module names:
   a module→upstream mapping is knowledge, not something star-ranking
   can infer. Maintenance branches are pinned where the module name
   demands it (`sdl2` → the SDL repo's `SDL2` branch, `gtk+-3.0` →
   `gtk-3-24`). Module names mapping to the same parent repository
   (`glib-2.0`, `gio-unix-2.0`, `gobject-2.0` → GLib) deduplicate to a
   single fetch/build through the identity-based walk dedup.
8. **ranked forge search — flagged fallback only.** Used when a name is
   in no layer above. Its result is **never silently auto-built**:
   candidates are printed flagged `UNCONFIRMED`, and proceeding
   requires either an interactive confirmation (`y`) or a
   `[dep.<name>]` config pin (the sanctioned non-interactive path for
   scripts/CI/`--yes`-style runs — `--yes` deliberately does NOT
   bypass this gate). "Interactive" means a real terminal on **both
   ends of the prompt** — stdout (where the question is printed) and
   stdin (where the answer is read), the classic `isatty(0) &&
   isatty(1)` idiom. Anything else — piped/captured stdout, non-TTY
   stdin, the inherited-but-unserviced terminal a packaging pipeline
   or test runner leaves on fd 0 — is non-interactive and takes the
   **hard-error path immediately: no prompt, no blocking stdin read
   at all** (a blocking read there is an indefinite hang with the
   question swallowed by output capture). Rationale: star/contributor/commit ranking
   answers *"what's a popular repo matching this text"*, which has no
   reliable correspondence to *"what is the correct upstream source for
   this pkg-config module"* — module names frequently are modules
   inside a parent library's repository (`gio-unix-2.0` is a GLib
   module; no repo is named that, so search finds nothing or something
   unrelated), and generic names string-match unrelated projects.
   `--dry-run` reports such deps as UNRESOLVED and skips their subtree
   instead of failing.

A name that no layer can resolve (and that search cannot even find
candidates for) is a hard, actionable error naming the dependency and
the `[dep.<name>]` pin syntax. Bare-name **application** installs
(`gitfull install <app>`) keep auto-selecting the top-ranked repo —
there the user asked for "a popular repo matching this text", which is
exactly what ranked search answers.

### Anonymous generic remotes and auth rejections

A URL whose host matches no `[forge.<name>]` entry is fetched as an
**anonymous generic git remote**. The clone runs sealed — credential
helpers reset on the command line (`-c credential.helper=`), empty
sealed `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM`, redirected `HOME`,
`GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS` answering empty — and because
no forge owns the host, **no token is attached at all**. `env_clear()`
makes the child environment exactly that sealed set, so nothing
ambient (a `GITHUB_TOKEN` in the session, a host credential helper,
`GIT_CONFIG_*` injection vars) can reach the clone of an
unconfigured host. A test observes the real spawned child (argv +
environment, via a shim `git`) to pin this byte-for-byte.

When a server answers such a clone with an authentication rejection,
gitfull reports a dedicated `CloneAuth` error that names which of two
very different situations occurred:

* **anonymous** — "gitfull sent NO credentials": the *server* refused
  anonymous access to that repository (auth-gated, moved, or gone).
  This is not a credential leak: GitLab's canned `HTTP Basic: Access
  denied` page is served to requests that carried no credentials at
  all (the gitlab.freedesktop.org/appstream incident looked exactly
  like a leaked PAT and was not one). Remedies are printed: pin an
  anonymously-clonable upstream via `[dep.<name>]`, or register the
  host as a `[forge.<name>]` with a `token_env`.
* **tokened** — a configured forge's token was declined (value,
  expiry, or scope); check that forge's `token_env` variable.

## Curated upstream map (libmap.rs)

An **upstream source map** for well-known pkg-config module names —
the same kind of data as a distribution's package→source mapping or
gitfull's own `toolchain::CATALOG`. It maps module names to the
repositories their maintainers actually publish from:

* seeded with common windowing/graphics/core-library modules — the
  GLib family, the GTK family, cairo, pango, harfbuzz, freetype,
  fontconfig, pixman, libsoup, json-glib, libadwaita, libgee,
  libnotify, appstream, libarchive, libxml2, openssl, libcurl, sqlite,
  zlib, libpng, libjpeg-turbo, SDL (2 and 3, on their correct
  branches), wayland, libdrm, libinput, dbus, xkbcommon, and
  similarly-scoped others — **seeded, not exhaustive**;
* **extensible and overridable via configuration, not code**: a
  `[dep.<name>]` entry in gitfull.conf always wins over the table
  (that is also how names missing from the seed set are pinned — see
  docs/CONFIG.md);
* plain upstream clone URLs: hosts not registered as forges
  (gitlab.gnome.org, gitlab.freedesktop.org, …) are fetched as
  anonymous generic git remotes, so no forge configuration is needed
  to use the map. This imposes a hard **audit criterion on every
  entry: the source must be anonymously cloneable** — the generic
  path attaches no credentials at all. The seed table has been
  audited against the live hosts with sealed anonymous `git ls-remote`;
  two entries failed and were corrected (`appstream` — fd.o's GitLab
  serves its `HTTP Basic: Access denied` page to credential-less
  requests even though the web UI is public — and `libwebp`, whose
  `google/libwebp` repository no longer exists);
* **not tuned to any test case**: no test fixture name appears in it,
  and no code path special-cases any entry — the mechanisms
  (curated-first ordering, flagged fallback, same-source dedup) are
  name-agnostic and covered by tests over unrelated fixture repos.

This table is deliberately *not* the "no name tables" rule the
manifest-parsing layer follows (see the generality constraint below):
depgraph.rs still contains zero per-repo knowledge — it parses
whatever a repository declares. The curated map is the resolution
side's equivalent of the toolchain catalog: shared, reviewable,
config-overridable upstream data.

## Shared library cache (libcache.rs)

Library dependencies are built from source with the toolchain-managed
compiler and their install prefixes are moved into `<root>/libs/`:

```
<root>/libs/<slug>-<identity-hash>/
├── bin/ include/ lib/ …     whatever the library installed
└── meta.toml                name, provides, source, git_ref, commit,
                             requires (link closure), built_by, date
```

* **Identity addressing.** The cache key is derived from the resolved
  source identity (clone URL + ref / local path / tarball URL), so it is
  deterministic before the build runs. Lookup also works **by identity**
  (`find_by_identity`) so the same physical library requested under an
  alias name is reused, never rebuilt.
* **Provides matching.** Lookup by *name* matches the names the built
  tree actually provides, discovered from its own files — so
  `dependency('zlib')` reuses an entry registered as `zlib` no matter
  which repo built it.
* **Link closure.** Each entry records the entries it was built against
  (`requires`), expanded transitively (`closure_dirs`). When a later app
  reuses an entry by name, its closure joins the link line — a static
  `libfoo.a` whose objects reference `libbar` symbols links correctly
  without this install ever fetching `libbar`. This is what makes
  *reused* libraries as linkable as freshly built ones.
* **Sharing semantics.** Like toolchain components, entries are
  content-addressed and never mutated after registration; `gitfull
  remove` never deletes them (other apps may reference them).

Optional dependencies (per the manifest's own flags) are reported and
never provisioned. Missing toolchains are never obtained from a host
package manager (impossible — see the denylist); gitfull tells you the
exact from-source command instead.

### OPEN DESIGN DECISION — prebuilt binaries for foundational libraries

**Status: flagged for the project owner. No direction is assumed or
implemented here; gitfull today is strictly source-built, and this
section records the open question, not a policy.**

When a dependency resolves (via the curated map) to a foundational
library with a large transitive graph — GTK → pango, harfbuzz, cairo,
wayland, fontconfig, freetype, … — should gitfull keep building the
*entire* chain from source on every fresh root, or prefer a prebuilt
binary for such foundational libraries when one is safely obtainable,
falling back to the full source build only when necessary?

What is already true today, verified by tests and working at any
depth:

* the BFS walk handles arbitrarily deep transitive recursion and is
  cycle-safe (a back-edge onto an ancestor is a hard error; diamond
  edges deduplicate through the visited map);
* the shared library cache applies across the whole depth: every
  declared dependency at every level consults identity + `provides`
  before any fetch, so each unique source is built once per root,
  reused under every sibling name, with link closures carried along;
* curated entries are per-deployment overridable via `[dep.<name>]`.

The cost of source-only: a first install pulling GTK-scale trees
spends a long time compiling libraries whose binaries are effectively
identical for every consumer, and real-world manifests also contain
names outside the curated seed (bash-completion, qt6, xmlb, libfyaml,
…) that stop for a `[dep]` pin before anything builds.

If a prebuilt tier is wanted, a sketch consistent with gitfull's
principles — **for decision, not implemented**: opt-in per module
(`[dep.<name>] prebuilt = "…"` or a curated prebuilt map) with pinned
URLs + sha256, fetched with the same sealed/redacted treatment as
toolchain tarballs and installed only into the sandbox; the source
build remains the default and the fallback. The trust trade-off —
building vs. executing downloaded binaries — is exactly what the
project owner must rule on.

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
  **TTY-state independence**: every test ctx pins
  `interactive_override: Some(false)` (a simulated non-interactive
  session), so prompt gates deterministically take their hard-error
  path no matter which fds the test runner inherited — the suite
  cannot hang on a confirmation prompt inside `makepkg check()`/CI,
  where fd 0 is often still an inherited terminal nobody services;
  the unconfirmed-search gate e2e additionally runs under a hard
  60 s watchdog (worker thread + deadline), so a regression that
  reintroduces a blocking stdin read fails loudly and fast instead of
  hanging the packaging build;
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
