#!/usr/bin/env bash
# Download a SHA-pinned public pilot build, without GitHub CLI or authentication.
# Never run the Rust compiler or an unverified executable on this busy VPS.
set -euo pipefail
umask 077
if [[ "$PWD" != "$(git rev-parse --show-toplevel 2>/dev/null)" ]]; then
  echo 'error=run_from_the_git_repository_root' >&2
  exit 2
fi
if [[ "$(git rev-parse --abbrev-ref HEAD)" != 'feat/bor-native-vps-release' ]]; then
  echo 'error=unexpected_branch' >&2
  exit 2
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo 'error=checkout_has_uncommitted_changes' >&2
  exit 2
fi
for tool in curl sha256sum tar git; do
  command -v "$tool" >/dev/null || { echo "error=missing_$tool" >&2; exit 2; }
done
sha=$(git rev-parse HEAD)
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'error=invalid_sha' >&2; exit 2; }
root=/root/bor-packet
mkdir -p -m 0700 "$root"
scratch=$(mktemp -d "$root/.fetch-XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
base="https://github.com/Lalalalendia/bot/releases/download/bor-pilot-$sha"
for filename in bor-release.tar.gz bor-release.sha256; do
  curl --fail --location --show-error --silent --retry 3 \
    --connect-timeout 10 --max-time 90 \
    "$base/$filename" -o "$scratch/$filename"
done
(cd "$scratch" && sha256sum -c bor-release.sha256)
archived_sha=$(tar -xOzf "$scratch/bor-release.tar.gz" SOURCE_SHA)
[[ "$archived_sha" == "$sha" ]] || { echo 'error=release_source_sha_mismatch' >&2; exit 3; }
install -m 0600 "$scratch/bor-release.tar.gz" "$root/bor-release.tar.gz"
install -m 0600 "$scratch/bor-release.sha256" "$root/bor-release.sha256"
echo "bor_pilot_packet=PASS sha=$sha"
echo "bor_pilot_packet_path=$root"
