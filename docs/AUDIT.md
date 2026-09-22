# AUDIT.md — gitfull isolation & host-interaction guarantees

This document exists so that the two deliberately-exceptional paths in
gitfull's design are **easy to audit**, and so that every other host
interaction is enumerated and justified. It is written to be read
alongside the code; module and function references are given throughout.

---

## 1. The two special paths

### 1.1 The single host-touching path: the seed GCC build

**What:** building the initial GCC toolchain uses the host's system
compiler (`cc`/`gcc`/`g++` found on the host tool path) exactly **once**.
After the seed GCC exists, the host compiler is never invoked again —
not for dependency builds, not for the target app, not for later
toolchain components (python, meson, ninja, cmake, vala, rust).

**Where (code):**

| concern | location |
|---|---|
| plan generation | `src/toolchain.rs` → `ToolchainManager::seed_gcc_plan()` |
| execution | `src/bootstrap.rs` → `bootstrap_seed_gcc()` / `seed_gcc_resolved()` |
| exec classification | `src/gitproc.rs` → `ExecClass::SeedHostCompiler` |
| provenance record | `toolchains/gcc-<v>/meta.toml` (`built_by = "host-cc (seed)"`) |
| trigger | **automatic**: `gitfull install` provisions the seed itself when missing (`bootstrap::ensure_components`); manual override `sudo gitfull toolchain bootstrap-gcc --execute` |

**Sequence (exactly what `--execute` runs):**

1. **Resolve the version.** Order: `--version` flag →
   `toolchain.seed_gcc_version` (config pin) → `toolchain.preferences.gcc`
   → **latest**, auto-detected from the GCC git repo tags
   (`git ls-remote --tags … 'refs/tags/releases/gcc-*'`, sealed
   FetchTool). There is deliberately **no hardcoded default version** in
   the code.
2. **Fetch the source** (sealed): `git clone --branch releases/gcc-<v>
   --depth 1 https://gcc.gnu.org/git/gcc.git` → `<root>/cache/gcc-<v>`.
   Source overridable via `[toolchain.sources]` (e.g. a mirror).
3. **Prerequisites** (sealed FetchTool): `sh contrib/download_prerequisites`
   — GMP/MPFR/MPC tarballs land inside the source tree.
4. **Build — THE host-compiler window.** In
   `<root>/toolchains/.build/gcc-<v>`:
   `…/configure --disable-bootstrap --disable-nls --disable-multilib
   --enable-languages=c,c++ --prefix=<root>/toolchains/gcc-<v>` →
   `make -j` → `make install`.
   `--disable-bootstrap` is essential to the guarantee: GCC's default
   bootstrap would run *three* compiler stages; single-stage means the
   **host compiler compiles the seed exactly once**. Every command in
   this window is spawned under `ExecClass::SeedHostCompiler` — the only
   code path in the entire codebase permitted to use that class (the
   `doctor` host-cc probe is the only other use, and it is a read-only
   `--version` check classified the same way so the audit log stays
   honest about host-compiler touches).
5. **Record provenance:** `toolchains/gcc-<v>/meta.toml` — component,
   version, source URL, commit, `built_by`, date.
6. **From here on:** every build's `PATH` starts with
   `toolchains/*/bin`, so `CC`/`CXX`/`cc` resolve to the toolchain
   compiler. Host compiler cannot be picked up implicitly even by
   build scripts, because the exec chokepoint resolves programs against
   the sandbox PATH only (`gitproc::resolve_program`).

**What this path does NOT do:** it does not write anywhere outside
`<root>` (source in `cache/`, build in `toolchains/.build/`, install in
`toolchains/gcc-<v>/`); it does not invoke any package manager. It is a
state-changing operation, so on a normal deployment it runs as root like
all other mutating operations (§2.1) — root is required by the *target
paths*, not by the host-compiler use itself.

### 1.2 The single sandbox-escape path: the final binary copy

**What:** when a build produces final binaries, gitfull copies exactly
those binaries out of the sandbox into the configured bin directory
(`core.bin_dir`, default `/usr/local/bin`). This is the sole sanctioned
crossing point between a sandbox and the host filesystem.

**Where (code):** `src/planner.rs` → `install_binaries()` — the only
function in the codebase that writes outside `<root>`.

**Guarantees at the crossing:**

* only files detected as **final binaries** cross (executables from the
  DESTDIR staging area / `prefix/bin` / cargo `target/release`; libraries,
  headers, object files, hidden files are excluded — `is_binary_artifact`
  + `is_executable_file`);
* an explicit list (`[repo] bins = [...]` or repo `gitfull.toml`) may
  narrow it further, never widen it beyond produced executables;
* each copied file is set to mode `0755`, **hashed (SHA-256)**, and
  recorded in two places: the app's `meta.toml` (for verification and
  `gitfull remove`) and `<root>/audit.log`
  (`install-binary\t<name>\t<sha256>\t<dest>` events);
* overwriting an existing file at the destination is refused unless the
  user passes `--yes`;
* `gitfull remove` only deletes binaries whose current hash matches the
  recorded hash (modified files are kept and warned about, unless
  forced).

**What this path does NOT do:** no directories are created outside
`bin_dir` (and `bin_dir` itself only if missing); no symlinks, no
libraries, no config files, no environment mutation, no `ldconfig`, no
anything else.

---

## 2. Everything else gitfull executes (full inventory)

gitfull spawns subprocesses in exactly one place:
`src/gitproc.rs` (`run`, `run_stream_stderr`). Every spawn is
deny-list-checked, given a fully-specified environment, and audited.

### 2.0 The privilege model (who may run what)

Mutating operations — `install`, `update`, `remove`, and toolchain builds
with `--execute` — write `<root>` (default `/var/lib/gitfull`) and copy
binaries into a system-wide bin dir (default `/usr/local/bin`). Neither
is possible as a normal user, so **gitfull requires root for all
state-changing operations**: `sudo gitfull install …`, `sudo gitfull
remove …`, etc. The check (`src/privilege.rs`) runs at both the CLI
(fail-fast, before any network work) and the library entry points
(`planner::install/update/remove`, `bootstrap::ensure_components` /
`bootstrap_seed_gcc` / `build_component`), so it cannot be bypassed by
calling the API directly. Exit code 3, with the exact `sudo gitfull …`
line to re-run.

Read-only commands — `list`, `info`, `doctor`, `config`, `audit` — never
require root: they create nothing and only read state that already exists
(filesystem permissions remain the final arbiter of visibility).

One explicit carve-out: when **both** `core.root` **and** `core.bin_dir`
are overridden away from the system defaults, gitfull is deliberately
operating on user-writable paths (dev sandboxes, the test suite) and
mutating commands are allowed without root. Keeping either system path
keeps the requirement.

### 2.1 Program inventory

| program | class | when | writes confined to | notes |
|---|---|---|---|---|
| `cc`/`gcc`/`g++` (host) | `SeedHostCompiler` | seed GCC build window only | `<root>` | §1.1 |
| `git` | `FetchTool` | clones, `ls-remote` (version auto-detect), `rev-parse` | clone destination (sandbox or cache) | sealed env: `GIT_CONFIG_NOSYSTEM=1`, empty `GIT_CONFIG_GLOBAL`, redirected `HOME`, `GIT_TERMINAL_PROMPT=0`, `GIT_ASKPASS=true`, credential helpers disabled via `-c credential.helper=`; optional token embedded in the clone URL and **redacted from every log** — and attached only for hosts with a `[forge.<name>]` entry (generic-remote clones are strictly credential-free: no token, no ambient env, no helper); an auth-rejected clone surfaces as a dedicated `CloneAuth` error stating affirmatively whether any credential was involved |
| `curl` | `FetchTool` | tarball toolchain sources (rust), the rust stable-channel TOML | `--output` target in `<root>/cache` (or stdout) | `--fail --location --silent --show-error` |
| `curl` | `ForgeApi` | **read-only forge search/ranking queries** (`gitfull install <name>`, `info <name>`) | **nothing — network reads only** | `--include` GET, unauthenticated, no tokens ever attached; responses parsed by the hand-written JSON parser (`src/json.rs`), so no new crates |
| `sh` | `FetchTool` | `contrib/download_prerequisites` (GCC GMP/MPFR/MPC) | inside the GCC source tree | network fetch only |
| `tar` | `HostUtility` | unpacking toolchain tarballs | `<root>/toolchains` | `-xJf` |
| `make`, `ninja`, `meson`, `cmake`, `cargo`, `configure` scripts | `Toolchain` | builds | the build's sandbox | resolved via the sandbox `PATH` (toolchain bins first); env is fully hermetic (§3) |
| `cc`/`gcc` (toolchain) | `Toolchain` | all builds after the seed | the build's sandbox | from `toolchains/<comp>-<v>/bin` |
| POSIX utilities (`env`, coreutils, …) | `HostUtility` | incidental build-system needs | sandbox paths passed as arguments | available via `core.host_tool_path` (§4) |

**gitfull never executes:** any host package manager or privilege
escalator (§5); anything not on the child's hermetic PATH (there is no
fallback to the host PATH — `gitproc::resolve_program` searches only the
PATH the caller specified for that child).

**Network egress points, exhaustively:**

1. `git clone` / `git ls-remote` → the forge host of the package being
   installed, and the GCC/toolchain source hosts (all configurable);
2. `curl` (`ForgeApi`, read-only GETs) → the REST search endpoints of the
   **configured forges** (GitHub `/search/repositories`, GitLab
   `/projects`, Gitea/Forgejo `/repos/search`; base URLs derived from the
   forge kind or set explicitly via `api_base`, and the top GitHub
   candidates' `/contributors` and `/commits` pages for ranking data).
   No authentication is ever attached — public endpoints only;
3. `curl` (`FetchTool`) → toolchain tarball hosts (default:
   `static.rust-lang.org`, configurable) and the rust stable-channel
   manifest;
4. `contrib/download_prerequisites` → GNU mirror hosts (GCC prerequisite
   tarballs), only during seed bootstrap;
5. inside target-app builds, the app's own build system may fetch its
   own dependencies (e.g. `cargo` fetching crates). That happens inside
   the sandbox, is the target app's behavior, not gitfull's, and never
   lands outside the sandbox. gitfull itself never consumes those
   artifacts.

Proxy environment variables (`http_proxy`, `https_proxy`, `all_proxy`,
`no_proxy`, upper and lower case) are the one host setting passed through
to fetch tools — sealed clones still need to reach the network in
proxied environments. Everything else is scrubbed.

---

## 3. Child environment hermeticity

Every spawned child gets `env_clear()` + an exactly-specified
environment. Nothing from the host leaks in unless explicitly listed:

| variable | in child env | value |
|---|---|---|
| `PATH` | always, explicit | toolchain bins → dep prefixes → sandbox prefix → `core.host_tool_path` |
| `HOME` | always | sandbox `env/` (or git-home in cache) — never the user's home |
| `TMPDIR` | builds | sandbox `tmp/` |
| `LC_ALL`, `LANG` | always | `C` (reproducible tool output parsing) |
| `SHELL` | builds | `/bin/sh` |
| `DESTDIR`, `PKG_CONFIG_PATH`, `LD_LIBRARY_PATH`, `CPPFLAGS`, `LDFLAGS`, `CC`, `CXX`, `PYTHON` | builds | sandbox/toolchain paths only |
| `GIT_*` | git invocations | sealed configuration (§2) |
| `GITFULL_CANARY`, user env vars, `SSH_AUTH_SOCK`, credentials, … | **never** | scrubbed by `env_clear()` |

This is tested: `tests/policy.rs::child_environment_is_hermetic` plants a
canary variable in the parent environment and asserts a child cannot see
it; `program_resolution_uses_hermetic_path` asserts a program present on
the host PATH but absent from the sandbox PATH cannot be resolved.

---

## 4. "Never touch the host" — precise statement

gitfull's guarantee has two halves:

1. **No host filesystem writes** outside `<root>` and `core.bin_dir`
   (§1.2), plus `/etc/gitfull.conf` which is read-only input. Read-only
   commands (`list`, `info`, `config`, `doctor`, `audit`) create nothing;
   the state root is created only by mutating commands.
2. **No host state dependencies**: builds run with hermetic env (§3),
   tools resolve against the sandbox PATH only, git runs sealed, and the
   host compiler is reachable only through the seed bootstrap (§1.1).

Executing a *program* that lives on the host (git, curl, `/bin/sh`,
coreutils) is not a host-state modification — it is tool use, exactly
like the sanctioned host-compiler use in §1.1, and every such invocation
is classified and audit-logged. `core.host_tool_path` (default
`/usr/bin:/bin`) exists because build systems need POSIX utilities; to
reach full strictness, build a busybox toolchain and point
`host_tool_path` at it — no other code changes required.

---

## 5. Forbidden-program enforcement (no package managers, ever)

`src/gitproc.rs` maintains a denylist applied to **every** spawn,
regardless of caller, before any process is created:

* host package managers: `pacman`, `pacman-g2`, `apt`, `apt-get`,
  `aptitude`, `dpkg`, `dnf`, `dnf5`, `yum`, `microdnf`, `rpm`, `zypper`,
  `urpmi`, `xbps-install`, `xbps-query`, `xbps-remove`, `xbps-src`,
  `apk`, `emerge`, `nix`, `nix-env`, `nix-shell`, `nix-build`, `guix`,
  `flatpak`, `snap`, `brew`, `port`, `fink`, `opkg`, `swupd`, `tazpkg`,
  `kiss`, `cards`
* privilege escalators (builds must never escalate): `sudo`, `doas`,
  `pkexec`, `su`

Checks are by basename, so `/usr/bin/apt-get` is denied just like
`apt-get`. Users extend the list with `[policy]
extra_forbidden_programs = [...]`. Violations produce a loud
`POLICY VIOLATION` error (exit code 3) and are structurally impossible to
bypass because there is no other spawn path in the codebase.

All dependency resolution is therefore performed internally by gitfull:
toolchain needs are computed from the detected build system + config
constraints and satisfied from `<root>/toolchains/` — and when a needed
component is missing, `gitfull install` **auto-provisions it from
source** (`bootstrap::ensure_components`): the seed GCC first (the single
host touch, §1.1), then each remaining component built with
already-managed tools. The user never runs a manual bootstrap step on
the normal path; `gitfull toolchain …` subcommands remain as optional
overrides. Package needs are other forge repos cloned and built inside
the requesting app's sandbox (`src/resolver.rs`, `src/planner.rs`).

---

## 6. License posture

**gitfull's own binary** links:

* the Rust standard library (which links the platform libc — glibc on
  glibc systems, the one unavoidable base-layer dependency of any Rust
  program; a static-musl build removes even that);
* `serde` + `toml` (MIT OR Apache-2.0) — the only external crates, kept
  to exactly two by a test (`tests/cargo_deps.rs`).

No copyleft-licensed code, and no Red Hat-associated system software
(D-Bus, systemd, GTK, or otherwise) is linked into, or invoked as a
library by, gitfull's own binary. Note that *invoking* a program is not
*linking*: gitfull executes `git` (GPLv2) and the seed host compiler
(GPLv3-family) as tools, and the spec's own sanctioned seed-bootstrap
step presupposes exactly that. Nothing from those programs is
incorporated into gitfull.

**Target packages** gitfull builds for users may be copyleft, may need
GTK or other Red Hat-associated software — that is allowed and by
design: such code is fetched and built strictly inside that package's
own sandbox, and gitfull neither links nor embeds it. gitfull is an
orchestrator, so target license terms do not attach to it
(`[policy] allow_copyleft_targets = true` controls this, default true).

---

## 7. Audit log

`<root>/audit.log` is append-only and records:

* every process spawn: `<epoch>\texec\t<class>\t<program>\t<args
  (redacted)>\t<cwd>\t<status>` — classes are `seed-host-compiler`,
  `fetch-tool`, `forge-api`, `host-utility`, `toolchain`
* every sandbox-escape: `<epoch>\tinstall-binary\t<name>\t<sha256>\t<dest>`
* every removal: `<epoch>\tremove-binary\t<name>\t<sha256>\t<dest>`

Inspect with `gitfull audit [N]`. Secrets (clone tokens) are redacted
before logging. Combined with each app's `meta.toml`, the log answers:
*what ran, with what environment class, and which binaries crossed the
sandbox boundary, with what hashes* — the complete story an auditor
needs.
