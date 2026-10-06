"""Unit tests against an in-process mock of the server's wire behaviour.

Run: ``python3 -m unittest discover -s tests`` from ``clients/python`` (no third-party packages).
"""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from vibeke_client import ERROR_KINDS, METHODS, Client, EventOverflow, VibekeError  # noqa: E402

CURSOR = {"machine_uuid": "m", "session_uuid": "s", "log_epoch": "e", "seq": 7}


def ev(seq: int) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "events.event",
        "params": {
            "subscription_id": "s1",
            "event": {
                "seq": seq, "ts": 1, "v": 1, "tier": "sync", "type": "workspace.created",
                "subject": {"workspace": "w"}, "actor": {"kind": "system"}, "data": {},
            },
        },
    }


class Mock:
    def __init__(self, path: str):
        self.path = path
        self.held: list = []
        self.server = None

    async def start(self) -> None:
        self.server = await asyncio.start_unix_server(self.handle, self.path)

    async def stop(self) -> None:
        self.server.close()
        await self.server.wait_closed()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        def send(*msgs: dict) -> None:
            writer.write(b"".join(json.dumps(m, ensure_ascii=False).encode() + b"\n" for m in msgs))

        try:
            while True:
                line = await reader.readuntil(b"\n")
                req = json.loads(line)
                rid, method, params = req["id"], req["method"], req["params"]
                if method == "client.hello":
                    send({"jsonrpc": "2.0", "id": rid, "result": {
                        "server_version": "t", "api": "vibeke/1", "session": "default",
                        "machine": "local", "capabilities": ["*"], "features": []}})
                elif method == "server.status":
                    if params.get("__hold"):
                        self.held.append(rid)
                        continue
                    send({"jsonrpc": "2.0", "id": rid, "result": {"pid": 1, "version": "t x"}})
                    for h in self.held:
                        send({"jsonrpc": "2.0", "id": h, "result": {"pid": 2}})
                    self.held.clear()
                elif method == "pane.close":
                    send({"jsonrpc": "2.0", "id": rid, "error": {
                        "code": -32001, "message": "no such pane",
                        "data": {"kind": "not_found", "details": {"object": "pane"}, "retryable": False}}})
                elif method == "events.subscribe":
                    send(
                        {"jsonrpc": "2.0", "id": rid, "result": {"subscription_id": "s1", "at": CURSOR}},
                        ev(8),
                        {"jsonrpc": "2.0", "method": "unknown.notification", "params": {}},
                        ev(9),
                    )
                    if "overflow" in (params.get("types") or []):
                        await asyncio.sleep(0.02)
                        send({"jsonrpc": "2.0", "method": "events.overflow", "params": {
                            "subscription_id": "s1", "resume_from": {**CURSOR, "seq": 99}}})
                await writer.drain()
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            writer.close()


class ClientTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="vkpy-", dir="/tmp")
        self.sock = os.path.join(self.dir, "s.sock")
        self.mock = Mock(self.sock)
        await self.mock.start()

    async def asyncTearDown(self) -> None:
        await self.mock.stop()
        shutil.rmtree(self.dir, ignore_errors=True)

    async def test_hello_typed_call_u2028_and_pipelining(self) -> None:
        c = await Client.connect(self.sock, token="")
        self.assertEqual(c.hello["api"], "vibeke/1")
        a, b = await asyncio.gather(c.call("server.status", {"__hold": True}), c.server_status({}))
        self.assertEqual(b["version"], "t x")  # not split on U+2028
        self.assertEqual(a["pid"], 2)  # responses may arrive out of order
        await c.close()

    async def test_error_responses_raise_with_the_stable_kind(self) -> None:
        c = await Client.connect(self.sock, hello=False)
        with self.assertRaises(VibekeError) as cm:
            await c.pane_close({"pane": "p9"})
        self.assertEqual((cm.exception.kind, cm.exception.code, cm.exception.retryable), ("not_found", -32001, False))
        self.assertEqual(cm.exception.details, {"object": "pane"})
        await c.close()

    async def test_events_async_iterator_ignores_unknown_notifications(self) -> None:
        c = await Client.connect(self.sock, hello=False)
        s = await c.events(types=["workspace.*"])
        self.assertEqual(s.at["seq"], 7)
        seqs = []
        async for e in s:
            seqs.append(e.seq)
            if len(seqs) == 2:
                break
        self.assertEqual(seqs, [8, 9])
        self.assertEqual(e.type, "workspace.created")
        await s.close()
        await c.close()

    async def test_overflow_and_connection_close_end_the_stream(self) -> None:
        c = await Client.connect(self.sock, hello=False)
        s = await c.events(types=["overflow"])
        seen = []
        with self.assertRaises(EventOverflow) as cm:
            async for e in s:
                seen.append(e.seq)
        self.assertEqual(cm.exception.resume_from["seq"], 99)
        self.assertEqual(seen, [8, 9])
        s2 = await c.events()
        await c.close()
        self.assertEqual([e.seq async for e in s2], [8, 9])
        with self.assertRaises(ConnectionError):
            await c.server_status({})

    def test_generated_tables(self) -> None:
        self.assertIn("pane.split", METHODS)
        self.assertEqual(METHODS["pane.split"]["mutating"], True)
        self.assertIn("not_found", ERROR_KINDS)


if __name__ == "__main__":
    unittest.main()
