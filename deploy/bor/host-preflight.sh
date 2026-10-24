#!/usr/bin/env bash
# Read-only checks; intended to run as bor-deploy over SSH.
set -euo pipefail
if [[ "$(id -un)" != 'bor-deploy' ]]; then
  echo 'error=expected_bor_deploy_account' >&2
  exit 2
fi
if [[ "$(uname -m)" != 'x86_64' ]]; then
  echo 'error=unsupported_cpu_expected_x86_64' >&2
  exit 2
fi
for executable in sha256sum tar flock systemctl sqlite3; do
  command -v "$executable" >/dev/null || { echo "error=missing_$executable" >&2; exit 2; }
done
for directory in /opt/bor /opt/bor/releases /var/lib/bor; do
  test -d "$directory" || { echo "error=missing_directory_$directory" >&2; exit 2; }
done
test -f /etc/systemd/system/bor.service
test -x /usr/local/libexec/bor-stage-release
test -x /usr/local/libexec/bor-activate-release
test -x /usr/local/sbin/bor-install-credentials
ram=$(awk '/MemAvailable:/ {print $2}' /proc/meminfo)
disk=$(df -Pm /opt/bor | awk 'END {print $4}')
test "$ram" -ge 131072 || { echo 'error=insufficient_available_memory_less_than_128MiB' >&2; exit 3; }
test "$disk" -ge 256 || { echo 'error=insufficient_disk_less_than_256MiB' >&2; exit 3; }
echo 'bor_host_preflight=PASS'
echo "available_memory_mib=$((ram / 1024))"
echo "release_disk_available_mib=$disk"
echo 'remote_mutations=no'
