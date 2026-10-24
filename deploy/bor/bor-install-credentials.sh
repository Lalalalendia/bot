#!/usr/bin/env bash
# Root-owned, one-time provisioning target for protected GitHub Secrets.
# Read two lines from stdin; never echo credentials.
set -euo pipefail
umask 077
if [[ "$(id -u)" -ne 0 ]]; then
  echo 'error=root_required' >&2
  exit 2
fi
if [[ -e /etc/bor/bot_token ]]; then
  echo 'error=credential_already_exists_rotation_requires_operator' >&2
  exit 3
fi
token=''
chat=''
IFS= read -r token || { echo 'error=missing_token' >&2; exit 2; }
IFS= read -r chat || { echo 'error=missing_test_chat_id' >&2; exit 2; }
if [[ ! "$token" =~ ^[0-9]{5,20}:[A-Za-z0-9_-]{20,}$ ]]; then
  echo 'error=invalid_bot_token_format' >&2
  exit 2
fi
if [[ ! "$chat" =~ ^-100[0-9]{5,18}$ ]]; then
  echo 'error=test_chat_must_be_supergroup_id' >&2
  exit 2
fi
test -d /etc/bor
temporary_token=$(mktemp /etc/bor/.token.XXXXXX)
temporary_env=$(mktemp /etc/bor/.env.XXXXXX)
trap 'rm -f -- "$temporary_token" "$temporary_env"' EXIT
printf '%s\n' "$token" > "$temporary_token"
printf 'TEST_CHAT_ID=%s\n' "$chat" > "$temporary_env"
chmod 0600 "$temporary_token" "$temporary_env"
chown root:root "$temporary_token" "$temporary_env"
mv -T -- "$temporary_env" /etc/bor/preflight.env
mv -T -- "$temporary_token" /etc/bor/bot_token
touch /opt/bor/.credentials-ready
chmod 0644 /opt/bor/.credentials-ready
unset token chat
echo 'bor_credentials=installed'
