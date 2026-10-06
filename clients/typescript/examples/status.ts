// Connects to the session socket, prints `server.status` and `workspace.list`, subscribes to
// workspace events, creates a workspace and waits for its `workspace.created` event.
//   VIBEKE_SESSION=default node examples/status.ts
import { VibekeClient } from "../src/index.ts";

const c = await VibekeClient.connect({ client: "vibeke-client-ts-example" });
try {
  const status = await c.call("server.status", {});
  const list = await c.call("workspace.list", {});
  const events = await c.events({ types: ["workspace.*"] });
  const made = await c.call("workspace.create", { cwd: process.cwd(), name: "ts-client" });
  let created: unknown = null;
  for await (const e of events) {
    if (e.type === "workspace.created") {
      created = e;
      break;
    }
  }
  console.log(
    JSON.stringify({
      session: status.session,
      pid: status.pid,
      workspaces_before: list.workspaces.length,
      created_workspace: made.workspace.id,
      event: created,
    }),
  );
} finally {
  c.close();
}
