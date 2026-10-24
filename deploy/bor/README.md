# BOR AntiBot — native Rust deployment on VPS (no Docker)

This is a deployment **candidate** for the Rust BOR implementation. It does not
deploy anything just by being merged into a feature branch. Live Telegram
acceptance, server bootstrap and GitHub Environment provisioning are operator
actions. The production long-poller is never started by a pull-request CI job.


## 0. Limited-memory pilot on an existing 891-MiB shared VPS

Only use this path with a separate test BotFather token and an isolated test
supergroup, **never** the working community bot or real production chat.
This avoids unsafe native Rust compilation on a busy 1-GiB VPS.

Pilot gate is explicitly different from production:

- Admission: >=256 MiB MemAvailable and PSI memory "some avg10" <=1.00%
  and "full avg10" <=0.20%, with the same disk/Telegram/Erida checks.
- After launch: >=192 MiB MemAvailable and the same pressure/service checks.
- Strict production thresholds of 768/640 MiB remain unchanged.
- Pilot drop-in caps BOR at **64 MiB RAM and 25% CPU** and disables automatic
  restart. The BOR service is **not enabled on boot** during the pilot.
- Rust runtime ignores messages, joins and callbacks outside TEST_CHAT_ID.
  Never use a token already running another long-polling process.

**Step A: Update checkout on the VPS and inspect the read-only pilot gate:**

    cd /root/bot
    git fetch origin feat/bor-native-vps-release
    git switch feat/bor-native-vps-release
    git pull --ff-only
    bash deploy/bor/shared-vps-preflight.sh --pilot-before

Do not proceed unless output includes `bor_shared_vps_preflight=PASS`
and `shared_vps_mutations=no`. If the memory budget shrinks or PSI rises,
refuse pilot deployment; never bypass checks.

**Step B: Direct HTTPS download from public GitHub release (no login).**

The feature-branch `push` workflow builds, verifies and publishes a
SHA-pinned **public prerelease** in this public repository. The GitHub runner
uses its own ephemeral `GITHUB_TOKEN` to publish the two checksum-verified
files. **Never install gh or authenticate GitHub on the VPS.**

After the corresponding native VPS CI and its public-pilot release job are
both green, from the already cloned VPS checkout:

    cd /root/bot
    git pull --ff-only
    bash deploy/bor/pilot-download.sh

The helper downloads both files from
`https://github.com/Lalalalendia/bot/releases/download/bor-pilot-<SHA>/`,
verifies SHA-256 and the archive's SOURCE_SHA against the checked-out
Git commit, then saves them in `/root/bor-packet`. No Docker, Rust compiler,
SCP, GitHub CLI, interactive login or credentials are needed on the VPS.
The later production auto-delivery uses the Erida-style GitHub Actions SSH
secrets instead of the public pilot release.

**Step C: One-time root bootstrap, credential provision and staged install.**

    cd /root/bot
    sudo apt-get update
    sudo apt-get install -y sqlite3 sudo
    sudo bash deploy/bor/bootstrap-host.sh --pilot
    sha=$(git rev-parse HEAD)
    cd /root/bor-packet
    sha256sum -c bor-release.sha256
    tar -xOzf bor-release.tar.gz SOURCE_SHA | grep -Fx "$sha"
    sudo install -d -m 0700 -o bor-deploy -g bor-deploy "/home/bor-deploy/bor-incoming/$sha"
    sudo install -m 0600 -o bor-deploy -g bor-deploy bor-release.tar.gz bor-release.sha256 "/home/bor-deploy/bor-incoming/$sha/"
    sudo -u bor-deploy -H /usr/local/libexec/bor-stage-release "$sha"

Run these commands only after confirming `sha` matches the downloaded
artifact. The root bootstrap itself repeats the pilot resource gate.
Provision the secret without printing it:

    read -rs -p 'TEST BotFather token: ' BOR_TOKEN; echo
    read -r -p 'Test supergroup ID (-100...): ' BOR_CHAT
    printf '%s\n%s\n' "$BOR_TOKEN" "$BOR_CHAT" |
      sudo /usr/local/sbin/bor-install-credentials
    unset BOR_TOKEN BOR_CHAT

The token is not stored in the repository. For later automated production
rollouts use protected GitHub Environment secrets; the pilot is a one-time
operator installation because the native deploy workflow is not yet in main.

**Step D: Manual start after Telegram preflight:**

    sudo -u bor-deploy -H /usr/local/libexec/bor-activate-release "$sha" --pilot
    systemctl status bor.service --no-pager
    systemctl show bor.service -p MemoryMax -p MemoryCurrent -p CPUQuotaPerSecUSec
    readlink -f /opt/bor/current
    journalctl -u bor.service -n 50 --no-pager

Any failure should stop or roll back *only BOR*. To stop the pilot:

    sudo systemctl stop bor.service

Do not enable `BOR_DEPLOY_ENABLED` or create
`/etc/bor-unattended-approved` on this constrained pilot. Run real
Telegram tests before enabling any bot autostart. Keep 319 MiB available
RAM and zero PSI in perspective: these were the prior user measurements,
not a guarantee of current headroom.

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


## Shared FirstVDS with Erida: read-only safety gate

This branch is intended for the *same* VPS as Chaptera/Erida, not a fresh
standalone VPS. It does not alter Erida services, Nginx, networking or SQLite.
Before the BOR bootstrap, an operator can run this read-only check from a
trusted workstation with an existing read-only SSH login:

    ssh USER@YOUR_VPS 'bash -s -- --before' < deploy/bor/shared-vps-preflight.sh

Use the SSH fingerprint independently verified through the hosting provider;
never disable host-key checking. No bot token is needed for this preflight.
It works **before** /opt/bor exists and creates no files on the VPS.

The fail-closed thresholds are conservative shared-service *admission gates*,
not benchmark results or proof of future resource sufficiency:

- Before install/switch: **at least 768 MiB MemAvailable** (512 MiB reserved
  for the Erida source worker, 128 MiB BOR cgroup ceiling, 128 MiB safety margin).
- After BOR startup: **at least 640 MiB MemAvailable**.
- At least **1536 MiB free** on both the future release filesystem (/opt)
  and SQLite filesystem (/var/lib); both can refer to the same mount.
- Public HTTPS/TLS reachability of api.telegram.org **without a token**.
  This only proves outgoing routing, not BotFather authorization.
- At least one **active, known Chaptera/Erida systemd unit**, stable states
  for all observed units, and no failed/activating/deactivating units. Checked
  names are chaptera-reader, chaptera-translator-api, chaptera-translator-worker,
  optional source-worker, and optional Drive/backup timers.
- On activation, **the exact observed Erida unit/status map must be unchanged**.
  Failure rolls back only BOR to the previous release, or stops BOR if it was
  the first installation. It never restarts Erida or Nginx.

GitHub Actions repeats this read-only gate before any staging, stores a
sanitized 7-day preflight report, and verifies SHA-256 of all root-installed
BOR deployment scripts and systemd units against the current reviewed Git
commit. An outdated root bootstrap blocks deployment rather than bypassing
the new shared-VPS gates. The root-installed BOR activation script repeats
the gate immediately before switching and after startup. Staging is denied
if the shared gate cannot prove the host, capacity or Telegram connectivity.

**Important:** Historical Erida preflight recorded only ~313 MiB available
out of 891 MiB total. That is **below** the new 768 MiB gate. This is a
historical observation, NOT a new reading of the VPS. Increase memory or free
capacity only after a fresh operator measurement. Do not lower thresholds to
force the deploy on an overloaded host.

Since activation uses a root-owned local copy of shared-vps-preflight.sh,
the operator must re-run the reviewed bootstrap after these scripts change.
No GitHub release is authorized to update root-owned host scripts itself.

## 1. Operator bootstrap on a Linux x86_64 VPS

Prerequisites: Ubuntu/Debian with systemd and sudo. Verify server identity
independently. The generic BOR host probe requires at least 128 MiB available
and 256 MiB release disk, but the **stricter shared Erida VPS safety gate above
is mandatory** for this deployment and takes precedence. Do not lower it.

Clone the approved SHA of this branch on the server using the operator account
(not the deployment user), then run:

    sudo apt-get update
    sudo apt-get install -y sqlite3 sudo openssh-server
    sudo bash deploy/bor/bootstrap-host.sh

Bootstrap **reruns the read-only Shared VPS gate first**. If resources are
insufficient, Telegram is unreachable or Erida has no active known service,
it refuses before any BOR user, directory or systemd unit is created.
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
