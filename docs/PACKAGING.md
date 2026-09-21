# PACKAGING.md — distributing gitfull itself in distro-native formats

This document is about **how a user gets gitfull onto their machine in
the first place** — nothing else. It does not change, weaken, or touch
gitfull's *runtime* behavior in any way:

* the exec-chokepoint denylist still forbids gitfull's own binary from
  ever invoking `pacman` / `dnf` / `apt` / `xbps` **when gitfull installs
  or builds other applications** (see docs/AUDIT.md §2.1). Packaging
  flips the direction of that relationship exactly once, at distribution
  time: the *user* invokes their system package manager to install
  gitfull itself. The two rules govern different binaries and different
  moments, and both hold simultaneously.
* the gitfull binary still links only `serde` + `toml`, still execs only
  `git` and `curl` as host tools, and still runs its sandboxed
  install/remove/toolchain pipeline exactly as documented in
  ARCHITECTURE.md.
* every tool mentioned here (makepkg, debhelper, rpmbuild, xbps-src,
  cargo itself) is **build-time/dev tooling on the maintainer's
  machine** — none of it is a runtime dependency of gitfull, and none of
  it is present on the user's machine merely because they installed the
  package.

Scope: current mainstream distros — Arch, Debian/Ubuntu, Fedora/RHEL,
Void. Making gitfull its own distribution or bootstrapping a from-scratch
system is explicitly out of scope.

## 1. What the packages ship

Every format installs the same payload:

| file | path | notes |
|---|---|---|
| gitfull binary | `/usr/bin/gitfull` | mode 0755, from `cargo build --release --locked` |
| example config | `/etc/gitfull.conf.example` | **never** the real `/etc/gitfull.conf` — see §2 |
| man page | `/usr/share/man/man1/gitfull.1(.gz)` | hand-written groff (`docs/gitfull.1`) |
| documentation | `/usr/share/doc/gitfull/` | README + ARCHITECTURE/AUDIT/CONFIG/PACKAGING |
| license | `/usr/share/licenses/gitfull/LICENSE` (Arch, Void) or `/usr/share/doc/gitfull/LICENSE` (deb, rpm) | proprietary, all rights reserved |

Two deliberate path decisions:

* **`/usr/bin`, not `/usr/local/bin`.** FHS assigns `/usr/bin` to the
  distro package manager's territory and `/usr/local/bin` to the local
  administrator. gitfull's *own* default `bin_dir` (where it installs the
  apps it builds) is `/usr/local/bin` — keeping the distro package out
  of that tree means the two file populations can never collide or
  accidentally overwrite each other, and it is always obvious which tool
  owns which binary: distro packages live in `/usr/bin`, gitfull-managed
  app binaries live in `/usr/local/bin`.
* **No `/var/lib/gitfull` at package-install time.** None of the recipes
  create directories, users, groups, or units, and none of them contain
  maintainer scripts or RPM scriptlets — the install step only places
  the five payload rows above. gitfull creates its whole state tree
  (`apps/`, `toolchains/`, `cache/`, `logs/`, `audit.log`) on the first
  state-changing run via `Config::ensure_root()` (`mkdir -p`, as root via
  sudo). This keeps packaging minimal and inspectable, and avoids
  questions like "what uid should the state dir have" at install time —
  root creates it, exactly as it will be used. `gitfull doctor` will
  tell a user whether the environment is ready before that first run.

Runtime dependency declarations are exactly `git` and `curl` — the two
programs gitfull execs as host tools. Everything else gitfull builds for
itself as managed toolchains; the binary itself has no dynamic
dependencies beyond glibc (and static musl targets would work too).

## 2. Upgrade safety: the conf files

The user's real configuration lives at `/etc/gitfull.conf`. **No package
ever owns, creates, modifies, or removes that file** — so a package
upgrade can never clobber or prompt about the user's actual
configuration. What ships is only `/etc/gitfull.conf.example`, and each
format's native mechanism protects local edits of *that* file too:

| format | mechanism | upgrade behavior for the example |
|---|---|---|
| Arch (pacman) | `backup=('etc/gitfull.conf.example')` | modified → new file lands as `.pacnew`, old kept; removal saves `.pacsave` |
| Debian (dpkg) | `conffiles` entry | unmodified → silently replaced; modified → dpkg's keep/replace prompt |
| Fedora/RHEL (rpm) | `%config(noreplace)` | modified → new file lands as `.rpmnew`, old kept |
| Void (xbps) | `conf_files` | modified → kept, new file lands as `.new` (`.gitfull.conf.example` sibling) |

A first install is a plain file copy in every format — no prompts.

## 3. Build prerequisites (maintainer machine, build-time only)

| distro | needs | driver |
|---|---|---|
| Arch | `base-devel` (makepkg), `cargo` (distro or rustup) | `./packaging/build-arch.sh` |
| Debian/Ubuntu | `build-essential`, `debhelper` ≥ 13 (dh), `dpkg-dev`, `cargo` | `./packaging/build-deb.sh` |
| Fedora/RHEL | `rpm-build`, `cargo` (Fedora package, or RHEL rust-toolset, or rustup) | `./packaging/build-rpm.sh` |
| Void | a void-packages checkout (xbps-src provides the cargo build style in the chroot) | manual, §8 |

Cargo notes for all formats: the MSRV is declared in `Cargo.toml`
(`rust-version`); a distro-packaged cargo older than that will fail —
use rustup's cargo in that case (`~/.cargo/bin` on PATH is enough; none
of the drivers sanitize PATH). `--locked` is used everywhere because
`Cargo.lock` is committed, so the build is reproducible against the
lockfile. The crate fetches `serde`/`toml` from crates.io at build time;
for fully-offline builds vendor first (`cargo vendor`) — not set up by
default to keep the recipes inspectable.

## 4. The source archive

All four formats consume the same source tarball, produced by:

```console
$ ./packaging/make-archive.sh
wrote /path/to/gitfull/dist/gitfull-0.1.0.tar.gz
6dc4b0…  dist/gitfull-0.1.0.tar.gz
```

`make-archive.sh` runs `git archive` against `HEAD` with the
`gitfull-<version>/` prefix (the prefix every recipe expects). That
gives the archive three properties the release process relies on:

* **tracked files only** — no `target/`, no `dist/`, no local debris;
* **determinism** — fixed member ordering and commit timestamps, so the
  same commit always hashes to the same sha256 (this is what the
  PKGBUILD checksum and the xbps `checksum=` record);
* **no VCS metadata** — the archive is just a clean source tree.

The man page is pre-compressed with `gzip -9n` in the Arch and RPM
recipes (no timestamp in the gzip header → reproducible); deb leaves it
to `dh_compress`, and Void's `vinstall` ships it uncompressed.

## 5. Arch Linux

Recipe: `packaging/PKGBUILD`. Quick path (run as a normal user; makepkg
refuses root):

```console
$ ./packaging/build-arch.sh
…
$ ls dist/
gitfull-0.1.0-1-x86_64.pkg.tar.zst   gitfull-0.1.0.tar.gz
```

The driver stages a copy of the PKGBUILD with the real sha256 injected
next to the tarball and runs `makepkg -f` in a temp directory — your
working tree is never touched. Manual path: copy `packaging/PKGBUILD`
and the archive into one directory, fix `sha256sums=()`, `makepkg -f`.

`check()` runs the offline test suite when makepkg is invoked with
`--check` (skip with `--nocheck`). The suite is safe in exactly this
environment: every test simulates a non-interactive session explicitly
(`interactive_override`), so confirmation gates deterministically take
their hard-error path instead of reading inherited stdin — an
inherited-but-unserviced terminal on fd 0 (makepkg from a console, a
build coordinator's pty) cannot hang the build — and the
unconfirmed-search gate test runs under a hard 60 s watchdog that
turns any prompt-read regression into a loud, fast failure. Verified
with `cargo test --release --locked --quiet </dev/null` and under an
alloc PTY that is never written to. Install and verify:

```console
$ sudo pacman -U dist/gitfull-0.1.0-1-x86_64.pkg.tar.zst
$ gitfull --version && man gitfull
```

For publishing (e.g. AUR or your own repo), switch `source=` to the
release URL and commit the real checksum — see §9.

## 6. Debian / Ubuntu

Recipe: `packaging/debian/` (control, rules, changelog, copyright,
conffiles, gitfull.docs, source/format). `dpkg-buildpackage` expects
`./debian` at the source root, so the driver extracts the archive into a
temp stage, layers `packaging/debian` on top, and builds an unsigned
binary-only package:

```console
$ ./packaging/build-deb.sh
…
$ ls dist/
gitfull_0.1.0_amd64.deb   gitfull-0.1.0.tar.gz
```

Install with dependency resolution (plain `dpkg -i` does not resolve
`git`/`curl` if they are missing):

```console
$ sudo apt install ./dist/gitfull_0.1.0_amd64.deb
$ gitfull --version && man gitfull
```

Notes:

* **unprivileged builds are fine**: `dpkg-buildpackage` ≥ 1.19.3 passes
  `--root-owner-group` to `dpkg-deb` automatically when building as a
  non-root user, so package files still land root-owned in the `.deb`.
* the package is `3.0 (native)` source format with the version carried
  in `debian/changelog`; there are no orig tarballs to wrangle.
* **MSRV**: Debian stable has carried rustc versions older than gitfull's
  MSRV in the past (e.g. bookworm's 1.63). If the distro cargo is too
  old, build with rustup's cargo on PATH — `debian/rules` just calls
  `cargo`.
* signing for local distribution: `debsign` or
  `dpkg-sig` on the result; `build-deb.sh` deliberately passes `-us -uc`
  (unsigned) because the key situation is maintainer-specific.
* `override_dh_auto_test` runs the offline test suite during build.

## 7. Fedora / RHEL

Recipe: `packaging/gitfull.spec`. Quick path:

```console
$ ./packaging/build-rpm.sh
…
$ ls dist/
gitfull-0.1.0-1.fc42.x86_64.rpm   gitfull-0.1.0.tar.gz
```

The driver points rpmbuild's `_sourcedir` at `dist/` and confines all
other build directories to a temp stage — nothing lands in `~/rpmbuild`.
Install:

```console
$ sudo dnf install dist/gitfull-0.1.0-1.fc42.x86_64.rpm
$ gitfull --version && man gitfull
```

Notes:

* `%check` runs the offline test suite during the build; skip with
  `rpmbuild --nocheck`.
* Fedora ships `cargo` in its repos; RHEL provides Rust via
  rust-toolset (`dnf module install rust-toolset`), and rustup's cargo
  works too — in the latter two cases drop the `BuildRequires: cargo`
  line or build outside a sterile mock.
* `%config(noreplace)` on the example conf: user-modified copies survive
  upgrades (updates arrive as `.rpmnew`).
* signing: `rpm --addsign dist/*.rpm` with your GPG key for local
  distribution.

## 8. Void Linux

Recipe: `packaging/void/srcpkgs/gitfull/template` (xbps-src uses the
cargo build style, which runs `cargo build --release --locked` in the
chroot and installs the binary to `/usr/bin`). Void builds inside a
void-packages checkout rather than from a driver script:

```console
$ git clone https://github.com/void-linux/void-packages
$ mkdir -p void-packages/srcpkgs/gitfull
$ cp packaging/void/srcpkgs/gitfull/template void-packages/srcpkgs/gitfull/
$ cd void-packages
$ ./xbps-src pkg gitfull        # fill checksum= first — see below
$ xbps-install --repository masterdir-x86_64/hostdir/binpkgs gitfull
```

Before building, `checksum=` must hold the sha256 of the release archive
— either run `xgensum gitfull` (fetches the distfile, computes, and
rewrites the template) or paste the checksum printed by
`packaging/make-archive.sh` for the tag. To test against a **local**
archive instead of the network URL, drop the tarball into your
void-packages masterdir's sources cache (xbps-src checks there before
downloading) and make the checksum match. For install-from-directory on
the target machine:

```console
$ sudo xbps-install -R . gitfull
```

## 9. Cutting a new release — maintainer checklist

1. **Bump the version everywhere.** The version lives in exactly six
   places, and `cargo test` (tests/packaging.rs) fails until they agree:
   `Cargo.toml` `version`, `packaging/PKGBUILD` `pkgver` (reset `pkgrel`
   to 1), `packaging/debian/changelog` (new entry on top, native version,
   no Debian revision), `packaging/gitfull.spec` `Version` (reset
   `Release` to `1%{?dist}`), `packaging/void/.../template` `version`
   (reset `revision=1`), and the `.TH` line of `docs/gitfull.1`
   (date + version). If the test count changed, also update the counts
   quoted in README.md.
2. **Green suite, clean tree.** `cargo test` (all of it — the packaging
   tests are the release checklist), `git status` clean.
3. **Tag.** `git tag -s v0.1.0` (annotated/signed) and push the tag.
4. **Archive + checksums.** `./packaging/make-archive.sh` →
   `dist/gitfull-<v>.tar.gz`; record its sha256 into the PKGBUILD
   (`sha256sums=(…)`) and the void template (`checksum=`) — these are
   the two recipes that verify integrity. Upload the tarball as a GitHub
   release asset so both URLs (release asset / tag auto-archive) are
   live; the release-asset URL is what the void `distfiles=` points at.
5. **Build every format you intend to ship** (§5–§8) into `dist/`.
6. **Smoke-test each artifact** in a throwaway VM/container of the
   matching distro: install the package, `gitfull --version`,
   `man gitfull`, `gitfull doctor`, then one full cycle
   `sudo gitfull install /abs/local/fixture` → `gitfull list` →
   `sudo gitfull remove <name>` (a local-path install exercises the
   pipeline with zero network), and confirm an upgrade in place
   (`pacman -U` / `apt install ./` / `dnf install ./` /
   `xbps-install -R .`) preserves a modified `/etc/gitfull.conf.example`
   and an existing `/etc/gitfull.conf`.
7. **Publish.** Push the commit + tag; attach `dist/` artifacts to the
   release. Signed packages (`debsign`, `rpm --addsign`, AUR/repo
   tooling) are maintainer-specific and intentionally not scripted here.

## 10. Non-goals

* **Official distro repos.** gitfull is proprietary (all rights
  reserved); these recipes are for self-distribution — your own repo,
  release assets, or direct `.pkg.tar.zst`/`.deb`/`.rpm` hand-off. They
  intentionally do not chase each distro's full policy stack
  (Fedora Rust guidelines, Debian NEW queue, lintian pedantry beyond
  what the recipes already satisfy).
* **gitfull as its own distro / bootstrap-from-scratch** — explicitly
  out of scope.
* **Install-time behavior**: no maintainer scripts, no scriptlets, no
  users/groups, no `/var/lib` creation, no systemd units (gitfull is a
  CLI tool). The package places five payload paths; everything else is
  gitfull's own first-run behavior.
* **Cross-compilation targets** (e.g. musl static binaries): the recipes
  build natively per distro; a musl variant would be a Cargo target
  change, not a packaging change.
