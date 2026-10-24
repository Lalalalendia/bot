"""BOR pilot GitHub Secrets deployment contracts, no network or real secrets."""
import unittest
from pathlib import Path

from tools.bor_find_test_group import unique_test_group, discover

ROOT = Path(__file__).resolve().parents[1]


class BotPilotSecretsTests(unittest.TestCase):
    def test_one_real_supergroup_with_duplicates(self):
        chat = -1001234567890
        events = [
            {"message": {"chat": {"id": 12, "type": "private"}}},
            {"my_chat_member": {"chat": {"id": chat, "type": "supergroup"}}},
            {"message": {"chat": {"id": chat, "type": "supergroup"}}},
        ]
        self.assertEqual(unique_test_group(events), chat)

    def test_missing_or_multiple_test_groups_do_not_guess(self):
        with self.assertRaisesRegex(ValueError, "No test supergroup"):
            unique_test_group([{"message": {"chat": {"id": -123, "type": "group"}}}])
        with self.assertRaisesRegex(ValueError, "Multiple test groups"):
            unique_test_group([
                {"message": {"chat": {"id": -10011111111, "type": "supergroup"}}},
                {"message": {"chat": {"id": -10022222222, "type": "supergroup"}}},
            ])

    def test_malformed_chat_ids_ignored(self):
        with self.assertRaisesRegex(ValueError, "No test supergroup"):
            unique_test_group([
                {},
                {"message": []},
                {"message": {"chat": {"id": True, "type": "supergroup"}}},
                {"message": {"chat": {"id": "-10011111111", "type": "supergroup"}}},
                {"message": {"chat": {"id": -1222, "type": "supergroup"}}},
            ])

    def test_missing_secret_is_safe(self):
        import os
        old = os.environ.pop("BOR_BOT_TOKEN", None)
        try:
            with self.assertRaisesRegex(ValueError, "BOR_BOT_TOKEN"):
                discover()
        finally:
            if old is not None:
                os.environ["BOR_BOT_TOKEN"] = old

    def test_branch_only_opt_in_and_existing_safe_installer(self):
        workflow = (ROOT / ".github/workflows/bor-pilot-provision.yml").read_text()
        self.assertIn("github.event_name == 'push'", workflow)
        self.assertIn("refs/heads/feat/bor-native-vps-release", workflow)
        self.assertIn("vars.BOR_PILOT_PROVISION_ENABLED == 'true'", workflow)
        self.assertIn("environment: bor-pilot", workflow)
        for secret in ("BOR_BOT_TOKEN", "BOR_DEPLOY_HOST",
                       "BOR_DEPLOY_SSH_KEY", "BOR_DEPLOY_KNOWN_HOSTS"):
            self.assertIn("secrets." + secret, workflow)
        self.assertIn("secrets.BOR_TEST_CHAT_ID", workflow)
        self.assertIn("StrictHostKeyChecking=yes", workflow)
        self.assertIn("bor-shared-vps-preflight --pilot-before", workflow)
        self.assertIn("26e3513fb4269890f135479af41e7df048e380b9", workflow)
        self.assertIn("sudo -n /usr/local/sbin/bor-install-credentials", workflow)
        self.assertIn("bor-activate-release", workflow)
        self.assertIn("67108864", workflow)
        self.assertIn("bor_service=ACTIVE", workflow)
        self.assertNotIn("set -x", workflow)
        self.assertNotIn("deleteWebhook", workflow)
        self.assertNotIn("systemctl restart chaptera-", workflow)


if __name__ == "__main__":
    unittest.main()
