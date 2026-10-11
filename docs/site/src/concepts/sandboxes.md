# Execution levels

Each task uses an execution level: `host`, `sandbox`, `container`, `vm` or `cloud`. Provider availability depends on the host.

| Level | What it is | Status |
| --- | --- | --- |
| `host` | The agent runs as your user. | No isolation. |
| `sandbox` | The operating system limits the agent: Seatbelt on macOS, Landlock on Linux. | Supported. |
| `container` | The agent runs in a container. | Supported. |
| `vm` | The agent runs in a virtual machine. | Experimental. Not verified on every host. |
| `cloud` | The agent runs on a machine of a cloud provider. | Preview. See [Cloud sandboxes](../cloud-sandboxes.md). |

## Host execution

Host mode uses your user permissions. Vibeke does not enforce any policy there. Rules, scopes and claims in host mode are advice for cooperative agents. A process can ignore them.

API tokens and scopes limit accidental misuse through Vibeke. They do not stop a process that bypasses the API.

Agents with unrestricted permissions show a `YOLO·HOST` indicator in host mode.

## Isolated execution

Supported sandbox and container providers limit access to host resources. The process cannot access privileged Vibeke sockets or state files. Vibeke does not yet verify the `vm` level on every host. Use it only to try it out.

A broker exposes only the APIs permitted for that pane. Network access, Git pushes, and credentials follow the isolation policy. Required approvals appear as interactions.

If the selected isolation provider is unavailable, Vibeke refuses to start the process. It does not start the process in host mode instead.

Use `vibeke sandbox status` to check available providers. `vibeke doctor` reports the same checks.

### Linux needs a recent kernel

The Linux sandbox uses Landlock ABI 6. This needs Linux 6.12 or newer. With an older kernel, Vibeke does not start sandbox or contained tasks. It shows an error that names the kernel requirement. It does not run the task with weaker limits.

### Commits in the Linux sandbox

An agent in a Linux sandbox cannot commit. The Linux sandbox can only make whole folders writable. The folder that holds the task branch also holds your other branches. Vibeke keeps that folder read-only. When the agent tries to commit, Git stops and explains this. The changes stay in the working tree. You can commit them on the host. On macOS, an agent in a sandbox can commit to its task branch.

### What the sandbox hides

- Each box has its own private Git object store. The agent cannot change objects that your checkout uses. Vibeke imports the objects when it reviews or merges the work.
- Credential files next to tool configuration are hidden from the box.
- On macOS, the sandbox may use only the ports that the task leased. It cannot reach services on the loopback address of your computer. To allow this, set `allow_host_loopback = true` in the `[isolation.sandbox]` section of your configuration.

## Where Vibeke runs jobs for a task

Vibeke sends work into the task's own box. You cannot choose another existing box.

| Job | Where it runs |
| --- | --- |
| Setup commands | Inside the task's box, for sandbox and container tasks. |
| Checks | Inside the task's box. A check for a container or cloud task is refused if the box is not available. It does not run on the host instead. |
| Review | Vibeke reads files and Git data. It does not run code from the repository. |
| Host checks | Host tasks run checks on the host. Vibeke removes credential variables unless the check names them. |

See the [security model](../security.md) for limits.
