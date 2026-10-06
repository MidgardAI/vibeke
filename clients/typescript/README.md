# @vibeke/client

Typed client for the Vibeke control API (`vibeke/1`, spec 07) over the session's unix socket
(Node `net`). Types for every method's params and result, every event type and every error kind
are generated from the schema registry (`crates/vk-server/src/api_schema.rs`) into
`src/types.gen.ts`; `src/client.ts` is the hand-written runtime.

```ts
import { VibekeClient, isEvent } from "@vibeke/client";

const c = await VibekeClient.connect();            // $VIBEKE_SOCKET, or the session's runtime dir
const status = await c.call("server.status", {});  // typed result
const events = await c.events({ types: ["agent.*", "pane.created"] });
for await (const e of events) {                    // events are an async iterator
  if (isEvent(e, "agent.state_changed")) console.log(e.subject.run, e.data.to);
}
```

- Errors reject with `VibekeError` (`kind`, `code`, `details`, `retryable`).
- `events.overflow` ends the stream with `EventOverflow`; resubscribe with `{ after: err.resumeFrom }`.
- `connect({ token })` (default `$VIBEKE_PANE_TOKEN`) limits the connection to pane scope.
- Before connecting, `checkSocketTrust` refuses a socket another user could have planted (as the
  CLI does): under the runtime root every directory up to the root must be a real 0700 directory
  of yours (no symlinks), an explicit socket elsewhere needs a parent of yours that is not group-
  or world-writable, and the socket must be yours. A refused socket gets no connection and no
  byte (`SocketTrustError`); `connect({ insecure: true })` skips the check.
- `stream.close()` (or `break` in `for await`) sends `events.unsubscribe` and discards later
  events for it; pushes for unknown subscription ids are dropped, never buffered.
- Unknown event types and notifications are delivered or ignored, never an error (07 §1.5).
- Needs Node >= 22.18 (native type stripping), Bun, or any TS-aware bundler; no runtime dependencies.

Tests: `node --test "test/*.test.ts"`. Regenerate the types after a registry change with
`VIBEKE_UPDATE_CLIENTS=1 cargo test -p vibeke --test api_clients`.
