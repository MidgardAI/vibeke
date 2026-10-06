"""Declares a preview with tls_origin, opens it through the authenticated proxy over HTTPS
without launching a browser, and prints the origin and the local CA to trust. The calls are
typed (mypy checks them, including the deliberate mistakes marked ``type: ignore``).

    VIBEKE_SESSION=default python3 examples/preview_tls.py <port>
"""

import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from vibeke_client import Client  # noqa: E402


async def typed_calls(c: Client, task: str, preview: str) -> None:
    """Typed examples that are only typechecked, never run."""
    # remove_worktree is "ask" or a JSON boolean.
    await c.task_finish({"task": task, "remove_worktree": True})
    await c.task_finish({"task": task, "remove_worktree": "ask"})
    # The string "true" is not a boolean (the server would keep the worktree).
    await c.task_finish({"task": task, "remove_worktree": "true"})  # type: ignore[typeddict-item]
    await c.preview_open({"preview": preview, "mode": "pane", "split": "down"})
    await c.preview_open({"preview": preview, "mode": "window"})
    await c.preview_open({"preview": preview, "mode": "profile"})  # type: ignore[arg-type]
    u = await c.preview_url({"preview": preview})
    maybe: "str | None" = u["proxy_url"]
    del maybe


async def main(port: int) -> None:
    async with await Client.connect(client="vibeke-client-py-tls-example") as c:
        d = await c.preview_declare({"port": port, "path": "/", "tls_origin": True})
        handle = d["preview"]["handle"]
        before = await c.preview_url({"preview": handle})
        r = await c.preview_open(
            {"preview": handle, "mode": "proxy", "tls_origin": True, "no_open": True}
        )
        if r["opened_in"] != "proxy":
            raise SystemExit(f"opened in {r['opened_in']}, not the proxy")
        ca = r.get("ca")
        print(
            json.dumps(
                {
                    "preview": handle,
                    "proxy_url_before": before["proxy_url"],
                    "opened_in": r["opened_in"],
                    "tls_origin": r.get("tls_origin"),
                    "https": str(r["url"]).startswith("https://"),
                    "has_open_url": isinstance(r.get("open_url"), str),
                    "session_ttl_s": r.get("session_ttl_s"),
                    "ca_sha256": ca["sha256"] if ca else None,
                    "ca_path": ca["path"] if ca else None,
                }
            )
        )
        await c.preview_forget({"preview": handle})


if __name__ == "__main__":
    arg = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("PREVIEW_PORT", "")
    if not arg.isdigit():
        raise SystemExit("usage: preview_tls.py <port>")
    asyncio.run(main(int(arg)))
