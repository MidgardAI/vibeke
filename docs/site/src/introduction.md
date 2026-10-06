# Vibeke

Vibeke is a terminal workspace for coding agents. It connects to agent hooks, extensions, and RPC interfaces. It can also detect agent state from terminal output.

Separate holder processes keep agents active after a server restart. Task worktrees separate file changes. Remote previews let you open an app from another host.

Use this guide to install and operate Vibeke. The `spec/` directory contains detailed design documents.

The build generates the CLI, configuration, and API references from the source. Automated checks detect differences. The control API is `vibeke/1`. Its compatibility rules can change before version 1.0. See the [API reference](reference/api.md).
