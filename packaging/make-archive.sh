#!/usr/bin/env bash
# Deterministic source archive for gitfull distro package builds.
#
# Uses `git archive` from HEAD: tracked files only, fixed ordering, and
# commit-time mtimes — the same commit always produces the same tarball
# (and therefore the same sha256), which the release checksums rely on.
# The gitfull-<version>/ prefix is what PKGBUILD, the RPM spec and the
# xbps-src template expect.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"
if [ -z "$version" ]; then
    echo "make-archive: could not read version from Cargo.toml" >&2
    exit 1
fi

out="$root/dist"
mkdir -p "$out"
archive="$out/gitfull-${version}.tar.gz"

git -C "$root" archive --prefix="gitfull-${version}/" --format=tar.gz \
    -o "$archive" HEAD

echo "wrote $archive"
sha256sum "$archive"
