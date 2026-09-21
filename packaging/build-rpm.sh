#!/usr/bin/env bash
# Build the RPM (gitfull-<ver>-<rel>.<dist>.<arch>.rpm) from HEAD.
#
# Points rpmbuild's _sourcedir at the archive from make-archive.sh and
# keeps every other build directory inside a temp stage (no ~/rpmbuild
# pollution). Requires rpmbuild (rpm-build) and cargo. The %check section
# runs the offline test suite; pass --nocheck below to skip it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n1)"

"$root/packaging/make-archive.sh" > /dev/null

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

rpmbuild -bb "$root/packaging/gitfull.spec" \
    --define "_topdir $stage" \
    --define "_sourcedir $root/dist"

cp "$stage"/RPMS/*/*.rpm "$root/dist/"
echo
echo "wrote:"
ls -l "$root/dist"/*.rpm
