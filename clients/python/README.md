# vibeke-client

Typed, standard-library-only client for the Vibeke control API (`vibeke/1`, spec 07) over the
session's unix socket (asyncio, Python 3.11+). `vibeke_client/types_gen.py` (TypedDicts for every
method's params and result, every event payload and the error kinds) and `api_gen.py` (one typed
coroutine per method) are generated from the schema registry
(`crates/vk-server/src/api_schema.rs`); `client.py` is the hand-written runtime.

```python
import asyncio
from vibeke_client import Client

async def main():
    async with await Client.connect() as c:         # $VIBEKE_SOCKET, or the session's runtime dir
        print((await c.server_status({}))["pid"])   # pane.split -> c.pane_split({...})
        events = await c.events(types=["agent.*"])
        async for ev in events:                     # ev.type, ev.subject, ev.data
            print(ev.type, ev.data)

asyncio.run(main())
```

- Errors raise `VibekeError` (`kind`, `code`, `details`, `retryable`); `c.call(method, params)` is the untyped form.
- `events.overflow` raises `EventOverflow` from the stream; resubscribe with `after=err.resume_from`.
- `Client.connect(token=...)` (default `$VIBEKE_PANE_TOKEN`) limits the connection to pane scope.
- Before connecting, `check_socket_trust` refuses a socket another user could have planted (as
  the CLI does): under the runtime root every directory up to the root must be a real 0700
  directory of yours (no symlinks), an explicit socket elsewhere needs a parent of yours that is
  not group- or world-writable, and the socket must be yours. A refused socket gets no connection
  and no byte (`SocketTrustError`); `Client.connect(insecure=True)` skips the check.
- `await stream.close()` sends `events.unsubscribe` and discards later events for it; pushes for
  unknown subscription ids are dropped, never buffered.
- Unknown event types and notifications are ignored, never an error (07 §1.5).

Tests: `python3 -m unittest discover -s tests`. Regenerate after a registry change with
`VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test api_clients`.
