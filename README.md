# gitfull

A forge-agnostic package manager that treats **any Git-forge-hosted
repository as an installable package** — GitHub, GitLab, Gitea, Codeberg,
or any self-hosted forge — and builds it inside an **isolated sandbox**
with gitfull-managed toolchains.

> gitfull is proprietary software (see `LICENSE`).

## Why

System package managers install whatever your distro happened to package,
in the version your distro happened to pick, onto your host filesystem.
gitfull takes the opposite position:

* the **source of truth is the Git forge** — you install `owner/repo`, from
  any forge, at any ref; a bare `name` searches every configured forge and
  auto-selects the best-ranked match (visible, never silent);
* the **host system is never touched** — every app is built inside its own
  sandbox folder, with shared toolchains that gitfull builds itself;
* **toolchains are fetched and built automatically** — `sudo gitfull
  install <name>` is the *only* command on the normal path; if a required
  toolchain component is missing, gitfull provisions it from source first
  (the host compiler is used exactly once, for the seed GCC — see
  [docs/AUDIT.md](docs/AUDIT.md));
* **root is required exactly where state changes** — install, update,
  remove, and toolchain builds write `/var/lib/gitfull` and copy binaries
  into a system bin dir, so they run as `sudo gitfull …`; `list`, `info`,
  `doctor`, `config`, and `audit` are read-only and unprivileged;
* **one file crosses the sandbox boundary** — the final binary, copied to
  your bin directory, hashed and audit-logged (see
  [docs/AUDIT.md](docs/AUDIT.md));
* **no OS package manager is ever invoked** — not `apt`, not `dnf`, not
  `pacman`, not `xbps`, none of them, enforced structurally at a single
  exec chokepoint in the code.

## Quickstart

```console
$ cargo build --release            # builds gitfull itself (a normal Rust build)
# optional but recommended: place the config
$ sudo cp config/gitfull.conf.example /etc/gitfull.conf
$ sudoedit /etc/gitfull.conf       # adjust root / bin_dir / forges

$ gitfull doctor                   # environment readiness check (no root)

$ sudo gitfull install meson       # ONE command does everything
#   1. ranked forge search: every configured forge is queried, candidates
#      are ranked by stars / contributors / commits / recency, and the
#      resolution is printed BEFORE anything is cloned or built;
#   2. missing toolchain components (gcc, python, meson, ninja, …) are
#      fetched and built automatically — the seed GCC is the only build
#      that ever touches the host compiler, and gitfull runs it for you;
#   3. the app is built in its sandbox and the final binary copied out.

$ gitfull list                     # read-only: no root needed
$ gitfull info meson               # read-only: record + ranked resolution
$ sudo gitfull remove meson        # hash-verified removal (state change)
```

Notes on the happy path:

* **No manual bootstrap step exists.** `gitfull toolchain bootstrap-gcc
  --execute` / `gitfull toolchain build <comp> --execute` still work, but
  they are optional manual overrides for advanced use — install
  auto-provisions whatever is missing.
* **State-changing commands need root** (`sudo`): `install`, `update`,
  `remove`, and `toolchain … --execute`. Read-only commands (`list`,
  `info`, `doctor`, `config`, `audit`) never do. Dev sandboxes that
  override **both** `core.root` and `core.bin_dir` away from the system
  defaults may skip sudo deliberately (see docs/AUDIT.md §2.0).

Spec forms accepted everywhere a package is named:

| form | meaning |
|---|---|
| `name` | **ranked search** across all configured forges; top match is
  auto-selected and printed before anything happens (§ below) |
| `forge:name` | ranked search scoped to that one forge |
| `owner/repo` | default forge (github, unless configured) — no search |
| `forge:owner/repo` | explicit forge by config name (`codeberg:org/app`) —
  bypasses ranking entirely |
| `owner/repo@v1.2` | pinned branch / tag / commit |
| `https://forge.example/owner/repo.git` | full URL (matched against configured forges) |
| `/abs/path` or `./rel/path` | local source tree (no network) |

## Ranked search: bare names resolve across forges

`sudo gitfull install meson` (no forge prefix, no `owner/repo`) queries the
search API of **every configured forge** that has one (GitHub, GitLab,
Gitea/Forgejo kinds; forges without an API are skipped with a visible
note), then ranks candidates by **stars (40%), contributors (25%), commit
count (25%), and recency of the last push (10%)** — log-normalized, with
absent signals re-normalized so terser forges are not structurally
punished. The full ranked table and the chosen `forge:owner/repo` are
printed *before anything is cloned or built*: the auto-selection is
always visible, never silent. Use `forge:owner/repo` to pin the exact
repository and skip ranking. Search queries are unauthenticated read-only
  GETs through the audited exec chokepoint — no tokens are attached.

## The 30-second tour

```console
$ sudo gitfull install meson
gitfull: searching all configured forges for `meson` (matching repositories
         are ranked by stars, contributors, commits, and recency; the top
         one is auto-selected)
gitfull: 3 candidate repository(ies) for `meson` across configured forges — ranked:
  #    repository                               forge        stars  contribs  commits  last push
  1    mesonbuild/meson                         github       5.9k      312      11k     3 days ago
  2    mesonbuild/meson                         gitlab       112         -        -     3 days ago
  3    star-lab/meson                           github        41         6       480     2 years ago
gitfull: resolved `meson` -> github:mesonbuild/meson (auto-selected: rank 1 of 3)
gitfull: cloning https://github.com/mesonbuild/meson.git
Receiving  [████████████░░░░░░░░░░░░░░░░]  47%   1.8/3.9 MiB   3.4 MiB/s  ETA 00:36
gitfull: cloned https://github.com/mesonbuild/meson.git (a1b2c3…)
gitfull: detected build system: meson (auto-detected from repo files)
gitfull: toolchain requirements: gcc, python>=3.8, meson, ninja
gitfull: auto-provisioning 4 missing toolchain component(s) — the seed GCC
         build uses the host compiler once; everything after uses it never
         (detailed logs: /var/lib/gitfull/logs/)
gitfull: toolchain gcc-14.2.0 (shared: /var/lib/gitfull/toolchains/gcc-14.2.0)
gitfull: toolchain python-3.13 (shared: /var/lib/gitfull/toolchains/python-3.13)
…
gitfull: building github-mesonbuild-meson [meson]
gitfull: installed /usr/local/bin/meson (sha256 9b8b6602e200, 1.2 MiB)
gitfull: install record: /var/lib/gitfull/apps/github-mesonbuild-meson/meta.toml
```

(The first install takes a while: it builds the toolchain components from
source, once, shared with every later install.)

## Installation

Build from source:

```console
$ cargo build --release
$ sudo install -m755 target/release/gitfull /usr/local/bin/gitfull
$ sudo cp config/gitfull.conf.example /etc/gitfull.conf   # optional; see docs/CONFIG.md
```

Or as a distro-native package — recipes in `packaging/` (PKGBUILD,
debian/, gitfull.spec, xbps-src template), built by
`packaging/build-{arch,deb,rpm}.sh`, documented in
**[docs/PACKAGING.md](docs/PACKAGING.md)**:

```console
$ sudo pacman -U gitfull-0.1.0-1-x86_64.pkg.tar.zst    # Arch
$ sudo apt install ./gitfull_0.1.0_amd64.deb            # Debian/Ubuntu
$ sudo dnf install ./gitfull-0.1.0-1.fc42.x86_64.rpm    # Fedora/RHEL
$ sudo xbps-install -R . gitfull                        # Void
```

Packages install `/usr/bin/gitfull` (distro-owned tree —
`/usr/local/bin` stays gitfull's own territory for the apps it
installs), `/etc/gitfull.conf.example` (never your
`/etc/gitfull.conf`, with each format's no-clobber semantics), the man
page, and the docs. They declare exactly `git` + `curl` as runtime
dependencies and contain no install-time scripts: `/var/lib/gitfull`
is created by gitfull itself on the first state-changing run.

## Isolation model (short version)

```
                         THE host filesystem
   ┌───────────────────────────────────────────────────────────────┐
   │ /etc/gitfull.conf          (read-only input)                  │
   │ /usr/local/bin/<binary>    ← the ONE sandbox-escape path      │
   │                                                               │
   │ /var/lib/gitfull/            everything gitfull writes        │
   │ ├── apps/<forge>-<owner>-<repo>/     per-app sandbox          │
   │ │   ├── src/ build/ stage/ prefix/ deps/ env/ tmp/ logs/      │
   │ │   └── meta.toml             install record (provenance)     │
   │ ├── toolchains/<comp>-<version>/     SHARED toolchains        │
   │ ├── cache/                                                   │
   │ └── audit.log                every exec + every binary copy   │
   └───────────────────────────────────────────────────────────────┘

   host cc ──(exactly once)──▶ seed GCC ──▶ all later builds
```

* **Single host-touching path**: the seed GCC build uses the host system
  compiler exactly once, with `--disable-bootstrap` (single-stage), and
  records its provenance — gitfull runs it **automatically** during
  `install` when the gcc toolchain is missing. Every build after that
  runs through the toolchain-managed compiler. (Manual override:
  `sudo gitfull toolchain bootstrap-gcc --execute`.)
* **Single sandbox-escape path**: `planner::install_binaries()` copies
  final binaries (mode 0755, SHA-256 recorded) to the configured bin dir.
  Nothing else writes outside `<root>`.

The full, auditable write-up — including every external program gitfull
executes, the exec-class system, the forbidden-program denylist, and the
environment hermeticity rules — is **[docs/AUDIT.md](docs/AUDIT.md)**.

## Configuration

`/etc/gitfull.conf` — custom TOML schema. Forges are **data**: adding a
forge is a config entry, not a code change:

```toml
[forge.mirror]
kind = "generic"                       # or github/gitlab/gitea/forgejo/cgit
host = "git.example.com"
clone_template = "https://{host}/src/{owner}/{repo}.git"

[repo."acme/secret-tool"]
forge = "mirror"                       # per-repo forge override
ref = "v1.2.0"                         # pin a ref
toolchains = ["gcc>=13"]               # extra toolchain constraints
packages = ["acme/libfoundation"]      # package deps (forge repos)
```

Full reference: **[docs/CONFIG.md](docs/CONFIG.md)** · annotated example:
`config/gitfull.conf.example`.

## Documentation

| doc | contents |
|---|---|
| [docs/AUDIT.md](docs/AUDIT.md) | the two special paths (seed GCC host-touch, final-binary sandbox-escape), exec classes, denylist, env hermeticity, license posture |
| [docs/CONFIG.md](docs/CONFIG.md) | complete `/etc/gitfull.conf` schema reference + forge extensibility guide |
| [docs/PACKAGING.md](docs/PACKAGING.md) | distro-native packaging recipes (Arch PKGBUILD, Debian, RPM, Void) + release-cutting checklist |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | module map, install pipeline, sandbox/toolchain layout, resolver design |

## Build & test

```console
$ cargo build --release
$ cargo test            # 102 tests: config, forge, spec/search, progress
                        # parsing, privilege model, ranked search (fake
                        # forge API), policy enforcement, hermetic env,
                        # e2e sandboxed install, packaging recipes
```

gitfull itself depends on exactly two crates — `serde` and `toml`
(MIT OR Apache-2.0) — for its config parsing. Nothing else. No copyleft
code, no Red Hat-associated system software is linked into the gitfull
binary (see docs/AUDIT.md).

## CLI reference

```
gitfull [options] <command> [args]

state-changing (run as root, e.g. `sudo gitfull install ...`):
  install <spec>...          name (ranked forge search) | owner/repo |
                              forge:owner/repo | o/r@ref | URL | local path.
                              Missing toolchains are fetched + built automatically.
  update [name...]           re-clone + rebuild installed packages
  remove <name>...           remove (hash-verified)
  toolchain bootstrap-gcc [--execute] [--version <v>]
                              manual seed-GCC build (optional override —
                              install does this automatically when needed)
  toolchain build <comp> [--execute] [--version <v>]
                              manual component build (optional override)

read-only (no root needed):
  list                       list installed packages
  info <query>               install record / ranked forge resolution
  toolchain list             catalog + installed versions
  doctor                     environment checks
  config show|validate|path
  audit [N]                  tail the audit log

options: --config <path> --root <path> --dry-run --yes/-y --no-color
         --color auto|always|never --verbose/-v --version/-V --help/-h

exit codes: 0 ok · 2 usage · 3 policy/privilege violation · 4 build failure
            · 5 not installed
```

## Project status

v0.1.0 scaffold: the forge abstraction, config schema, sandbox manager,
toolchain manager (seed-GCC bootstrap plan + shared versioned installs,
**auto-provisioned during install**), dependency resolver, build-system
auto-detection, **ranked multi-forge search for bare package names**,
**root-privilege enforcement for all state-changing operations**, clone
progress UI, exec chokepoint + audit log, and the install/remove/list/
update pipeline are implemented and tested (102 tests, incl.
distro-packaging recipe consistency). The seed-GCC
*execution* path targets a full Linux machine and is intentionally not
exercised in restricted development environments — its plan, version
resolution, and exec classification are tested.

License: **proprietary** — all rights reserved (see `LICENSE`).
