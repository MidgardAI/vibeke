# Sandboxes and execution levels

Each task runs at an **execution level**: `host`, `sandbox` (OS sandbox), `container` or `vm`. Host mode offers cooperative guardrails only: agents run as you, and Vibeke's tokens, scopes and self-answer rules stop accidents and casual misuse, not code that deliberately bypasses the API. Contained levels enforce a boundary: the process tree cannot reach Vibeke's sockets or state, and its only Vibeke endpoint is a brokered, pane-scoped socket. Egress, push and credentials are gated at the boundary and surface as interactions.

Host runs of "yolo" agents carry a `YOLO·HOST` badge. See the [security model](../security.md).
