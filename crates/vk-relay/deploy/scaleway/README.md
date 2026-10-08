# Deploy the relay on Scaleway

This deploys one relay VM. The marketing site stays on Vercel. You can also serve
the built web app from the relay's domain with `--app-dir`.

The default is a DEV1-S in Paris with 2 vCPUs, 2 GB RAM, a 20 GB boot disk,
and one public IPv4 address. Review current provider pricing before deploying.

## Set up

Install Python 3.11+, the [Scaleway CLI](https://www.scaleway.com/en/docs/scaleway-cli/),
OpenSSH, and the tools in the repository's `mise.toml`. Use `mise install` to install
the build tools. The script builds the relay for Linux x86_64 with `cargo zigbuild`.
It does not build or upload the source tree, dotenv files, or Scaleway credentials
to the VM.

Add the values from [env.example](env.example) to `.env.local`. The project UUID is
required so the script cannot select an unrelated project from a local CLI profile.
Use an SSH key that has both its private key and its `.pub` file on this machine.
The script adds the public key as an Instance-specific `AUTHORIZED_KEY` tag and
sets the initial SSH configuration through cloud-init. Scaleway regenerates
`authorized_keys` at boot, so the tag preserves access after a reboot. SSH
password login is off. See [Scaleway SSH key tags](https://www.scaleway.com/en/docs/instances/reference-content/add-instance-specific-ssh-keys-using-tags/).

The access key needs permission to read the project and manage Instances,
security groups, public IPs, and Block Storage in that project.

```sh
python3 scripts/deploy-scaleway.py plan
python3 scripts/deploy-scaleway.py check
python3 scripts/deploy-scaleway.py deploy
```

`plan` is offline and makes no changes. `check` reads the project and existing VM.
`deploy` builds the relay, creates missing resources, then installs the release.
If the VM type is unavailable, it stops. It does not select a more expensive type.

In a worktree, point to the original dotenv file. There is no need to copy it:

```sh
python3 scripts/deploy-scaleway.py plan --env-file /Users/demo/code/vibeke/.env.local
python3 scripts/deploy-scaleway.py deploy --env-file /Users/demo/code/vibeke/.env.local
```

Process environment variables override file values. The standard `SCW_ACCESS_KEY`,
`SCW_SECRET_KEY`, and `SCW_DEFAULT_PROJECT_ID` names also work. The parser supports
literal single-line values, quotes, comments, and `export`; it does not run shell
commands or expand `$VARIABLES`.

## DNS and verification

The script prints the new IPv4 address. Set an **A** record for `relay.vibeke.dev`
to that address at the current DNS provider. Remove any conflicting AAAA record
for this subdomain if it points elsewhere. Do not change the apex or `www` records.

Caddy obtains and renews the TLS certificate. The firewall accepts inbound TCP
80 and 443, plus SSH from `VIBEKE_SSH_CIDR`. The relay listens on localhost:8787.
Host registration uses the relay's existing open-registration mode; end-to-end
authentication and encryption still happen between the paired host and device.

The install checks the local relay health. It then checks public HTTPS and the
release ID served by Caddy, with up to two minutes for the first TLS certificate.
If DNS is pending, it reports an incomplete deployment
and returns a nonzero exit code. Set the record and rerun the same command.

The reusable connection check creates temporary test identities and exercises a
Noise IK handshake plus a 150 KB encrypted response through the public relay:

```sh
bun install --cwd web --frozen-lockfile
bun web/packages/core/scripts/check-relay.ts wss://relay.vibeke.dev
```

This does not test pairing through the app UI, a real terminal, or session transfer.
The gateway and its terminal processes run on the user's host; a relay
restart interrupts remote connections. It does not restart those processes.

## Updates

The script identifies its VM and firewall by exact name, project, zone, and a
management tag. It refuses to use a resource with the same name that it does not
own. Keep those settings stable. Changing the name or project creates a separate
deployment and can add charges.

To change the deployment SSH key, add its Instance-specific key tag and reboot
the VM during a maintenance window before using the new key with this script.
Existing keys are not removed automatically.

Each release contains checksums. An atomic symlink selects the active release.
A failed relay health check restores the previous release. Reinstalling the same
healthy release does not restart it. An update disconnects clients briefly; this
single-VM setup is not highly available.

State, bundles, and SSH host keys are saved under `dist/scaleway/<name>/`, which is
ignored by Git. SSH accepts a new host key on first contact and rejects a changed
key. Set `VIBEKE_KNOWN_HOSTS` to use an existing pinned file, especially in CI.

The script does not delete cloud resources. If a provider operation fails partway,
inspect the project for an unused VM, IP, volume, or firewall before retrying.
Release folders are retained on the VM for rollback; remove old folders when disk
space is low. Install OS security updates as part of VM maintenance.

## Optional web app

```sh
cd web
bun install --frozen-lockfile
bun run --cwd apps/pwa build
cd ..
python3 scripts/deploy-scaleway.py deploy --app-dir web/apps/pwa/dist
```

This serves the PWA at `https://relay.vibeke.dev/`. Reuse `--app-dir` on later
deployments if you want to keep serving it. The script always builds a complete
release; omitting this option creates a relay-only release.

## GitHub Actions later

Use the same script on a Linux runner. Install `scw`, Python, and the mise tools;
provide credentials as environment secrets and the project/domain as variables.
Write the deployment SSH key to a file with mode 600 and provide a pinned
`known_hosts` file. Restrict Actions concurrency to one deployment at a time.

You can build separately and pass a Linux x86_64 binary with `--binary`. Build with
the repository's locked dependencies and musl target. The deploy command checks
the executable architecture. It still validates the installed service on the VM.
No GitHub workflow is enabled by this change.

## Local checks

```sh
python3 -m unittest discover -s scripts/tests -p 'test_deploy_scaleway.py'
sh -n crates/vk-relay/deploy/scaleway/install.sh
shellcheck crates/vk-relay/deploy/scaleway/install.sh
```
