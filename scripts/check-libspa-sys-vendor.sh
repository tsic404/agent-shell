#!/usr/bin/env bash
# Verify vendor/libspa-sys stays in sync with the upstream crates.io libspa-sys
# release it pins. Only the files patched for UOS 20 compatibility may differ;
# every other vendored file must be byte-identical to upstream. Registry-only
# artifacts (Cargo.lock, Cargo.toml.orig, .cargo_vcs_info.json) are not
# vendored and are skipped.
#
# Requires: curl, tar, cmp (diffutils).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor="$repo_root/vendor/libspa-sys"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$vendor/Cargo.toml" | head -1)"
[ -n "$version" ] || { echo "ERROR: cannot read vendored version from Cargo.toml" >&2; exit 1; }

# Files intentionally patched for UOS 20 compatibility (may differ upstream):
patched=" src/type-info.c src/type_info.rs README.md "
# Registry-only artifacts intentionally not vendored:
omitted=" Cargo.lock Cargo.toml.orig .cargo_vcs_info.json "

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL --retry 3 --retry-delay 2 \
  "https://static.crates.io/crates/libspa-sys/libspa-sys-$version.crate" \
  -o "$tmp/libspa-sys-$version.crate"
tar xzf "$tmp/libspa-sys-$version.crate" -C "$tmp"
upstream="$tmp/libspa-sys-$version"

fail=0
while IFS= read -r -d '' f; do
  rel="${f#"$upstream"/}"
  case "$omitted" in *" $rel "*) continue ;; esac
  if [ -f "$vendor/$rel" ]; then
    if cmp -s "$f" "$vendor/$rel"; then
      : # identical
    elif case "$patched" in *" $rel "*) true ;; *) false ;; esac; then
      echo "ok (patched): $rel"
    else
      echo "ERROR: vendored file differs from upstream: $rel" >&2
      fail=1
    fi
  else
    echo "ERROR: upstream file not vendored: $rel" >&2
    fail=1
  fi
done < <(find "$upstream" -type f -print0)

[ "$fail" -eq 0 ] && echo "OK: vendor/libspa-sys in sync with upstream libspa-sys $version"
exit "$fail"
