"""Offline contract checks for the native Rust VPS deployment."""
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "bor"


class NativeDeployTests(unittest.TestCase):
    def text(self, relative):
        return (ROOT / relative).read_text(encoding="utf-8")

    def test_all_scripts_parse_without_running(self):
        scripts = list(DEPLOY.glob("*.sh"))
        self.assertGreaterEqual(len(scripts), 6)
        for path in scripts:
            with self.subTest(script=path.name):
                subprocess.run(["bash", "-n", str(path)], check=True)

    def test_service_loads_secret_via_systemd_not_environment(self):
        for name in ("bor.service", "bor-preflight.service"):
            service = self.text(f"deploy/bor/{name}")
            self.assertIn("LoadCredential=bot_token:/etc/bor/bot_token", service)
            self.assertIn("User=bor", service)
            self.assertIn("NoNewPrivileges=yes", service)
            self.assertNotIn("Environment=BOT_TOKEN=", service)
            self.assertIn("/opt/bor/current/rebuild-bot", service)
        self.assertIn("DATABASE_PATH=/var/lib/bor/rebuild.sqlite3",
                      self.text("deploy/bor/bor.service"))

    def test_linux_bot_uses_credential_file_before_env(self):
        rust = self.text("src/main.rs")
        self.assertIn('std::env::var_os("CREDENTIALS_DIRECTORY")', rust)
        self.assertIn('directory.join("bot_token")', rust)
        self.assertIn("missing_systemd_credential_never_falls_back_to_env", rust)

    def test_release_identity_and_rollback_are_preserved(self):
        staging = self.text("deploy/bor/stage-release.sh")
        activate = self.text("deploy/bor/activate-release.sh")
        self.assertIn("sha256sum --status -c bor-release.sha256", staging)
        self.assertIn("sha256sum --status -c SHA256SUMS", staging)
        self.assertIn('[[ $# -ne 1 || ! "$1" =~ ^[0-9a-f]{40}$ ]]', staging)
        self.assertIn("flock -x 9", activate)
        self.assertIn("mv -Tf", activate)
        self.assertIn("trap rollback ERR", activate)
        self.assertIn("systemctl start bor-preflight.service", activate)
        self.assertIn("systemctl start bor-backup.service", activate)
        self.assertIn("/etc/bor-unattended-approved", activate)

    def test_bootstrap_does_not_activate_and_sudo_is_scoped(self):
        bootstrap = self.text("deploy/bor/bootstrap-host.sh")
        self.assertIn("systemctl enable bor.service", bootstrap)
        self.assertNotIn("systemctl start bor.service", bootstrap)
        self.assertNotIn("systemctl restart bor.service", bootstrap)
        self.assertIn("visudo -cf", bootstrap)
        self.assertIn("bor-install-credentials", bootstrap)
        provision = self.text("deploy/bor/bor-install-credentials.sh")
        self.assertIn("credential_already_exists_rotation_requires_operator", provision)
        self.assertIn("chmod 0600", provision)
        self.assertNotIn('echo "$token"', provision)

    def test_actions_no_pr_ssh_and_autodeploy_explicit_opt_in(self):
        workflow = self.text(".github/workflows/bor-native-vps.yml")
        self.assertIn("needs: verify-and-package", workflow)
        self.assertIn("environment: production", workflow)
        self.assertIn("vars.BOR_DEPLOY_ENABLED == 'true'", workflow)
        self.assertIn("github.ref == 'refs/heads/main'", workflow)
        self.assertIn("inputs.provision_credentials == true", workflow)
        self.assertIn("inputs.activate == true", workflow)
        self.assertIn("--unattended", workflow)
        self.assertIn("secrets.BOR_BOT_TOKEN", workflow)
        self.assertIn("secrets.BOR_TEST_CHAT_ID", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("targets: x86_64-unknown-linux-musl", workflow)
        self.assertNotIn("docker build", workflow)
        self.assertNotIn("echo \"$BOR_BOT_TOKEN\"", workflow)


    def mock_shared_vps(self, *, phase="--before", mem_kib=1048576,
                        disk_mib=4096, telegram_status="403", erida_state="active",
                        psi_avg10="0.00"):
        """Exercise the real, read-only Bash gate with mock external commands."""
        import os
        import tempfile
        with tempfile.TemporaryDirectory() as temp:
            bindir = Path(temp)
            commands = {
                "awk": '''#!/usr/bin/env bash
if [[ " __D__* " == *"/proc/meminfo"* ]]; then
  printf '%s\\n' "__D__{BOR_FAKE_MEM_KIB:-1048576}"
elif [[ " __D__* " == *"/proc/pressure/memory"* ]]; then
  printf '%s\\n' "__D__{BOR_FAKE_PSI:-0.00}"
else
  exec /usr/bin/awk "__D__@"
fi
''',
                "df": '''#!/usr/bin/env bash
printf 'Filesystem 1M-blocks Used Available Use%% Mounted on\\n'
printf '/dev/fake 10000 1000 %s 10%% /\\n' "__D__{BOR_FAKE_DISK_MIB:-4096}"
''',
                "curl": '''#!/usr/bin/env bash
if [[ "__D__{BOR_FAKE_TELEGRAM:-403}" == offline ]]; then
  exit 7
fi
printf '%s' "__D__{BOR_FAKE_TELEGRAM:-403}"
''',
                "systemctl": '''#!/usr/bin/env bash
unit="__D__{*: -1}"
if [[ "__D__{BOR_FAKE_ERIDA:-active}" == missing ]]; then
  printf 'LoadState=not-found\\nActiveState=inactive\\n'
elif [[ "__D__unit" == chaptera-reader.service ]]; then
  printf 'LoadState=loaded\\nActiveState=%s\\n' "__D__{BOR_FAKE_ERIDA:-active}"
elif [[ "__D__unit" == chaptera-translator-api.service ]]; then
  printf 'LoadState=loaded\\nActiveState=inactive\\n'
else
  printf 'LoadState=not-found\\nActiveState=inactive\\n'
fi
''',
            }
            for name, source in commands.items():
                binary = bindir / name
                binary.write_text(source.replace("__D__", "$"), encoding="utf-8")
                binary.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = str(bindir) + os.pathsep + env.get("PATH", "")
            env.update(
                BOR_FAKE_MEM_KIB=str(mem_kib),
                BOR_FAKE_DISK_MIB=str(disk_mib),
                BOR_FAKE_TELEGRAM=telegram_status,
                BOR_FAKE_ERIDA=erida_state,
                BOR_FAKE_PSI=psi_avg10,
            )
            return subprocess.run(
                ["bash", str(DEPLOY / "shared-vps-preflight.sh"), phase],
                check=False, capture_output=True, text=True, env=env,
            )

    def test_shared_vps_accepts_healthy_erida_and_token_free_telegram(self):
        result = self.mock_shared_vps()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("bor_shared_vps_preflight=PASS", result.stdout)
        self.assertIn("shared_vps_mutations=no", result.stdout)
        self.assertIn("telegram_egress=PASS", result.stdout)
        self.assertIn("erida_unit=chaptera-reader.service|active", result.stdout)
        self.assertIn("erida_unit=chaptera-translator-api.service|inactive", result.stdout)

    def test_shared_vps_blocks_historical_low_memory(self):
        # Previous Erida observation: 313 MiB MemAvailable.
        result = self.mock_shared_vps(mem_kib=313 * 1024)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("shared_vps_ram_below_768MiB", result.stderr)

    def test_shared_vps_preserves_640_mib_after_activation(self):
        self.assertNotEqual(
            self.mock_shared_vps(mem_kib=700 * 1024, phase="--before").returncode, 0
        )
        self.assertEqual(
            self.mock_shared_vps(mem_kib=700 * 1024, phase="--after").returncode, 0
        )
        self.assertNotEqual(
            self.mock_shared_vps(mem_kib=630 * 1024, phase="--after").returncode, 0
        )

    def test_shared_vps_refuses_low_disk_and_blocked_telegram(self):
        self.assertIn("shared_vps_disk_below_1536MiB",
                      self.mock_shared_vps(disk_mib=512).stderr)
        self.assertIn("telegram_egress_unavailable",
                      self.mock_shared_vps(telegram_status="offline").stderr)
        self.assertIn("telegram_https_bad_status",
                      self.mock_shared_vps(telegram_status="503").stderr)

    def test_shared_vps_fails_for_wrong_or_failed_erida_host(self):
        result = self.mock_shared_vps(erida_state="missing")
        self.assertIn("no_active_known_erida_service", result.stderr)
        result = self.mock_shared_vps(erida_state="failed")
        self.assertIn("erida_unit_not_stable", result.stderr)

    def test_shared_vps_gate_is_read_only_and_rollback_is_bor_only(self):
        script = self.text("deploy/bor/shared-vps-preflight.sh")
        for forbidden in ("sudo ", "systemctl restart", "systemctl start",
                          "systemctl stop", "systemctl enable", "BOT_TOKEN",
                          "rm -", "mkdir ", "tee "):
            self.assertNotIn(forbidden, script)
        activation = self.text("deploy/bor/activate-release.sh")
        self.assertIn("bor-shared-vps-preflight --before", activation)
        self.assertIn("bor-shared-vps-preflight --after", activation)
        self.assertIn('if [[ "$before_units" != "$after_units" ]]', activation)
        self.assertIn("error=erida_service_state_changed", activation)
        self.assertNotIn("systemctl restart chaptera-", activation)
        self.assertIn("sudo -n systemctl restart bor.service", activation)
        bootstrap = self.text("deploy/bor/bootstrap-host.sh")
        self.assertIn("/usr/local/libexec/bor-shared-vps-preflight", bootstrap)
        self.assertIn('bash "$script_dir/shared-vps-preflight.sh" --before', bootstrap)
        self.assertLess(
            bootstrap.index('bash "$script_dir/shared-vps-preflight.sh" --before'),
            bootstrap.index('useradd --system'),
        )
        workflow = self.text(".github/workflows/bor-native-vps.yml")
        self.assertIn("bash -s -- --before", workflow)
        self.assertIn("bor-shared-vps-preflight.txt", workflow)
        self.assertIn("bor_host_installer_pinned=PASS", workflow)
        self.assertIn("error=bor_bootstrap_outdated", workflow)
        for source in ("activate-release.sh", "shared-vps-preflight.sh",
                       "stage-release.sh", "bor.service",
                       "bor-preflight.service", "bor-backup.service"):
            self.assertIn(f"deploy/bor/{source}|", workflow)


    def test_pilot_resource_gate_accepts_measured_vps_only_without_pressure(self):
        pilot = self.mock_shared_vps(phase="--pilot-before", mem_kib=319 * 1024)
        self.assertEqual(pilot.returncode, 0, pilot.stdout + pilot.stderr)
        self.assertIn("pilot_memory_some_avg10=0.00", pilot.stdout)
        self.assertIn("bor_shared_vps_preflight=PASS", pilot.stdout)

        # Relaxed threshold must NEVER apply to production profiles.
        strict = self.mock_shared_vps(phase="--before", mem_kib=319 * 1024)
        self.assertIn("shared_vps_ram_below_768MiB", strict.stderr)

    def test_pilot_stops_on_memory_pressure_or_low_available_ram(self):
        self.assertIn("pilot_memory_pressure_too_high",
                      self.mock_shared_vps(phase="--pilot-before",
                                           mem_kib=319 * 1024,
                                           psi_avg10="2.00").stderr)
        self.assertIn("shared_vps_ram_below_256MiB",
                      self.mock_shared_vps(phase="--pilot-before",
                                           mem_kib=250 * 1024).stderr)
        self.assertIn("shared_vps_ram_below_192MiB",
                      self.mock_shared_vps(phase="--pilot-after",
                                           mem_kib=180 * 1024).stderr)
        self.assertEqual(self.mock_shared_vps(phase="--pilot-after",
                                              mem_kib=200 * 1024).returncode, 0)

    def test_pilot_requires_root_bootstrap_and_enforces_64mib_cap(self):
        bootstrap = self.text("deploy/bor/bootstrap-host.sh")
        activation = self.text("deploy/bor/activate-release.sh")
        unit = self.text("deploy/bor/bor-pilot.conf")
        main = self.text("src/main.rs")
        bot = self.text("src/bot.rs")
        self.assertIn('shared-vps-preflight.sh" --pilot-before', bootstrap)
        self.assertIn('systemctl is-active --quiet bor.service', bootstrap)
        self.assertIn('systemctl is-enabled --quiet bor.service', bootstrap)
        self.assertIn("install -o root -g root -m 0644", bootstrap)
        self.assertIn("/etc/bor/pilot-mode", bootstrap)
        self.assertIn("MemoryHigh=48M", unit)
        self.assertIn("MemoryMax=64M", unit)
        self.assertIn("CPUQuota=25%", unit)
        self.assertIn("Restart=no", unit)
        self.assertIn("BOR_PILOT_MODE=1", unit)
        self.assertIn("EnvironmentFile=/etc/bor/preflight.env", unit)
        self.assertIn('memory_max" == 67108864', activation)
        self.assertIn("pilot_cannot_autoactivate", activation)
        self.assertIn("--pilot-before", activation)
        self.assertIn("--pilot-after", activation)
        self.assertIn("BOR_PILOT_MODE", main)
        self.assertIn("TEST_CHAT_ID required when BOR_PILOT_MODE=1", main)
        self.assertIn("pilot ignored out-of-scope update", bot)
        self.assertIn("update_target_chat(&u) != Some(chat_id)", bot)

    def test_pr_release_artifact_is_pinned_to_branch_head(self):
        workflow = self.text(".github/workflows/bor-native-vps.yml")
        self.assertIn("github.event.pull_request.head.sha || github.sha", workflow)
        self.assertIn('SOURCE_SHA=$(git rev-parse HEAD)', workflow)
        self.assertIn("bor-native-release-${{ github.event.pull_request.head.sha || github.sha }}", workflow)
        self.assertIn("if: github.event_name == 'pull_request' ||", workflow)
        self.assertIn("if: github.ref == 'refs/heads/main' &&", workflow)

if __name__ == "__main__":
    unittest.main()
