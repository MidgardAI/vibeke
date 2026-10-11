# Security model

## Host execution

Agents in host mode use your user permissions. Vibeke does not enforce policy in host mode. Pane tokens, scopes, rate limits, and audit records limit misuse through the Vibeke API. Rules and claims there are advice for cooperative agents.

Pane scope is a guardrail for cooperative or prompt-injected agents. It is not a security boundary. A process running as your user can escape it, for example by detaching from the pane, and it can access state files, holder sockets, or hook configuration outside the API. On Linux, Vibeke keeps detached descendants attributed to their pane. On macOS it cannot.

Only [isolated execution](#isolated-execution) is a boundary.

## Isolated execution

Supported sandbox and container providers restrict access to host resources. A broker provides the APIs permitted for a pane. The agent cannot access privileged Vibeke sockets or files through that boundary.

The `vm` level is experimental and not verified on every host. The `cloud` level is a preview. Cloud sandboxes run on another machine, and their tests use mocks for some providers. Do not rely on either level as your only protection.

The Linux sandbox needs Landlock ABI 6, which means Linux 6.12 or newer. On an older kernel, Vibeke refuses to start the task.

If the required isolation is unavailable, Vibeke refuses to start the process. It does not substitute host mode.

Vibeke runs setup commands and checks for a sandbox or container task inside that task's box. It refuses a container or cloud check when it cannot reach the box. Host checks drop credential variables unless the check names them. Review reads files and Git data. It does not run code from the repository.

## Threats and controls

| Threat | Control |
| --- | --- |
| Other local users | Socket permissions, peer identity checks, and private directories. |
| Malicious repository content | Trust applies to a content digest. Repository policy cannot reduce existing restrictions. |
| Prompt injection through an agent | Pane scopes and rules that prevent agents from approving their own requests apply. Containment requires isolated execution. |
| Malicious plugins | Explicit trust, identity checks, broker access, and audit records. |
| Compromised remote hosts | Remote input cannot directly execute local commands. |
| Network and preview content | Authentication, connection policy, and isolated preview origins. |
| Release file changes | Minisign signatures and checksums. |

Root access and malware with your user permissions outside Vibeke are outside this security boundary.

## Pane permissions

A pane token can read permitted state. It can send input to its own panes and panes that it created, unless a request is open there.

A pane token can start agents in those panes. It can create tasks and worktrees, and use permitted previews and browser sessions.

A pane's request to preview an unrelated local port asks you for confirmation.

A pane token cannot approve interactions, change focus, modify other workspaces, edit policy, install integrations, or stop the server.

See the [API reference](reference/api.md) for each method's scope.

## Remote access

Phones, browsers and the desktop app reach a host through the gateway.
Each paired device has one of three access levels:

| Level | What the device can do |
| --- | --- |
| Full | It can run any command as you on this host. It can type into panes, start agents and open previews. |
| Approve | It can approve agent requests, including "always allow". It can also send text to agents. This is close to full access. |
| View | It can read all pane output, transcripts, files and diffs. It cannot change anything. |

A share gives one of these levels for one pane or workspace, for a limited time.
When you revoke a device, Vibeke also revokes the shares, linked hosts and invitations that this device created.

### Pairing links

A pairing link holds a one-time secret.
The first device that opens the link can ask to pair.
Vibeke then shows a check code, and the device shows a check code too.
Pair only when both codes are the same.
A different code means that another party is in the middle.

Share links and invitations work without a check code.
Treat them like passwords, and send them only through a channel that you trust.
Vibeke hides pairing links in logs.

Inside a pane, `vibeke gateway pair`, `share`, `peer invite` and `peer add` ask you in Vibeke first.
An agent cannot hand out access without your approval.
Vibeke tells you each time a device pairs, a share is created, or someone opens an invitation.

### The relay

The relay forwards encrypted messages between devices and hosts. It cannot read them.
The relay can see host ids, IP addresses, connection times, message sizes and timing.
The relay admits a device only with a ticket that the host signed.

### The app origin

The browser app loads from its app origin. The app holds the device keys.
Whoever controls that origin can change the app and can read those keys.
Use an app origin that you trust.
If you serve the app yourself, set it with `vibeke gateway pair --app-url <origin>`.

### Previews from devices

A device with full access can open previews of your host.
Its preview streams reach only preview ports, the ports of a task, and ports that you allowed.
The app asks you before a preview reaches any other local port.

## Data

Pane output, scrollback archives, and `state.db` use restrictive file permissions. Redaction filters remove recognized secrets from logs, audit records, assistance prompts, and debug bundles. Filters cannot identify every possible secret.

Push notifications are registered only with your own paired hosts, not shared hosts.

Telemetry is disabled.

## Releases

Release files have checksums and Minisign signatures. Vibeke does not yet publish build provenance.

See [Verify a release](reference/releases.md) for the checks and their limits.
