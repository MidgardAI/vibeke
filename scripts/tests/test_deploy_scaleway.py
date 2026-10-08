"""Exercise deployment boundaries without credentials or cloud mutations."""
import argparse
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("deploy", Path(__file__).parents[1] / "deploy-scaleway.py")
deploy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(deploy)
PROJECT = "12345678-1234-1234-1234-123456789012"


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        args = argparse.Namespace(binary=None, app_dir=None)
        self.deployment = deploy.Deployment(args, {
            "SCALEWAY_PROJECT_ID": PROJECT,
            "SCALEWAY_ACCESS_KEY": "dummy-access",
            "SCALEWAY_SECRET_KEY": "dummy-secret",
        })

    def envfile(self, text):
        path = self.root / ".env.local"
        path.write_text(text)
        return path

    def test_dotenv_is_literal_and_environment_overrides(self):
        marker = self.root / "must-not-exist"
        path = self.envfile(f"export EXAMPLE='$(touch {marker})'\nVALUE=local # note\nEMPTY=\n")
        result = deploy.load_env(path, {"VALUE": "CI"})
        self.assertEqual(result["EXAMPLE"], f"$(touch {marker})")
        self.assertFalse(marker.exists())
        self.assertEqual(result["VALUE"], "CI")
        self.assertEqual(result["EMPTY"], "")

    def test_ci_alias_overrides_local_scaleway_name(self):
        path = self.envfile("SCALEWAY_SECRET_KEY=local-secret\n")
        result = deploy.load_env(path, {"SCW_SECRET_KEY": "ci-secret"})
        self.assertEqual(result["SCALEWAY_SECRET_KEY"], "ci-secret")

    def test_bad_dotenv_does_not_echo_secret_line(self):
        path = self.envfile("SECRET=\"sensitive-but-unclosed\n")
        with self.assertRaisesRegex(ValueError, "line 1") as error:
            deploy.load_env(path, {})
        self.assertNotIn("sensitive", str(error.exception))

    def test_plan_has_no_network_or_secrets(self):
        output = io.StringIO()
        with patch.object(self.deployment, "run", side_effect=AssertionError("No commands allowed")):
            with contextlib.redirect_stdout(output):
                self.deployment.plan()
        plan = json.loads(output.getvalue())
        self.assertEqual(plan["estimated_eur_30_days_ex_vat"], 11.94)
        self.assertNotIn("dummy-secret", output.getvalue())

    def test_scw_credentials_use_environment_not_argv(self):
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "[]", "")) as run:
            self.deployment.scw("account", "project", "list")
        self.assertNotIn("dummy-secret", str(run.call_args.args))
        self.assertEqual(run.call_args.kwargs["env"]["SCW_SECRET_KEY"], "dummy-secret")

    def test_subprocess_error_redacts_credentials(self):
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, "", "dummy-secret dummy-access")):
            with self.assertRaises(RuntimeError) as error:
                self.deployment.scw("account", "project", "list")
        self.assertNotIn("dummy", str(error.exception))

    def test_missing_project_fails_before_any_cli_call(self):
        self.deployment.project = ""
        self.deployment.scw = Mock()
        with self.assertRaisesRegex(ValueError, "SCALEWAY_PROJECT_ID"):
            self.deployment.preflight()
        self.deployment.scw.assert_not_called()

    def test_preflight_derives_organization_from_selected_project(self):
        project = {"id": PROJECT, "organization_id": "87654321-1234-1234-1234-123456789012", "name": "Vibeke"}
        response = io.BytesIO(json.dumps(project).encode())
        self.deployment.instance = Mock(return_value=[])
        with patch("urllib.request.urlopen", return_value=response) as http:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertIsNone(self.deployment.preflight())
        self.assertEqual(http.call_args.args[0].full_url, f"https://api.scaleway.com/account/v3/projects/{PROJECT}")
        self.assertEqual(self.deployment.env["SCW_DEFAULT_ORGANIZATION_ID"], project["organization_id"])
        self.deployment.instance.assert_called_once_with("server", "list", f"project-id={PROJECT}", "name=vibeke-relay")

    def test_failed_preflight_cannot_build_or_create(self):
        self.deployment.preflight = Mock(side_effect=RuntimeError("DNS failed"))
        self.deployment.artifact = Mock()
        self.deployment.provision = Mock()
        with self.assertRaisesRegex(RuntimeError, "DNS failed"):
            self.deployment.deploy()
        self.deployment.artifact.assert_not_called()
        self.deployment.provision.assert_not_called()

    def test_foreign_or_duplicate_resource_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "not created"):
            deploy.owned_match([{"name": "relay", "tags": []}], "relay")
        owned = {"name": "relay", "tags": [deploy.OWNER]}
        with self.assertRaisesRegex(ValueError, "Multiple"):
            deploy.owned_match([owned, owned], "relay")
        self.assertIsNone(deploy.owned_match([owned], "relay-staging"))

    def firewall_fixture(self):
        group = {"id": "firewall-id", "name": "vibeke-relay-firewall", "tags": [deploy.OWNER],
                 "stateful": True, "inbound_default_policy": "drop"}
        rules = [{"direction": "inbound", "protocol": "TCP", "action": "accept", "ip_range": "0.0.0.0/0",
                  "dest_port_from": port, "dest_port_to": None} for port in (22, 80, 443)]
        return group, rules

    def test_repeated_firewall_run_is_read_only(self):
        group, rules = self.firewall_fixture()
        self.deployment.instance = Mock(side_effect=[[group], rules])
        self.assertEqual(self.deployment.security_group()["id"], "firewall-id")
        self.assertEqual([call.args[:2] for call in self.deployment.instance.call_args_list],
                         [("security-group", "list"), ("security-group", "list-rules")])

    def test_partial_firewall_creation_adds_only_missing_rule(self):
        group, rules = self.firewall_fixture()
        self.deployment.instance = Mock(side_effect=[[group], rules[:2], {}])
        self.deployment.security_group()
        call = self.deployment.instance.call_args_list[-1].args
        self.assertEqual(call[:2], ("security-group", "create-rule"))
        self.assertIn("dest-port-from=443", call)

    def test_unexpected_firewall_rule_is_not_silently_reused(self):
        group, rules = self.firewall_fixture()
        rules[0]["dest_port_from"] = 8787
        self.deployment.instance = Mock(side_effect=[[group], rules])
        with self.assertRaisesRegex(ValueError, "Unexpected inbound"):
            self.deployment.security_group()
        self.assertEqual(self.deployment.instance.call_count, 2)

    def test_domain_shell_metacharacters_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "plain DNS"):
            deploy.Deployment(self.deployment.args, {"VIBEKE_RELAY_DOMAIN": "example.com;whoami"})

    def make_artifact(self, app_text="first"):
        binary = self.root / "relay"
        binary.write_bytes(b"\x7fELF\x02\x01" + bytes(12) + b"\x3e\x00" + b"test-only")
        app = self.root / "app"
        app.mkdir(exist_ok=True)
        (app / "index.html").write_text(app_text)
        self.deployment.args.binary = binary
        self.deployment.args.app_dir = app
        self.deployment.output = self.root / "output"
        self.deployment.output.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=self.root) as temp:
            return self.deployment.artifact(Path(temp))

    def test_release_id_covers_app_and_bundle_excludes_credentials(self):
        first, bundle = self.make_artifact()
        again, _ = self.make_artifact()
        self.assertEqual(first, again)
        with tarfile.open(bundle) as archive:
            names = archive.getnames()
            self.assertEqual(set(names), {"vibeke-relay", "app", "app/index.html", "Caddyfile",
                                          "vibeke-relay.service", "SHA256SUMS"})
            self.assertIn(b"https://relay.vibeke.dev", archive.extractfile("vibeke-relay.service").read())
        changed, _ = self.make_artifact("changed")
        self.assertNotEqual(first, changed)

    def test_wrong_binary_architecture_is_rejected(self):
        binary = self.root / "mac-binary"
        binary.write_bytes(b"\xcf\xfa\xed\xfe" + bytes(20))
        self.deployment.args.binary = binary
        with self.assertRaisesRegex(ValueError, "Linux x86_64"):
            self.deployment.artifact(self.root)

    def provision_fixture(self):
        self.deployment.key = self.root / "id_ed25519"
        Path(str(self.deployment.key) + ".pub").write_text("ssh-ed25519 AAAATEST test")
        self.deployment.output = self.root
        self.deployment.security_group = Mock(return_value={"id": "firewall-id"})
        return {"id": "vm-id", "state": "running", "commercial_type": "DEV1-S",
                "tags": [deploy.OWNER, "AUTHORIZED_KEY=ssh-ed25519_AAAATEST"],
                "security_group": {"id": "firewall-id"}, "public_ips": [{"address": "192.0.2.1"}]}

    def test_new_vm_uses_selected_project_and_never_uploads_api_key(self):
        server = self.provision_fixture()
        self.deployment.instance = Mock(side_effect=[server, server])
        result, address = self.deployment.provision(None, self.root)
        self.assertEqual(address, "192.0.2.1")
        create = self.deployment.instance.call_args_list[0].args
        self.assertEqual(create[:2], ("server", "create"))
        self.assertIn(f"project-id={PROJECT}", create)
        self.assertIn("root-volume=sbs:20GB:5000", create)
        self.assertIn(f"tags.0={deploy.OWNER}", create)
        self.assertIn("tags.1=AUTHORIZED_KEY=ssh-ed25519_AAAATEST", create)
        self.assertIn("stopped=true", create)
        cloud_init = (self.root / "cloud-init.yaml").read_text()
        self.assertIn("ssh-ed25519 AAAATEST", cloud_init)
        self.assertNotIn("dummy-secret", cloud_init)

    def test_existing_vm_is_reused_without_creation_or_restart(self):
        server = self.provision_fixture()
        self.deployment.instance = Mock(return_value=server)
        self.deployment.provision(server, self.root)
        self.deployment.instance.assert_called_once_with("server", "get", "vm-id")

    def test_invalid_ssh_key_fails_before_resource_mutations(self):
        self.provision_fixture()
        Path(str(self.deployment.key) + ".pub").write_text("bad-key")
        with self.assertRaisesRegex(ValueError, "Invalid SSH public key"):
            self.deployment.provision(None, self.root)
        self.deployment.security_group.assert_not_called()

    def test_foreign_cli_configuration_is_not_inherited(self):
        with patch.dict("os.environ", {"SCW_DEBUG": "1", "SCW_API_URL": "https://other.invalid"}):
            config = deploy.Deployment(self.deployment.args, {"SCALEWAY_SECRET_KEY": "dummy-secret"})
        self.assertNotIn("SCW_API_URL", config.env)
        self.assertNotIn("SCW_DEBUG", config.env)
        self.assertEqual(config.env["SCW_SECRET_KEY"], "dummy-secret")

    def test_cli_resource_envelopes_are_unwrapped(self):
        self.deployment.scw = Mock(side_effect=[{"security_group": {"id": "firewall-id"}},
                                               {"server": {"id": "vm-id"}}, {"id": "vm-id"}])
        self.assertEqual(self.deployment.instance("security-group", "create"), {"id": "firewall-id"})
        self.assertEqual(self.deployment.instance("server", "get"), {"id": "vm-id"})
        self.assertEqual(self.deployment.instance("server", "create"), {"id": "vm-id"})

    def test_cloud_init_allows_completed_boot_with_provider_warning(self):
        report = json.dumps({"status": "done", "errors": [], "recoverable_errors": {"WARNING": ["vendor-data unavailable"]}})
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 2, report, "")):
            with contextlib.redirect_stdout(io.StringIO()):
                self.deployment.wait_cloud_init(["ssh", "test-host"])

    def test_cloud_init_fatal_error_blocks_install(self):
        self.deployment.run = Mock(return_value=json.dumps({"status": "done", "errors": ["setup failed"]}))
        with self.assertRaisesRegex(RuntimeError, "did not finish"):
            self.deployment.wait_cloud_init(["ssh", "test-host"])

    def test_public_check_waits_for_first_certificate(self):
        replies = [OSError("certificate not ready"), io.BytesIO(b"ok"), io.BytesIO(b"release-id\n")]
        with patch("urllib.request.urlopen", side_effect=replies), patch("time.sleep") as sleep:
            self.deployment.verify_public("release-id", "192.0.2.1")
        sleep.assert_called_once_with(3)

    def test_public_check_rejects_wrong_release(self):
        replies = [io.BytesIO(b"ok"), io.BytesIO(b"old-release")]
        with patch("urllib.request.urlopen", side_effect=replies), patch("time.monotonic", side_effect=[0, 121]):
            with self.assertRaisesRegex(RuntimeError, "different release"):
                self.deployment.verify_public("release-id", "192.0.2.1")


if __name__ == "__main__":
    unittest.main()
