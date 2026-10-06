# Agents and interactions

Vibeke integrates with agent harnesses through structured channels: Claude Code hooks and transcript, the pi/omp extension, Codex's app server (via a PATH shim), with screen scraping only as a labelled fallback. Each run has an `AgentState` carrying its source and confidence.

An **interaction** is something an agent needs from you: a permission request, a question, a plan review. Interactions have a delivery state machine (decision, lease, native delivery, ack), survive server restarts, appear as cards in the inbox and can be answered without attaching to the pane. Agents can see but never answer their own interactions.

`vibeke integration install|status|uninstall|doctor` sets up hooks for Claude, pi/omp and Codex alongside any existing ones. `vibeke agent start|prompt|wait|read|send-keys|get|list` drives runs.
