# Vibeke

Vibeke is a terminal workspace for running coding agents. It understands agents structurally (hooks, extensions, RPC) instead of by reading their screens, keeps every process alive through server crashes and upgrades, isolates agents in task workspaces, and makes remote dev servers and previews feel local.

This book covers installing and using Vibeke. The design documents live in the repository's `spec/` directory and are authoritative when this book is silent.

The reference chapters (CLI, configuration, control API) are generated from the code and checked in CI, so they cannot drift from the binary. The control API string is `vibeke/1`; its freeze is a draft until 1.0 (see the [API reference](reference/api.md)).
