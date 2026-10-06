# Holders and durability

Every pane's process runs under its own small **holder** process. Holders own the PTY (or a pipe, for headless adapters), keep a bounded output ring, and talk to the server over the holder protocol (`holder/1`). If the server crashes or is upgraded (`server.restart`), holders and their processes keep running and the next server reattaches. Input carries ids and acks so delivery is never duplicated.

Screen recovery after a server restart is best effort: the holder replays from its checkpoint and forces a redraw. Process survival is the hard guarantee; screen recovery is measured, not promised.
