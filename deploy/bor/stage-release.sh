#!/usr/bin/env bash
# Unprivileged install of a SHA- and digest-pinned static Rust ELF.
set -euo pipefail
umask 022
if [[ $# -ne 1 || ! "$1" =~ ^[0-9a-f]{40}$ ]]; then
  echo 'error=invalid_release_sha' >&2
  exit 2
fi
sha=$1
root=/opt/bor
incoming="$HOME/bor-incoming/$sha"
release="$root/releases/$sha"
test -d "$incoming"
test -f "$incoming/bor-release.tar.gz"
test -f "$incoming/bor-release.sha256"
cd "$incoming"
sha256sum --status -c bor-release.sha256
bytes=$(stat -c %s bor-release.tar.gz)
test "$bytes" -le 67108864 || { echo 'error=release_archive_too_large' >&2; exit 3; }
expected=$(printf 'SHA256SUMS\nSOURCE_SHA\nrebuild-bot\n')
actual=$(tar -tzf bor-release.tar.gz | LC_ALL=C sort)
[[ "$actual" == "$expected" ]] || { echo 'error=archive_entries_unexpected' >&2; exit 3; }
if [[ -e "$release" ]]; then
  test -d "$release" && test ! -L "$release"
  [[ "$(cat "$release/SOURCE_SHA")" == "$sha" ]]
  (cd "$release" && sha256sum --status -c SHA256SUMS)
  echo "bor_stage=EXISTS_VERIFIED sha=$sha"
  exit 0
fi
temporary=$(mktemp -d "$root/releases/.stage-$sha.XXXXXXXX")
trap 'rm -rf -- "$temporary"' EXIT
tar -xzf bor-release.tar.gz -C "$temporary" --no-same-owner --no-same-permissions
[[ "$(cat "$temporary/SOURCE_SHA")" == "$sha" ]] || { echo 'error=release_sha_mismatch' >&2; exit 3; }
(cd "$temporary" && sha256sum --status -c SHA256SUMS)
test -s "$temporary/rebuild-bot"
chmod 0555 "$temporary/rebuild-bot"
chmod 0444 "$temporary/SOURCE_SHA" "$temporary/SHA256SUMS"
chmod 0755 "$temporary"
mv -T -- "$temporary" "$release"
trap - EXIT
echo "bor_stage=PASS sha=$sha"
