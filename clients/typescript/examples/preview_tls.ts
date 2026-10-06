// Declares a preview with `tls_origin`, opens it through the authenticated proxy over HTTPS
// without launching a browser, and prints the origin and the local CA to trust. The calls are
// fully typed: `tsc --noEmit -p tsconfig.json` checks them (and the deliberate mistakes below).
//   VIBEKE_SESSION=default node examples/preview_tls.ts <port>
import { VibekeClient } from "../src/index.ts";
import type { Methods } from "../src/index.ts";

type OpenResult = Methods["preview.open"]["result"];

/** The proxy variant of `preview.open`'s result (`opened_in: "proxy"`). */
function proxyResult(r: OpenResult): Extract<OpenResult, { opened_in: "proxy" }> {
  if (r.opened_in !== "proxy") throw new Error(`opened in ${r.opened_in}, not the proxy`);
  return r;
}

/** Typed examples that are only typechecked, never run. */
export async function typedCalls(c: VibekeClient, task: string, preview: string): Promise<void> {
  // `remove_worktree` is "ask" or a JSON boolean.
  await c.call("task.finish", { task, remove_worktree: true });
  await c.call("task.finish", { task, remove_worktree: "ask" });
  // @ts-expect-error the string "true" is not a boolean (the server would keep the worktree)
  await c.call("task.finish", { task, remove_worktree: "true" });
  // Every opening mode the server accepts.
  await c.call("preview.open", { preview, mode: "pane", split: "down" });
  await c.call("preview.open", { preview, mode: "window" });
  // @ts-expect-error "profile" is not an opening mode
  await c.call("preview.open", { preview, mode: "profile" });
  // `proxy_url` is null until a proxy origin exists.
  const u = await c.call("preview.url", { preview });
  const maybe: string | null = u.proxy_url;
  void maybe;
}

const port = Number(process.argv[2] ?? process.env.PREVIEW_PORT);
if (!Number.isInteger(port) || port <= 0) throw new Error("usage: preview_tls.ts <port>");
const c = await VibekeClient.connect({ client: "vibeke-client-ts-tls-example" });
try {
  const d = await c.call("preview.declare", { port, path: "/", tls_origin: true });
  const before = await c.call("preview.url", { preview: d.preview.handle });
  const r = proxyResult(
    await c.call("preview.open", {
      preview: d.preview.handle,
      mode: "proxy",
      tls_origin: true,
      no_open: true,
    }),
  );
  console.log(
    JSON.stringify({
      preview: d.preview.handle,
      proxy_url_before: before.proxy_url,
      opened_in: r.opened_in,
      tls_origin: r.tls_origin,
      https: r.url.startsWith("https://"),
      has_open_url: typeof r.open_url === "string",
      session_ttl_s: r.session_ttl_s,
      ca_sha256: r.ca?.sha256 ?? null,
      ca_path: r.ca?.path ?? null,
    }),
  );
  await c.call("preview.forget", { preview: d.preview.handle });
} finally {
  c.close();
}
