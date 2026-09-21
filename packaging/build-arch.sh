#!/usr/bin/env bash
# Build the Arch package (gitfull-<ver>-<rel>-<arch>.pkg.tar.zst) from HEAD.
#
# Stages a copy of packaging/PKGBUILD with the real source checksum
# injected next to the source tarball, then runs makepkg there. Run as a
# normal user (makepkg refuses root); requires makepkg (pacman) and cargo.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"

"$root/packaging/make-archive.sh" > /dev/null
archive="$root/dist/gitfull-${version}.tar.gz"
sum="$(sha256sum "$archive" | cut -d' ' -f1)"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$archive" "$stage/"
sed "s/^sha256sums=('SKIP')/sha256sums=('${sum}')/" \
    "$root/packaging/PKGBUILD" > "$stage/PKGBUILD"

cd "$stage"
makepkg -f

cp gitfull-*.pkg.tar.* "$root/dist/"
echo
echo "wrote:"
ls -l "$root/dist"/gitfull-*.pkg.tar.*
