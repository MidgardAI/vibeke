// Vibeke OpenCode plugin (spec 04 §6.4). Installed by `vibeke integration install opencode` to
// ~/.config/opencode/plugins/vibeke.ts. Inert outside a Vibeke pane (needs VIBEKE=1,
// VIBEKE_SOCKET and VIBEKE_PANE_TOKEN).
//
// Every forwarded event runs `$VIBEKE_BIN hook opencode <event>` with the event JSON on stdin,
// the same shim Claude/Codex/Gemini hooks use (fail-open, always exit 0). The server maps events
// in crates/vk-server/src/agents/opencode.rs.
//
// Approvals: OpenCode 1.x publishes a `permission.asked` event and shows its own dialog. The
// plugin asks the shim for a decision without blocking the event bus. A reply of
// `{"status": "allow" | "always" | "deny"}` answers the request through OpenCode's server API;
// no reply leaves the dialog to the user. A reply in OpenCode (`permission.replied`) stops the
// pending shim. Older versions call the `permission.ask` hook instead, answered the same way.
//
// Ride-along messages: `vibeke message take` hands over the messages other panes queued for this
// pane. The plugin calls it at turn start (`chat.message`), after a tool call
// (`tool.execute.after`) and when the session goes idle (`session.idle`), so the agent reads
// them without anything typed into its input box. Any failure returns nothing.
// Verified against OpenCode 1.19.0.
import { spawn } from "node:child_process";

const BIN = process.env.VIBEKE_BIN || "vibeke";
// `serve`/`web` host sessions for other clients, and `acp` reports through Vibeke's ACP host.
const SUBCOMMAND = process.argv.slice(2).find((a) => !a.startsWith("-"));
const ON =
  process.env.VIBEKE === "1" &&
  !!process.env.VIBEKE_SOCKET &&
  !!process.env.VIBEKE_PANE_TOKEN &&
  !["serve", "web", "acp"].includes(SUBCOMMAND ?? "");

// Bus events worth a process spawn (message.part.updated fires per token: never forwarded).
const FORWARD = new Set([
  "session.created",
  "session.status",
  "session.idle",
  "session.error",
  "session.compacted",
  "permission.replied",
  "file.edited",
  "message.updated",
]);

// Permission request id → the shim waiting for its decision.
const pending = new Map<string, any>();
const asked = new Set<string>();
// Assistant message id → its latest text part, sent with the completed message.
const texts = new Map<string, string>();

function send(event: string, payload: unknown, wait = false, id?: string): Promise<any> {
  if (!ON) return Promise.resolve(null);
  return new Promise((resolve) => {
    let out = "";
    let child: any;
    try {
      child = spawn(BIN, ["hook", "opencode", event], {
        stdio: ["pipe", wait ? "pipe" : "ignore", "ignore"],
      });
    } catch {
      return resolve(null);
    }
    if (id) pending.set(id, child);
    child.on("error", () => resolve(null));
    if (wait) child.stdout.on("data", (d: any) => (out += d));
    child.on("close", () => {
      if (id) pending.delete(id);
      if (!wait) return;
      try {
        resolve(out.trim() ? JSON.parse(out) : null);
      } catch {
        resolve(null);
      }
    });
    try {
      child.stdin.end(JSON.stringify(payload ?? {}));
    } catch {
      resolve(null);
    }
    if (!wait) {
      child.unref?.();
      resolve(null);
    }
  });
}

// `vibeke message take --json`: the queued messages of this pane as one text, or null. One call
// at a time: a caller during it gets null, because the call in flight hands the messages over
// once. An error or a timeout (3 s) gives null and never throws.
let taking: Promise<string | null> | null = null;
function take(): Promise<string | null> {
  if (!ON) return Promise.resolve(null);
  if (taking) return Promise.resolve(null);
  taking = new Promise<string | null>((resolve) => {
    let out = "";
    let done = false;
    let child: any;
    const finish = (v: string | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      try {
        child?.kill?.();
      } catch {}
      resolve(v);
    };
    const timer = setTimeout(() => finish(null), 3000);
    try {
      child = spawn(BIN, ["message", "take", "--json"], { stdio: ["ignore", "pipe", "ignore"] });
    } catch {
      return finish(null);
    }
    child.on("error", () => finish(null));
    child.stdout.on("data", (d: any) => (out += d));
    child.on("close", () => {
      try {
        const c = JSON.parse(out.trim())?.context;
        finish(typeof c === "string" && c.trim() ? c : null);
      } catch {
        finish(null);
      }
    });
  }).finally(() => {
    taking = null;
  });
  return taking;
}

const RESPONSE: Record<string, string> = { allow: "once", always: "always", deny: "reject" };

// Subagent sessions (`parentID` set): their prompts, tool calls and idle events never take the
// pane's messages, which belong to the main session.
const children = new Set<string>();
const isChild = (sessionID: unknown) => typeof sessionID === "string" && children.has(sessionID);

export const VibekePlugin = async ({ client, directory }: { client?: any; directory?: string }) => {
  if (!ON) return {};

  async function gate(p: any) {
    if (!p?.id || asked.has(p.id)) return;
    asked.add(p.id);
    const r = await send("permission.asked", p, true, p.id);
    const response = RESPONSE[r?.status];
    if (!response || !p.sessionID) return;
    let ok = false;
    try {
      const res = await client?.postSessionIdPermissionsPermissionId?.({
        path: { id: p.sessionID, permissionID: p.id },
        body: { response },
      });
      ok = !!res && !res.error;
    } catch {}
    // The shim already confirmed delivery: report that OpenCode refused the answer.
    if (!ok) await send("permission.reply_failed", { id: p.id, sessionID: p.sessionID });
  }

  return {
    event: async ({ event }: { event: any }) => {
      const type = event?.type;
      const props = event?.properties ?? {};
      if (type === "permission.asked") {
        // Not awaited: the decision may take minutes and the bus must keep flowing.
        void gate(props);
        return;
      }
      if (type === "message.part.updated") {
        // Per token: remembered, never forwarded.
        const part = props.part;
        if (part?.type === "text" && part.messageID && typeof part.text === "string") {
          texts.set(part.messageID, part.text);
          // User messages never complete as assistant messages: keep only recent ones.
          if (texts.size > 32) texts.delete(texts.keys().next().value);
        }
        return;
      }
      if (type === "session.created" || type === "session.updated") {
        const info = props.info ?? {};
        if (info.id && info.parentID) children.add(info.id);
      }
      if (!type || !FORWARD.has(type)) return;
      if (type === "permission.replied") {
        const id = props.requestID ?? props.permissionID;
        pending.get(id)?.kill?.();
        pending.delete(id);
      }
      if (type === "message.updated") {
        // Usage only: completed assistant messages.
        const info = props.info ?? {};
        if (info.role !== "assistant" || !info.time?.completed) return;
        const text = texts.get(info.id);
        texts.delete(info.id);
        await send(type, { ...props, text });
        return;
      }
      // Forwarded first and not delayed by the take below.
      const forwarded = send(type, { ...props, directory });
      if (type === "session.idle") {
        const sessionID = props.sessionID;
        // Taking marks the messages delivered: only take when they can be handed over.
        const context =
          sessionID && !isChild(sessionID) && client?.session?.promptAsync ? await take() : null;
        if (context) {
          try {
            // A new turn: the agent reads the messages without typing into the input box.
            await client.session.promptAsync({
              path: { id: sessionID },
              body: { parts: [{ type: "text", text: context }] },
            });
          } catch {}
        }
      }
      await forwarded;
    },
    "chat.message": async (input: any, output: any) => {
      const text = (output?.parts ?? [])
        .filter((p: any) => p?.type === "text")
        .map((p: any) => p.text)
        .join("\n");
      const sent = send("chat.message", { sessionID: input?.sessionID, text });
      try {
        const context =
          !isChild(input?.sessionID) && Array.isArray(output?.parts) && output?.message?.id
            ? await take()
            : null;
        if (context) {
          output.parts.push({
            id: `prt_vibeke_${Date.now().toString(36)}`,
            sessionID: input?.sessionID,
            messageID: output.message.id,
            type: "text",
            text: context,
            synthetic: true,
          });
        }
      } catch {}
      await sent;
    },
    "tool.execute.before": async (input: any, output: any) => {
      await send("tool.execute.before", { ...input, args: output?.args });
    },
    "tool.execute.after": async (input: any, output: any) => {
      const sent = send("tool.execute.after", {
        ...input,
        title: output?.title,
        metadata: output?.metadata,
      });
      try {
        const context =
          !isChild(input?.sessionID) && typeof output?.output === "string" ? await take() : null;
        if (context) {
          output.output += `\n\n[Vibeke: a message arrived from another pane]\n${context}`;
        }
      } catch {}
      await sent;
    },
    // Versions before the `permission.asked` event.
    "permission.ask": async (input: any, output: any) => {
      if (!input?.id || asked.has(input.id)) return;
      asked.add(input.id);
      const r = await send("permission.asked", input, true, input.id);
      const status = r?.status === "always" ? "allow" : r?.status;
      if (status === "allow" || status === "deny") output.status = status;
    },
  };
};
