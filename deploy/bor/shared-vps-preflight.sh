#!/usr/bin/env bash
# Read-only safety gate for sharing an existing Erida/Chaptera VPS with BOR.
# Works BEFORE BOR bootstrap as an ordinary unprivileged SSH account.
# No tokens, sudo, systemctl mutations or writes to server files.
set -euo pipefail
if [[ $# -gt 1 ]]; then
  echo 'error=expected_optional_before_or_after' >&2
  exit 2
fi
phase=${1:---before}
case "$phase" in
  --before) min_mem_kib=786432 ;;  # Strict production, unchanged
  --after) min_mem_kib=655360 ;;
  --pilot-before) min_mem_kib=262144 ;;  # Explicit 64-MiB-limited test bot only
  --pilot-after) min_mem_kib=196608 ;;
  *) echo 'error=invalid_shared_vps_phase' >&2; exit 2 ;;
esac

for command in awk df curl systemctl; do
  command -v "$command" >/dev/null || { echo "error=missing_$command" >&2; exit 2; }
done
[[ "$(uname -m)" == "x86_64" ]] || { echo 'error=unsupported_shared_vps_architecture' >&2; exit 2; }
for root in /opt/bor /var/lib/bor; do
  [[ ! -L "$root" ]] || { echo 'error=bor_path_must_not_be_symlink' >&2; exit 3; }
done
release_root=/opt
state_root=/var/lib
[[ ! -d /opt/bor ]] || release_root=/opt/bor
[[ ! -d /var/lib/bor ]] || state_root=/var/lib/bor
available_kib=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
release_free=$(df -Pm "$release_root" | awk 'END {print $4}')
state_free=$(df -Pm "$state_root" | awk 'END {print $4}')
for value in "$available_kib" "$release_free" "$state_free"; do
  [[ "$value" =~ ^[0-9]+$ ]] || { echo 'error=invalid_system_resource_measurement' >&2; exit 3; }
done
echo "bor_shared_phase=${phase#--}"
echo "available_memory_mib=$((available_kib / 1024))"
echo "release_disk_available_mib=$release_free"
echo "state_disk_available_mib=$state_free"
if (( available_kib < min_mem_kib )); then
  echo "error=shared_vps_ram_below_$((min_mem_kib / 1024))MiB" >&2
  exit 3
fi
if (( release_free < 1536 || state_free < 1536 )); then
  echo 'error=shared_vps_disk_below_1536MiB' >&2
  exit 3
fi

# The pilot's lower admission threshold is permitted only with NO recent
# memory pressure. Production keeps its independent 768/640 MiB thresholds.
if [[ "$phase" == --pilot-* ]]; then
  some_10=$(awk '$1 == "some" { for (i=2; i<=NF; i++) if ($i ~ /^avg10=/) { sub(/^avg10=/, "", $i); print $i } }' /proc/pressure/memory)
  full_10=$(awk '$1 == "full" { for (i=2; i<=NF; i++) if ($i ~ /^avg10=/) { sub(/^avg10=/, "", $i); print $i } }' /proc/pressure/memory)
  [[ "$some_10" =~ ^[0-9]+([.][0-9]+)?$ && "$full_10" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
    echo 'error=memory_pressure_metrics_missing' >&2
    exit 3
  }
  echo "pilot_memory_some_avg10=$some_10"
  echo "pilot_memory_full_avg10=$full_10"
  if awk -v some="$some_10" -v full="$full_10" 'BEGIN { exit !(some > 1.0 || full > 0.2) }'; then
    echo 'error=pilot_memory_pressure_too_high' >&2
    exit 3
  fi
fi

# A public, token-free request only verifies HTTPS/TLS and outgoing routing.
# 403/404 are acceptable as transport evidence; this is NOT a Telegram bot test.
if ! telegram_code=$(curl -q --noproxy '*' --proto '=https' --tlsv1.2 \
    --connect-timeout 5 --max-time 12 --silent --show-error \
    --output /dev/null --write-out '%{http_code}' 'https://api.telegram.org/'); then
  echo 'error=telegram_egress_unavailable' >&2
  exit 3
fi
if [[ ! "$telegram_code" =~ ^[234][0-9][0-9]$ ]]; then
  echo 'error=telegram_https_bad_status' >&2
  exit 3
fi
echo 'telegram_egress=PASS'

# Use exact unit names; never touch/start/stop any of these Erida services.
# Some optional units may be absent or intentionally inactive.
units=(
  chaptera-reader.service
  chaptera-translator-api.service
  chaptera-translator-worker.service
  chaptera-translator-source-worker.service
  chaptera-translator-drive-export.timer
  chaptera-translator-backup-drive.timer
)
seen=0
running=0
for unit in "${units[@]}"; do
  if ! metadata=$(systemctl show --no-pager \
      --property=LoadState --property=ActiveState "$unit"); then
    echo "error=systemd_query_failed:$unit" >&2
    exit 3
  fi
  load=''
  active=''
  while IFS='=' read -r key value; do
    case "$key" in
      LoadState) load=$value ;;
      ActiveState) active=$value ;;
    esac
  done <<< "$metadata"
  case "$load" in
    not-found|masked) continue ;;
    loaded) ;;
    *) echo "error=unexpected_unit_load_state:$unit" >&2; exit 3 ;;
  esac
  case "$active" in
    active|inactive) ;;
    *) echo "error=erida_unit_not_stable:$unit:$active" >&2; exit 3 ;;
  esac
  ((seen+=1))
  [[ "$active" != active ]] || ((running+=1))
  echo "erida_unit=$unit|$active"
done
if (( seen < 1 || running < 1 )); then
  echo 'error=no_active_known_erida_service_wrong_or_unhealthy_vps' >&2
  exit 3
fi
echo 'bor_shared_vps_preflight=PASS'
echo 'shared_vps_mutations=no'
