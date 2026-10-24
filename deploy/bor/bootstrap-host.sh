#!/usr/bin/env bash
# ONE-TIME root bootstrap on a dedicated Ubuntu/Debian VPS. No service activation.
set -euo pipefail
umask 022
if [[ "$(id -u)" -ne 0 ]]; then
  echo 'error=root_required' >&2
  exit 2
fi
[[ "$(uname -m)" == x86_64 ]] || { echo 'error=unsupported_cpu' >&2; exit 3; }
for program in systemctl useradd install sudo visudo sha256sum flock sqlite3 tar; do
  command -v "$program" >/dev/null || { echo "error=missing_$program" >&2; exit 3; }
done
script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
if [[ -L /opt/bor || -L /etc/bor || -L /var/lib/bor ]]; then
  echo 'error=unexpected_symlink_in_service_roots' >&2
  exit 3
fi
if ! id -u bor >/dev/null 2>&1; then
  useradd --system --user-group --home-dir /var/lib/bor --shell /usr/sbin/nologin bor
fi
if ! id -u bor-deploy >/dev/null 2>&1; then
  useradd --create-home --user-group --shell /bin/bash bor-deploy
fi
[[ "$(id -gn bor)" == bor && "$(id -gn bor-deploy)" == bor-deploy ]]
if id -nG bor-deploy | grep -Eq '(^| )(sudo|root|docker)( |$)'; then
  echo 'error=deploy_account_has_privileged_group' >&2
  exit 3
fi
install -d -o bor-deploy -g bor-deploy -m 0755 /opt/bor /opt/bor/releases
install -d -o bor-deploy -g bor-deploy -m 0700 /home/bor-deploy/bor-incoming
install -d -o bor -g bor -m 0700 /var/lib/bor
install -d -o root -g root -m 0700 /etc/bor
install -d -o root -g root -m 0755 /usr/local/libexec
install -o root -g root -m 0644 "$script_dir/bor.service" /etc/systemd/system/bor.service
install -o root -g root -m 0644 "$script_dir/bor-preflight.service" /etc/systemd/system/bor-preflight.service
install -o root -g root -m 0644 "$script_dir/bor-backup.service" /etc/systemd/system/bor-backup.service
install -o root -g root -m 0755 "$script_dir/bor-backup.sh" /usr/local/libexec/bor-backup
install -o root -g root -m 0755 "$script_dir/host-preflight.sh" /usr/local/libexec/bor-host-preflight
install -o root -g root -m 0755 "$script_dir/shared-vps-preflight.sh" /usr/local/libexec/bor-shared-vps-preflight
install -o root -g root -m 0755 "$script_dir/stage-release.sh" /usr/local/libexec/bor-stage-release
install -o root -g root -m 0755 "$script_dir/activate-release.sh" /usr/local/libexec/bor-activate-release
install -o root -g root -m 0755 "$script_dir/bor-install-credentials.sh" /usr/local/sbin/bor-install-credentials
sysctl_bin=$(command -v systemctl)
sudoers_tmp=$(mktemp /etc/sudoers.d/.bor-deploy.XXXXXXXX)
trap 'rm -f -- "$sudoers_tmp"' EXIT
printf 'bor-deploy ALL=(root) NOPASSWD: %s start bor-backup.service, %s start bor-preflight.service, %s restart bor.service, %s stop bor.service, /usr/local/sbin/bor-install-credentials\n' \
  "$sysctl_bin" "$sysctl_bin" "$sysctl_bin" "$sysctl_bin" > "$sudoers_tmp"
chmod 0440 "$sudoers_tmp"
visudo -cf "$sudoers_tmp"
mv -T -- "$sudoers_tmp" /etc/sudoers.d/bor-deploy
trap - EXIT
systemctl daemon-reload
systemctl enable bor.service
echo 'bor_bootstrap=PASS'
echo 'bor_service_started=no'
echo 'next=install_deploy_public_key_then_configure_GitHub_Secrets'
