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
  any forge, at any ref;
* the **host system is never touched** — every app is built inside its own
  sandbox folder, with shared toolchains that gitfull builds itself;
* the **host compiler is used exactly once** — to build the seed GCC — and
  never again (see [docs/AUDIT.md](docs/AUDIT.md));
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

$ gitfull doctor                   # environment readiness check
$ gitfull toolchain bootstrap-gcc --execute
#   ^ the ONLY step that uses the host compiler (once); everything after
#     this uses toolchains/gcc-<v>/bin/gcc
$ gitfull toolchain build python --execute
$ gitfull toolchain build meson --execute
$ gitfull toolchain build ninja --execute

$ gitfull install github:mesonbuild/meson   # live progress bar, sandboxed build
$ gitfull list
$ gitfull info mesonbuild/meson
$ gitfull remove mesonbuild/meson           # hash-verified removal
```

Spec forms accepted everywhere a package is named:

| form | meaning |
|---|---|
| `owner/repo` | default forge (github, unless configured) |
| `forge:owner/repo` | explicit forge by config name (`codeberg:org/app`) |
| `owner/repo@v1.2` | pinned branch / tag / commit |
| `https://forge.example/owner/repo.git` | full URL (matched against configured forges) |
| `/abs/path` or `./rel/path` | local source tree (no network) |

## The 30-second tour

```console
$ gitfull install https://gitlab.com/gnome/libfoo
gitfull: cloning https://gitlab.com/gnome/libfoo.git
Receiving  [████████████░░░░░░░░░░░░░░░░]  47%   1.8/3.9 MiB   3.4 MiB/s  ETA 00:36
gitfull: cloned https://gitlab.com/gnome/libfoo.git (a1b2c3…)
gitfull: detected build system: meson (auto-detected from repo files)
gitfull: toolchain requirements: gcc, python>=3.8, meson, ninja
gitfull: toolchain gcc-14.2.0 (shared: /var/lib/gitfull/toolchains/gcc-14.2.0)
gitfull: building gitlab-gnome-libfoo [meson]
gitfull: installed /usr/local/bin/libfoo-tool (sha256 9b8b6602e200, 1.2 MiB)
gitfull: install record: /var/lib/gitfull/apps/gitlab-gnome-libfoo/meta.toml
```

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

* **Single host-touching path**: the seed GCC build
  (`gitfull toolchain bootstrap-gcc --execute`) uses the host system
  compiler exactly once, with `--disable-bootstrap` (single-stage), and
  records its provenance. Every build after that runs through the
  toolchain-managed compiler.
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
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | module map, install pipeline, sandbox/toolchain layout, resolver design |

## Build & test

```console
$ cargo build --release
$ cargo test            # 49 tests: config, forge, progress parsing, policy
                        # enforcement, hermetic env, e2e sandboxed install
```

gitfull itself depends on exactly two crates — `serde` and `toml`
(MIT OR Apache-2.0) — for its config parsing. Nothing else. No copyleft
code, no Red Hat-associated system software is linked into the gitfull
binary (see docs/AUDIT.md).

## CLI reference

```
gitfull [options] <command> [args]

  install <spec>...          install packages
  update [name...]           re-clone + rebuild installed packages
  remove <name>...           remove (hash-verified)
  list                       list installed packages
  info <query>               install record / forge resolution
  toolchain list             catalog + installed versions
  toolchain bootstrap-gcc [--execute] [--version <v>]
  toolchain build <comp> [--execute]
  doctor                     environment checks
  config show|validate|path
  audit [N]                  tail the audit log

options: --config <path> --root <path> --dry-run --yes/-y --no-color
         --color auto|always|never --verbose/-v --version/-V --help/-h

exit codes: 0 ok · 2 usage · 3 policy violation · 4 build failure
            · 5 not installed
```

## Project status

v0.1.0 scaffold: the forge abstraction, config schema, sandbox manager,
toolchain manager (seed-GCC bootstrap plan + shared versioned installs),
dependency resolver, build-system auto-detection, clone progress UI, exec
chokepoint + audit log, and the install/remove/list/update pipeline are
implemented and tested (49 tests). The seed-GCC *execution* path targets a
full Linux machine and is intentionally not exercised in restricted
development environments — its plan, version resolution, and exec
classification are tested.

License: **proprietary** — all rights reserved (see `LICENSE`).
