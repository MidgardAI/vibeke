# Holders and process recovery

Each pane has a separate **holder process**. The holder owns the pseudo-terminal (PTY) or pipe for the agent. It stores a limited amount of recent output.

If the server stops, the holder keeps the pane process active. A new server reconnects through the `holder/1` protocol. Input IDs and acknowledgments prevent duplicate input.

## Recovery limits

After a server restart, the holder replays output from its checkpoint. The server then requests a screen redraw. Screen recovery can be incomplete.

The holder preserves processes through a server crash, server restart, or client disconnect. A host restart or holder failure stops those processes.
