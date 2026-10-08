#!/usr/bin/env python3
"""Build and deploy one Vibeke relay VM. Uses Python's standard library and scw CLI."""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "crates/vk-relay/deploy/scaleway"
OWNER = "managed-by=vibeke-deploy"


def load_env(path, inherited):
    """Read literal KEY=value lines. Never execute shell code or expand variables."""
    values = {}
    if path:
        for number, line in enumerate(Path(path).read_text().splitlines(), 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            line = line.removeprefix("export ")
            key, sep, value = line.partition("=")
            key = key.strip()
            if not sep or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
                raise ValueError(f"Invalid environment assignment on line {number}")
            try:
                parts = shlex.split(value, comments=True)
            except ValueError:
                raise ValueError(f"Invalid quoted value on line {number}") from None
            if len(parts) > 1:
                raise ValueError(f"Quote values with spaces on line {number}")
            values[key] = parts[0] if parts else ""
    # Normalize aliases before merging so either CI spelling overrides either file spelling.
    aliases = {"SCW_ACCESS_KEY": "SCALEWAY_ACCESS_KEY", "SCW_SECRET_KEY": "SCALEWAY_SECRET_KEY",
               "SCW_DEFAULT_PROJECT_ID": "SCALEWAY_PROJECT_ID"}
    for alias, canonical in aliases.items():
        if alias in values and canonical not in values:
            values[canonical] = values[alias]
    inherited = dict(inherited)
    for alias, canonical in aliases.items():
        if alias in inherited and canonical not in inherited:
            inherited[canonical] = inherited[alias]
    values.update(inherited)  # GitHub Actions secrets take precedence over local files.
    return values


def owned_match(items, name):
    matches = [item for item in items if item["name"] == name]
    if len(matches) > 1:
        raise ValueError(f"Multiple resources named {name}; resolve the duplicate before deploying")
    if matches and OWNER not in matches[0].get("tags", []):
        raise ValueError(f"Refusing to change {name}: it was not created by this script")
    return matches[0] if matches else None


class Deployment:
    def __init__(self, args, env):
        self.args = args
        # Do not inherit another scw profile, endpoint, debug mode or organization.
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("SCW_", "SCALEWAY_"))}
        self.project = env.get("SCALEWAY_PROJECT_ID") or env.get("SCW_DEFAULT_PROJECT_ID", "")
        self.zone = env.get("SCALEWAY_ZONE", "fr-par-1")
        self.name = env.get("VIBEKE_DEPLOY_NAME", "vibeke-relay")
        self.domain = env.get("VIBEKE_RELAY_DOMAIN", "relay.vibeke.dev")
        self.ssh_cidr = str(ipaddress.IPv4Network(env.get("VIBEKE_SSH_CIDR", "0.0.0.0/0")))
        self.key = Path(env.get("VIBEKE_SSH_KEY", "~/.ssh/id_ed25519")).expanduser().resolve()
        self.known_hosts_file = env.get("VIBEKE_KNOWN_HOSTS")
        for key in ("ACCESS_KEY", "SECRET_KEY"):
            self.env[f"SCW_{key}"] = env.get(f"SCALEWAY_{key}") or env.get(f"SCW_{key}", "")
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,61}", self.name):
            raise ValueError("VIBEKE_DEPLOY_NAME must contain lowercase letters, digits and hyphens")
        labels = self.domain.split(".")
        if len(labels) < 2 or any(not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", p) for p in labels):
            raise ValueError("VIBEKE_RELAY_DOMAIN must be a plain DNS name")
        if self.zone not in ("fr-par-1", "fr-par-2", "fr-par-3", "nl-ams-1", "nl-ams-2", "nl-ams-3", "pl-waw-1", "pl-waw-2", "pl-waw-3"):
            raise ValueError("Unsupported SCALEWAY_ZONE")
        self.output = ROOT / "dist/scaleway" / self.name
        self.redactions = [self.env[k] for k in ("SCW_ACCESS_KEY", "SCW_SECRET_KEY") if self.env.get(k)]

    def redact(self, text):
        for value in self.redactions:
            text = text.replace(value, "[redacted]")
        return text

    def run(self, cmd, *, timeout=900, capture=True, ok_codes=(0,), **kwargs):
        result = subprocess.run(cmd, capture_output=capture, text=True, timeout=timeout, **kwargs)
        if result.returncode not in ok_codes:
            detail = self.redact((result.stderr or result.stdout or "").strip())
            raise RuntimeError(f"{Path(cmd[0]).name} failed ({result.returncode}): {detail}")
        return result.stdout or ""

    def scw(self, *args):
        # Credentials stay in the child environment, never in argv or a config file.
        return json.loads(self.run(["scw", "--config", os.devnull, "-o", "json", *args], env=self.env))

    def instance(self, *args):
        result = self.scw("instance", *args, f"zone={self.zone}")
        # CLI list commands return arrays; some create/get commands keep the API envelope.
        if isinstance(result, dict) and args[0] in ("server", "security-group"):
            return result.get(args[0].replace("-", "_"), result)
        return result

    def preflight(self):
        if not all(self.env.get(k) for k in ("SCW_ACCESS_KEY", "SCW_SECRET_KEY")):
            raise ValueError("Set SCALEWAY_ACCESS_KEY and SCALEWAY_SECRET_KEY")
        if not self.project:
            raise ValueError("Set SCALEWAY_PROJECT_ID to the intended project's UUID")
        uuid.UUID(self.project)
        # Bootstrap the organization from the selected project. The CLI requires an
        # organization even for this read, so use the account API before invoking it.
        request = urllib.request.Request(
            f"https://api.scaleway.com/account/v3/projects/{self.project}",
            headers={"X-Auth-Token": self.env["SCW_SECRET_KEY"]},
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            project = json.load(response)
        if project.get("id") != self.project or not project.get("organization_id"):
            raise ValueError("Account API returned an unexpected project")
        uuid.UUID(project["organization_id"])
        self.env["SCW_DEFAULT_ORGANIZATION_ID"] = project["organization_id"]
        self.env["SCW_DEFAULT_PROJECT_ID"] = self.project
        print(f"Project: {project['name']} ({project['id']}); zone: {self.zone}")
        servers = self.instance("server", "list", f"project-id={self.project}", f"name={self.name}")
        return owned_match(servers, self.name)

    def plan(self):
        print(json.dumps({
            "project": self.project or "REQUIRED: SCALEWAY_PROJECT_ID",
            "zone": self.zone, "name": self.name, "domain": self.domain,
            "instance": "DEV1-S", "image": "ubuntu_noble", "disk": "20 GB SBS 5K",
            "public_ip": "IPv4", "inbound_tcp": {"22": self.ssh_cidr, "80": "all", "443": "all"},
            "estimated_eur_30_days_ex_vat": 11.94,
            "database": "none; the current relay does not use a database",
            "app_dir": str(self.args.app_dir) if self.args.app_dir else None,
        }, indent=2))

    def security_group(self):
        name = f"{self.name}-firewall"
        groups = self.instance("security-group", "list", f"project-id={self.project}", f"name={name}")
        group = owned_match(groups, name)
        if group is None:
            group = self.instance("security-group", "create", f"project-id={self.project}",
                                  f"name={name}", f"tags.0={OWNER}", "stateful=true",
                                  "inbound-default-policy=drop", "outbound-default-policy=accept")
        if group.get("inbound_default_policy") != "drop" or not group.get("stateful"):
            raise ValueError("Managed firewall settings changed; inspect them before deploying")
        wanted = {(22, self.ssh_cidr), (80, "0.0.0.0/0"), (443, "0.0.0.0/0")}
        rules = self.instance("security-group", "list-rules", f"security-group-id={group['id']}")
        present = set()
        for rule in rules:
            if rule["direction"] != "inbound":
                continue
            pair = (rule.get("dest_port_from"), rule.get("ip_range"))
            if (pair not in wanted or rule["protocol"] != "TCP" or rule["action"] != "accept"
                    or rule.get("dest_port_to") not in (None, pair[0])):
                raise ValueError("Unexpected inbound firewall rule; inspect it before deploying")
            present.add(pair)
        for port, cidr in sorted(wanted - present):
            self.instance("security-group", "create-rule", f"security-group-id={group['id']}",
                          "protocol=TCP", "direction=inbound", "action=accept",
                          f"dest-port-from={port}", f"ip-range={cidr}")
        return group

    def artifact(self, staging):
        binary = self.args.binary
        if binary is None:
            build_env = os.environ.copy()
            build_env["CARGO_TARGET_DIR"] = str(self.output / "cargo")
            self.run(["mise", "exec", "--", "cargo", "zigbuild", "--release", "--locked",
                      "-p", "vk-relay", "--target", "x86_64-unknown-linux-musl"],
                     cwd=ROOT, env=build_env, capture=False, timeout=1800)
            binary = self.output / "cargo/x86_64-unknown-linux-musl/release/vibeke-relay"
        with Path(binary).open("rb") as source:
            header = source.read(20)
        if header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
            raise ValueError("--binary must be a Linux x86_64 ELF executable")
        shutil.copy2(binary, staging / "vibeke-relay")
        (staging / "vibeke-relay").chmod(0o755)
        if self.args.app_dir:
            if not (self.args.app_dir / "index.html").is_file():
                raise ValueError("--app-dir must contain the built PWA index.html")
            if any(p.is_symlink() for p in self.args.app_dir.rglob("*")):
                raise ValueError("--app-dir must not contain symlinks")
            shutil.copytree(self.args.app_dir, staging / "app")
        else:
            (staging / "app").mkdir()
        for filename in ("Caddyfile", "vibeke-relay.service"):
            template = (DEPLOY / filename).read_text()
            (staging / filename).write_text(template.replace("@DOMAIN@", self.domain))
        files = sorted(p for p in staging.rglob("*") if p.is_file())
        if any(any(c in str(p.relative_to(staging)) for c in "\n\r\\") for p in files):
            raise ValueError("Artifact filenames cannot contain newlines or backslashes")
        sums = "".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(staging)}\n" for p in files)
        (staging / "SHA256SUMS").write_text(sums)
        release = hashlib.sha256(sums.encode()).hexdigest()[:20]
        bundle = self.output / f"{release}.tar.gz"
        with tarfile.open(bundle, "w:gz") as archive:
            for path in staging.iterdir():
                archive.add(path, arcname=path.name)
        return release, bundle

    def provision(self, server, staging):
        public_key = Path(str(self.key) + ".pub").read_text().strip()
        if not re.fullmatch(r"(?:ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp\d+) [A-Za-z0-9+/=]+(?: [^\n]*)?", public_key):
            raise ValueError("Invalid SSH public key")
        key_tag = "AUTHORIZED_KEY=" + "_".join(public_key.split()[:2])
        group = self.security_group()
        if server:
            if server["commercial_type"] != "DEV1-S" or server["security_group"]["id"] != group["id"]:
                raise ValueError("Existing VM type or firewall differs from the deployment plan")
            if key_tag not in server.get("tags", []):
                raise ValueError("Managed VM lacks this SSH key tag; add the key tag and reboot before deploying")
        else:
            cloud_init = staging / "cloud-init.yaml"
            cloud_init.write_text("#cloud-config\n" + json.dumps({
                "ssh_pwauth": False, "disable_root": False,
                "users": [{"name": "root", "lock_passwd": True, "ssh_authorized_keys": [public_key]}],
            }))
            server = self.instance("server", "create", f"project-id={self.project}",
                                   f"name={self.name}", f"tags.0={OWNER}", f"tags.1={key_tag}", "type=DEV1-S",
                                   "image=ubuntu_noble", "root-volume=sbs:20GB:5000", "ip=ipv4",
                                   "stopped=true",
                                   f"security-group-id={group['id']}", f"cloud-init=@{cloud_init}")
        state_file = self.output / "state.json"
        state_file.write_text(json.dumps({"project": self.project, "zone": self.zone,
                                         "server_id": server["id"], "firewall_id": group["id"]}, indent=2))
        if server["state"] in ("stopped", "stopped in place"):
            self.instance("server", "start", server["id"])
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline:
            server = self.instance("server", "get", server["id"])
            if server["state"] == "running":
                addresses = server.get("public_ips") or [server.get("public_ip") or {}]
                for address in addresses:
                    if address.get("address") and ipaddress.ip_address(address["address"]).version == 4:
                        return server, address["address"]
            time.sleep(5)
        raise RuntimeError(f"VM not ready after 10 minutes. Resource IDs: {state_file}")

    def deploy(self):
        server = self.preflight()  # Authenticate and check ownership before builds or mutations.
        if not self.key.is_file() or not Path(str(self.key) + ".pub").is_file():
            raise ValueError("VIBEKE_SSH_KEY must have both a private key and a .pub file")
        self.output.mkdir(parents=True, exist_ok=True)
        self.output.chmod(0o700)
        with tempfile.TemporaryDirectory(prefix="bundle-", dir=self.output) as temp:
            staging = Path(temp)
            release, bundle = self.artifact(staging)
            server, address = self.provision(server, staging)
        print(f"VM: {server['id']} at {address}. DNS: A {self.domain} -> {address}", flush=True)
        known_hosts = Path(self.known_hosts_file or str(self.output / "known_hosts")).expanduser()
        strict = "yes" if self.known_hosts_file else "accept-new"
        ssh = ["ssh", "-i", str(self.key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
               "-o", "ConnectTimeout=10", "-o", f"StrictHostKeyChecking={strict}",
               "-o", f"UserKnownHostsFile={known_hosts}", f"root@{address}"]
        deadline = time.monotonic() + 300
        while True:
            try:
                self.run([*ssh, "true"], timeout=15)
                break
            except (RuntimeError, subprocess.TimeoutExpired):
                if time.monotonic() >= deadline:
                    raise RuntimeError("SSH did not become ready. Check the key, firewall and known_hosts file") from None
                time.sleep(5)
        self.wait_cloud_init(ssh)
        remote_bundle = f"/root/vibeke-{release}.tar.gz"
        with bundle.open("rb") as stream:
            self.run([*ssh, f"umask 077; cat > {remote_bundle}"], stdin=stream)
        installer = (DEPLOY / "install.sh").read_text()
        result = self.run([*ssh, "sh -s -- " + shlex.join([release, remote_bundle])], input=installer)
        print(result.strip().splitlines()[-1])
        self.verify_public(release, address)
        print(f"HTTPS verified: https://{self.domain}; release {release}")
        print("Full host/device pairing and session transfer still require an end-to-end check.")

    def verify_public(self, release, address):
        # On first boot Caddy may still be obtaining the certificate after it starts.
        deadline = time.monotonic() + 120
        while True:
            try:
                with urllib.request.urlopen(f"https://{self.domain}/healthz", timeout=10) as response:
                    if response.read().strip() != b"ok":
                        raise ValueError("Unexpected health response")
                with urllib.request.urlopen(f"https://{self.domain}/.well-known/vibeke-release", timeout=10) as response:
                    if response.read().decode().strip() != release:
                        raise ValueError("Public endpoint serves a different release")
                return
            except Exception as error:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"VM installed, but public HTTPS is not verified ({error}). "
                                       f"Check A {self.domain} -> {address} and Caddy's TLS logs, then rerun deploy") from None
                time.sleep(3)

    def wait_cloud_init(self, ssh):
        # cloud-init returns 2 for completed boots with recoverable provider warnings.
        report = json.loads(self.run([*ssh, "cloud-init status --wait --format=json"],
                                     timeout=600, ok_codes=(0, 2)))
        if report.get("status") != "done" or report.get("errors"):
            raise RuntimeError("cloud-init did not finish successfully; inspect cloud-init status --long")
        if report.get("recoverable_errors"):
            print("cloud-init completed with recoverable warnings; continuing with service health checks.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("plan", "check", "deploy"))
    parser.add_argument("--env-file", type=Path, help="Literal dotenv file; process environment overrides it")
    parser.add_argument("--binary", type=Path, help="Use a prebuilt Linux x86_64 relay instead of building")
    parser.add_argument("--app-dir", type=Path, help="Also serve a built PWA from this directory")
    args = parser.parse_args()
    env_file = args.env_file or (ROOT / ".env.local" if (ROOT / ".env.local").is_file() else None)
    deployment = None
    try:
        deployment = Deployment(args, load_env(env_file, os.environ))
        if args.command == "plan":
            deployment.plan()
        elif args.command == "check":
            server = deployment.preflight()
            print(f"Existing managed VM: {server['id']}" if server else "No managed VM exists; deploy will create one")
        else:
            deployment.deploy()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        message = str(error)
        print("deploy-scaleway: " + (deployment.redact(message) if deployment else message), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
