# Security model

This is a summary of spec 09, which is authoritative.

## Two promises

- **Host execution: cooperative guardrails.** Agents run as you. Pane tokens, scopes, the no-self-answer rule, rate limits and the audit log stop accidental and casual prompt-injected misuse that goes through the `vibeke` API. They do not stop code running as the same user from reading state files, connecting to holder sockets or editing hook configs. Vibeke never calls host mode contained.
- **Sandbox, container and VM: enforced containment.** The agent cannot reach privileged sockets or files; the only endpoint is a brokered, pane-scoped socket that exposes the pane capability set and cannot be upgraded. Spawns whose containment is unavailable fail instead of falling back to the host.

## Adversaries in scope

Other local users (socket permissions and peer credential checks, 0700 directories), malicious repository content (repo-local config and policy are trusted per content digest and never loosen policy), prompt-injected agents (the main novel threat), compromised plugins (consent, identity-bound brokers, audit), compromised remote machines (remote is untrusted input), network attackers, malicious web content in previews, and the supply chain (checksums, signing, reproducible builds, dependency policy). Root or same-user malware outside Vibeke is out of scope.

## Pane-scope capabilities

A pane token can read, write to its own panes and the panes it created (unless an interaction is open there), start agents there, create tasks and worktrees, and use previews and the browser. It cannot answer interactions, move focus, change other workspaces, edit policy, install integrations, or stop the server. The exact per-method scope is listed in the [API reference](reference/api.md).

## Data

Pane contents, scrollback archives and `state.db` are sensitive and stored with restrictive permissions. Secrets are redacted before they reach logs, the audit trail or assistance prompts. Telemetry is off. Debug bundles are redacted before sharing.

## Releases

Artifacts are checksummed; signing with minisign and Sigstore provenance is planned (see [Releases](reference/releases.md)). Linux musl builds are checked for reproducibility with `scripts/repro-check.sh`.
