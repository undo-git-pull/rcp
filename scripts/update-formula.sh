#!/bin/sh
# Write Formula/rcp.rb for a published release into a local homebrew-tap checkout.
# It changes no git state: review the diff, then commit and push yourself.
#
#   scripts/update-formula.sh v0.1.0 ../homebrew-tap
#
# Environment:
#   RCP_REPO      GitHub repo the release lives in (default: undo-git-pull/rcp)
#   RCP_BASE_URL  download folder override, e.g. file:///path/to/dist for testing
set -eu

usage="usage: $0 <tag, e.g. v0.1.0> <path to homebrew-tap checkout>"
tag="${1:?$usage}"
tap="${2:?$usage}"
repo="${RCP_REPO:-undo-git-pull/rcp}"
version="${tag#v}"
root="$(cd "$(dirname "$0")/.." && pwd)"
base="${RCP_BASE_URL:-https://github.com/$repo/releases/download/$tag}"

[ -d "$tap/.git" ] || { echo "$tap is not a git checkout" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Downloads one asset and its .sha256, checks they agree, and prints the checksum.
# The formula pins the checksum, so the published one must match the actual file.
checksum() {
  asset="$1"
  curl -fsSL "$base/$asset" -o "$tmp/$asset"
  curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
  published="$(cut -d' ' -f1 "$tmp/$asset.sha256")"
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
  if [ "$published" != "$actual" ]; then
    echo "Checksum mismatch for $asset: published $published, archive is $actual" >&2
    exit 1
  fi
  echo "$actual"
}

echo "Downloading $tag release files from $repo"
sha_macos="$(checksum rcp-macos-universal.tar.gz)"
sha_x86_64="$(checksum rcp-linux-x86_64.tar.gz)"
sha_aarch64="$(checksum rcp-linux-aarch64.tar.gz)"

out="$tap/Formula/rcp.rb"
mkdir -p "$tap/Formula"
# Drop the template's header comment first, then fill in the placeholders.
sed -e '/^# Template for/,/^# the @SHA256_\*@ placeholders/d' \
  -e "s|@VERSION@|$version|g" -e "s|@REPO@|$repo|g" \
  -e "s|@SHA256_MACOS@|$sha_macos|g" \
  -e "s|@SHA256_LINUX_X86_64@|$sha_x86_64|g" \
  -e "s|@SHA256_LINUX_AARCH64@|$sha_aarch64|g" \
  "$root/packaging/homebrew/rcp.rb.in" > "$out"

grep -q '^class Rcp < Formula' "$out" || { echo "rendered formula is malformed" >&2; exit 1; }
if grep -q '@[A-Z0-9_]*@' "$out"; then echo "unfilled placeholder in $out" >&2; exit 1; fi
if command -v ruby >/dev/null; then ruby -c "$out" >/dev/null; fi

echo "Wrote $out ($version)"
echo "  macOS universal  $sha_macos"
echo "  Linux x86_64     $sha_x86_64"
echo "  Linux aarch64    $sha_aarch64"
echo
echo "Next:"
echo "  cd $tap && git diff"
echo "  git add Formula/rcp.rb && git commit -m 'rcp $version' && git push"
