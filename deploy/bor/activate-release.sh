#!/usr/bin/env bash
# Activate only an already staged immutable release. Roll back on failure.
set -Eeuo pipefail
umask 022
if [[ $# -lt 1 || $# -gt 2 || ! "$1" =~ ^[0-9a-f]{40}$ ]]; then
  echo 'error=invalid_release_sha' >&2
  exit 2
fi
sha=$1
mode=${2:-manual}
case "$mode" in
  manual)
    test ! -e /etc/bor-pilot-mode || { echo 'error=use_explicit_pilot_mode' >&2; exit 3; }
    before_phase=--before
    after_phase=--after
    ;;
  --unattended)
    test ! -e /etc/bor-pilot-mode || { echo 'error=pilot_cannot_autoactivate' >&2; exit 3; }
    test -f /etc/bor-unattended-approved || {
      echo 'error=offhost_restore_not_approved_for_unattended_rollout' >&2
      exit 3
    }
    before_phase=--before
    after_phase=--after
    ;;
  --pilot)
    test -f /etc/bor-pilot-mode || { echo 'error=pilot_bootstrap_not_installed' >&2; exit 3; }
    # Cgroup memory ceiling must actually be loaded before any Telegram polling.
    memory_max=$(systemctl show --value --property=MemoryMax bor.service)
    [[ "$memory_max" == 67108864 ]] || {
      echo 'error=pilot_systemd_64MiB_memory_cap_missing' >&2
      exit 3
    }
    before_phase=--pilot-before
    after_phase=--pilot-after
    ;;
  *) echo 'error=invalid_activation_mode' >&2; exit 2 ;;
esac
root=/opt/bor
release="$root/releases/$sha"
test -d "$release" && test ! -L "$release"
[[ "$(cat "$release/SOURCE_SHA")" == "$sha" ]]
(cd "$release" && sha256sum --status -c SHA256SUMS)
test -f "$root/.credentials-ready" || { echo 'error=credentials_not_provisioned' >&2; exit 3; }
exec 9>"$root/.deployment.lock"
flock -x 9
previous=''
changed=0
if [[ -L "$root/current" ]]; then
  previous=$(readlink -f -- "$root/current")
  case "$previous" in "$root"/releases/*) ;; *) echo 'error=unsafe_previous_release' >&2; exit 3 ;; esac
elif [[ -e "$root/current" ]]; then
  echo 'error=current_is_not_a_symlink' >&2
  exit 3
fi
rollback() {
  code=$?
  trap - ERR
  if [[ "$changed" -eq 1 ]]; then
    if [[ -n "$previous" ]]; then
      ln -s "$previous" "$root/.rollback.$$"
      mv -Tf -- "$root/.rollback.$$" "$root/current"
      sudo -n systemctl restart bor.service || true
      echo 'bor_rollback=previous_release_restored' >&2
    else
      sudo -n systemctl stop bor.service || true
      rm -f -- "$root/current"
      echo 'bor_rollback=first_release_stopped' >&2
    fi
  fi
  exit "$code"
}
trap rollback ERR
if [[ "$previous" == "$release" ]]; then
  echo 'bor_release=ALREADY_CURRENT'
  exit 0
fi
# Read-only shared-host safety check before touching BOR or its SQLite.
# The root-owned script is installed only by operator-reviewed bootstrap.
shared_before=$(/usr/local/libexec/bor-shared-vps-preflight "$before_phase")
printf '%s\n' "$shared_before"
before_units=$(printf '%s\n' "$shared_before" | grep '^erida_unit=')
test -n "$before_units"
# Use SQLite online-backup API via sqlite3 CLI, never cp a live WAL database.
sudo -n systemctl start bor-backup.service
ln -s "$release" "$root/.current.next.$$"
mv -Tf -- "$root/.current.next.$$" "$root/current"
changed=1
# This is a READ-ONLY Telegram preflight; it never polls or opens SQLite.
sudo -n systemctl start bor-preflight.service
sudo -n systemctl restart bor.service
sleep 8
systemctl is-active --quiet bor.service
[[ "$(readlink -f -- "$root/current")" == "$release" ]]
# Recheck both remaining resources and Erida's exact service states.
# Any change triggers the existing BOR-only rollback trap.
shared_after=$(/usr/local/libexec/bor-shared-vps-preflight "$after_phase")
printf '%s\n' "$shared_after"
after_units=$(printf '%s\n' "$shared_after" | grep '^erida_unit=')
if [[ "$before_units" != "$after_units" ]]; then
  echo 'error=erida_service_state_changed_during_bor_activation' >&2
  false
fi
echo 'bor_shared_vps_erida_services_unchanged=PASS'
trap - ERR
echo "bor_release=PASS sha=$sha"
