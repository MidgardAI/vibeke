# Security model

## Host execution

Agents in host mode use your user permissions. Pane tokens, scopes, rate limits, and audit records limit misuse through the Vibeke API.

Pane scope is a guardrail for cooperative or prompt-injected agents. It is not a security boundary. A process running as your user can escape it, for example by detaching from the pane, and it can access state files, holder sockets, or hook configuration outside the API. On Linux, Vibeke keeps detached descendants attributed to their pane. On macOS it cannot.

Only [isolated execution](#isolated-execution) is a boundary.

## Isolated execution

Supported sandbox, container, and VM providers restrict access to host resources. A broker provides the APIs permitted for a pane. The agent cannot access privileged Vibeke sockets or files through that boundary.

If the required isolation is unavailable, Vibeke refuses to start the process. It does not substitute host mode.

## Threats and controls

| Threat | Control |
| --- | --- |
| Other local users | Socket permissions, peer identity checks, and private directories. |
| Malicious repository content | Trust applies to a content digest. Repository policy cannot reduce existing restrictions. |
| Prompt injection through an agent | Pane scopes and rules that prevent agents from approving their own requests apply. Containment requires isolated execution. |
| Malicious plugins | Explicit trust, identity checks, broker access, and audit records. |
| Compromised remote hosts | Remote input cannot directly execute local commands. |
| Network and preview content | Authentication, connection policy, and isolated preview origins. |
| Release file changes | Minisign signatures, checksums, and reproducibility checks. |

Root access and malware with your user permissions outside Vibeke are outside this security boundary.

## Pane permissions

A pane token can read permitted state. It can send input to its own panes and panes that it created, unless a request is open there.

A pane token can start agents in those panes. It can create tasks and worktrees, and use permitted previews and browser sessions.

A pane's request to preview an unrelated local port asks you for confirmation.

A pane token cannot approve interactions, change focus, modify other workspaces, edit policy, install integrations, or stop the server.

See the [API reference](reference/api.md) for each method's scope.

## Data

Pane output, scrollback archives, and `state.db` use restrictive file permissions. Redaction filters remove recognized secrets from logs, audit records, assistance prompts, and debug bundles. Filters cannot identify every possible secret.

Push notifications are registered only with your own paired hosts, not shared hosts.

Telemetry is disabled.

## Releases

Release files have checksums and Minisign signatures. Sigstore provenance is not yet provided. Linux musl reproducibility checks use `scripts/repro-check.sh`.

See [release verification](reference/releases.md) for current results and limits.
