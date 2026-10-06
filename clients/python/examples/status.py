"""Connects to the session socket, prints server.status and workspace.list, subscribes to
workspace events, creates a workspace and waits for its workspace.created event.

    VIBEKE_SESSION=default python3 examples/status.py
"""

import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from vibeke_client import Client  # noqa: E402


async def main() -> None:
    async with await Client.connect(client="vibeke-client-py-example") as c:
        status = await c.server_status({})
        listing = await c.workspace_list({})
        events = await c.events(types=["workspace.*"])
        made = await c.workspace_create({"cwd": os.getcwd(), "name": "py-client"})
        created = None
        async for ev in events:
            if ev.type == "workspace.created":
                created = ev
                break
        print(
            json.dumps(
                {
                    "session": status["session"],
                    "pid": status["pid"],
                    "workspaces_before": len(listing["workspaces"]),
                    "created_workspace": made["workspace"]["id"],
                    "event": created.__dict__ if created else None,
                }
            )
        )


asyncio.run(main())
