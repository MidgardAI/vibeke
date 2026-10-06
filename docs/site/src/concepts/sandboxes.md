# Execution levels

Each task uses an execution level: `host`, `sandbox`, `container`, or `vm`. Provider availability depends on the host.

## Host execution

Host mode uses your user permissions. API tokens and scopes limit accidental misuse through Vibeke. They do not stop a process that bypasses the API.

Agents with unrestricted permissions show a `YOLO·HOST` indicator in host mode.

## Isolated execution

Supported sandbox, container, and VM providers limit access to host resources. The process cannot access privileged Vibeke sockets or state files.

A broker exposes only the APIs permitted for that pane. Network access, Git pushes, and credentials follow the isolation policy. Required approvals appear as interactions.

If the selected isolation provider is unavailable, Vibeke refuses to start the process. It does not start the process in host mode instead.

Use `vibeke sandbox status` to check available providers. See the [security model](../security.md) for limits.
