#!/usr/bin/env bash
# Build the Debian package (gitfull_<ver>_<arch>.deb) from HEAD.
#
# dpkg-buildpackage expects ./debian at the source root, so this script
# extracts the git-archive source tree into a temp stage and layers
# packaging/debian on top, then produces an unsigned binary-only .deb
# (-us -uc -b). Requires dpkg-buildpackage, debhelper 13, and cargo.
#
# Building as a normal user is fine: dpkg-buildpackage >= 1.19.3 passes
# --root-owner-group to dpkg-deb automatically, so package files are
# root-owned in the .deb even when built unprivileged.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"

"$root/packaging/make-archive.sh" > /dev/null
archive="$root/dist/gitfull-${version}.tar.gz"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
tar -xzf "$archive" -C "$stage"
cp -a "$root/packaging/debian" "$stage/gitfull-${version}/debian"

cd "$stage/gitfull-${version}"
dpkg-buildpackage -us -uc -b

cp "$stage"/gitfull_*.deb "$root/dist/"
echo
echo "wrote:"
ls -l "$root/dist"/gitfull_*.deb
