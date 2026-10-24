#!/usr/bin/env bash
# SQLite live backup before switching the running Rust binary.
set -euo pipefail
umask 077
db=/var/lib/bor/rebuild.sqlite3
directory=/var/lib/bor/backups
if [[ ! -f "$db" ]]; then
  echo 'bor_backup=initial_install_no_database'
  exit 0
fi
install -d -m 0700 "$directory"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
destination="$directory/bor-$stamp-$$.sqlite3"
temporary="$destination.tmp"
trap 'rm -f -- "$temporary"' EXIT
sqlite3 "$db" ".timeout 5000" ".backup '$temporary'"
test -s "$temporary"
chmod 0600 "$temporary"
mv -T -- "$temporary" "$destination"
(cd "$directory" && sha256sum "$(basename "$destination")" > "$(basename "$destination").sha256")
echo 'bor_backup=PASS'
# These are LOCAL backups only. Encrypted off-host backup and restore
# are separate readiness gates for unattended production deployment.
