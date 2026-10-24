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


if __name__ == "__main__":
    unittest.main()
