# BOR AntiBot — native Rust deployment on VPS (no Docker)

This is a deployment **candidate** for the Rust BOR implementation. It does not
deploy anything just by being merged into a feature branch. Live Telegram
acceptance, server bootstrap and GitHub Environment provisioning are operator
actions. The production long-poller is never started by a pull-request CI job.

## Source and release layout

Borrowed from the established Chaptera/Erida first-VPS deployment practices:
immutable per-commit release, SHA-256 receipt, verified SSH host identity,
read-only preflight, atomic current symlink, systemd, rollback. The bot itself
is a single static MUSL executable, without Docker, HTTP, Nginx or Python worker.

    GitHub main + protected production environment
        -> cargo fmt / clippy / tests / MUSL ELF (exact commit)
        -> tar.gz + SHA256 receipt
        -> SSH as bor-deploy
        -> /opt/bor/releases/<40-character Git SHA>
        -> manual approval / unattended release gate
        -> SQLite online backup
        -> /opt/bor/current (atomic symlink switch)
        -> one-shot read-only Telegram preflight
        -> systemd bor.service restart / liveness check
        -> rollback to prior symlink on any failure

Server paths:

    /opt/bor/releases/<SHA>/rebuild-bot       Executable; immutable identity
    /opt/bor/releases/<SHA>/SOURCE_SHA         Commit identity
    /opt/bor/releases/<SHA>/SHA256SUMS         Verified bytes
    /opt/bor/current                          Atomic active-version symlink
    /var/lib/bor/rebuild.sqlite3               Persistent SQLite, never in releases
    /var/lib/bor/backups/                     Consistent LOCAL pre-upgrade backups
    /etc/bor/bot_token                        root:root mode 0600
    /etc/bor/preflight.env                    root:root mode 0600 (test group only)
    /etc/bor-unattended-approved              Root-only unattended-approval marker

GitHub does not deploy from PRs. After the project is merged into main,
workflow dispatch can stage without starting the bot. Automatic push-on-main
deployment requires BOR_DEPLOY_ENABLED=true and the independent off-host
restore approval marker on the VPS.

## 1. Operator bootstrap on a Linux x86_64 VPS

Prerequisites: Ubuntu/Debian with systemd and sudo; verify the server identity
from your hosting provider independently. At least 128 MiB MemAvailable and
256 MiB free for release staging are required by the read-only host probe.
Do not bypass the preflight by lowering the thresholds. If this shares the
Erida/Chaptera VPS, measure current RAM and service impact independently.

Clone the approved SHA of this branch on the server using the operator account
(not the deployment user), then run:

    sudo apt-get update
    sudo apt-get install -y sqlite3 sudo openssh-server
    sudo bash deploy/bor/bootstrap-host.sh

The bootstrap creates the separate system user "bor", the unprivileged SSH
deployment user "bor-deploy", systemd units, and a *strict sudo command allowlist*.
It enables (but does not start) bor.service; no token is configured by bootstrap.
It does not modify Erida, Chaptera, Nginx, or existing Telegram services.

On a separate trusted computer, create an ED25519 SSH deploy key. Copy its PUBLIC
key to /home/bor-deploy/.ssh/authorized_keys (owner bor-deploy, directory 0700,
file 0600) using the operator's sudo permissions. Keep the private key only in
GitHub secrets; do not give bor-deploy sudo/admin/root group membership.

Validate the VPS SSH host public-key fingerprint *out of band* from the provider
console before storing its known_hosts line. Raw ssh-keyscan alone is not
authentication.

## 2. GitHub Environment secrets (no credential text in the repository)

In repository Settings -> Environments create "production" and add protection
(required reviewer and main-branch restriction, if available).

Add the following *GitHub Actions Environment secrets*:

    BOR_DEPLOY_HOST              VPS hostname/address
    BOR_DEPLOY_USER              bor-deploy
    BOR_DEPLOY_SSH_KEY           Private ED25519 deploy key
    BOR_DEPLOY_KNOWN_HOSTS       Independently verified OpenSSH known_hosts line
    BOR_BOT_TOKEN                Private TEST BotFather token
    BOR_TEST_CHAT_ID             Numeric -100... ID of the dedicated test supergroup

Do not paste BOT_TOKEN in .env, Git history, Actions variables, Notion or chat.
The one-time provisioning step passes BOT_TOKEN and TEST_CHAT_ID over SSH stdin
to /usr/local/sbin/bor-install-credentials. The helper never echoes them and
refuses to overwrite an installed token. systemd LoadCredential delivers the
token file to the Rust process at startup; it is not in systemd Environment,
command-line args, release directories or runner build artifacts.

Token rotation is intentionally not an unreviewed automatic release action.
Use an operator-controlled credential rotation and verify Telegram preflight.

## 3. Stage and then activate (manual first deployment)

The workflow can be dispatched from GitHub Actions **only after its YAML exists
on the default main branch**. Prior to that, this PR is for validation only.
Also finish the Rust foundation and AntiBot PRs; do not deploy a partial main.

GitHub -> Actions -> "BOR native Rust VPS release" -> Run workflow -> main:

1. First run: provision_credentials=true, activate=false.
   This runs tests, builds the MUSL binary, stages its verified release under
   /opt/bor/releases/<SHA>, and installs the test token exactly once. No bot start.
2. Confirm staging receipt, SSH host check, separate TEST bot, no competing
   Telegram getUpdates consumer, and no webhook.
3. Second run: provision_credentials=false, activate=true.
   This verifies the same release, performs a read-only Telegram preflight,
   starts/restarts bor.service and observes immediate process liveness.
4. Confirm with a real second Telegram account: newcomer restriction,
   challenge success/failure, no-message timeout, restart recovery and manual
   moderation. See TEST_DEPLOY.md and the Notion BOR acceptance checklist.

Read-only verification / logs on the VPS:

    systemctl status bor.service --no-pager
    journalctl -u bor.service -n 80 --no-pager
    readlink -f /opt/bor/current
    sudo -u bor-deploy /usr/local/libexec/bor-host-preflight

A passing systemd process check is not equivalent to a successful real Telegram
challenge. Do not assign the production group until acceptance passes.

## 4. Unattended deployments and rollback

The first deploy stays manual. Before allowing unattended updates:

- Prove an actual encrypted OFF-HOST backup of the live BOR SQLite and an
  independent restore. Local /var/lib/bor/backups does not satisfy this.
- Have the root operator create /etc/bor-unattended-approved only after that
  evidence has been reviewed:

      sudo install -m 0644 -o root -g root /dev/null /etc/bor-unattended-approved

- Set repository variable BOR_DEPLOY_ENABLED=true. Future pushes to main that
  touch BOR source/deploy files will build, test, stage and attempt activation
  without carrying or replacing the BotFather token. The production Environment
  may additionally require explicit approval for each run.

Only already staged SHA releases can activate. The launcher stops old polling
by a single systemd restart, not by running two instances. An installation error
restores the previous current symlink and attempts to restart that release.

Manual rollback (operator, using an ALREADY INSTALLED previous 40-hex SHA):

    sudo -u bor-deploy /usr/local/libexec/bor-activate-release PREVIOUS_40_HEX_SHA manual

The rollback itself runs preflight, backup, restart and health checks. If the
first-ever release fails there is no previous binary; the service is stopped.

Never use "cp" to back up an open SQLite database with WAL. The local backup
service uses the SQLite online backup API. Its local copies are not encrypted
off-host disaster recovery and must not be mistaken for that requirement.

## Boundaries and non-goals

- Only Linux x86_64 supported by this release workflow. ARM64 needs its own
  compiled target and host gate.
- GitHub Secrets and deployment Environment must be configured by the operator.
  The GitHub integration cannot write these settings for you.
- The host bootstrap and root-owned deployment scripts need manual re-bootstrap
  if their source changes; a normal application release cannot silently grant
  itself additional server privileges.
- A healthy process after 8 seconds is only immediate liveness. Independent
  alerts for stuck polling and off-host backup/restore acceptance remain
  necessary before declaring fully unattended production readiness.
- The code remains Draft until exact-head CI and a live test Telegram group
  have passed. No automatic changes are made to the existing Erida VPS.
