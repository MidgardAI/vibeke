// Vibeke OpenCode plugin (spec 04 §6.4). Installed by `vibeke integration install opencode` to
// ~/.config/opencode/plugin/vibeke.ts. Inert outside a Vibeke pane (needs VIBEKE=1,
// VIBEKE_SOCKET and VIBEKE_PANE_TOKEN).
//
// Every forwarded event runs `$VIBEKE_BIN hook opencode <event>` with the event JSON on stdin,
// the same shim Claude/Codex/Gemini hooks use (fail-open, always exit 0). `permission.ask`
// waits for the shim's stdout: `{"status": "allow" | "deny"}` sets the permission, anything else
// leaves OpenCode's own prompt in place. The server maps events in
// crates/vk-server/src/agents/opencode.rs.
//
// [verify M2] The hook names and payload shapes follow the upstream plugin docs; they have not
// been exercised against a live OpenCode binary.
import { spawn } from "node:child_process";

const BIN = process.env.VIBEKE_BIN || "vibeke";
const ON =
  process.env.VIBEKE === "1" &&
  !!process.env.VIBEKE_SOCKET &&
  !!process.env.VIBEKE_PANE_TOKEN;

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

function send(event: string, payload: unknown, wait = false): Promise<any> {
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
    child.on("error", () => resolve(null));
    if (wait) child.stdout.on("data", (d: any) => (out += d));
    child.on("close", () => {
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

export const VibekePlugin = async ({ directory }: { directory?: string }) => ({
  event: async ({ event }: { event: any }) => {
    const type = event?.type;
    if (!type || !FORWARD.has(type)) return;
    const props = event.properties ?? {};
    if (type === "message.updated") {
      // Usage only: completed assistant messages.
      const info = props.info ?? {};
      if (info.role !== "assistant" || !info.time?.completed) return;
    }
    await send(type, { ...props, directory });
  },
  "chat.message": async (input: any, output: any) => {
    const text = (output?.parts ?? [])
      .filter((p: any) => p?.type === "text")
      .map((p: any) => p.text)
      .join("\n");
    await send("chat.message", { sessionID: input?.sessionID, text });
  },
  "tool.execute.before": async (input: any, output: any) => {
    await send("tool.execute.before", { ...input, args: output?.args });
  },
  "tool.execute.after": async (input: any, output: any) => {
    await send("tool.execute.after", {
      ...input,
      title: output?.title,
      metadata: output?.metadata,
    });
  },
  "permission.ask": async (input: any, output: any) => {
    const r = await send("permission.ask", input, true);
    const status = r?.status;
    if (status === "allow" || status === "deny") output.status = status;
  },
});
