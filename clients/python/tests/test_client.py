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

from vibeke_client import (  # noqa: E402
    ERROR_KINDS,
    METHODS,
    Client,
    EventOverflow,
    SocketTrustError,
    VibekeError,
    check_socket_trust,
)

CURSOR = {"machine_uuid": "m", "session_uuid": "s", "log_epoch": "e", "seq": 7}


def ev(seq: int, sid: str = "s1") -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "events.event",
        "params": {
            "subscription_id": sid,
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
        self.unsubscribed: list = []
        self.writer = None

    async def start(self) -> None:
        self.server = await asyncio.start_unix_server(self.handle, self.path)

    async def stop(self) -> None:
        self.server.close()
        await self.server.wait_closed()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        def send(*msgs: dict) -> None:
            writer.write(b"".join(json.dumps(m, ensure_ascii=False).encode() + b"\n" for m in msgs))

        self.writer = writer
        self.send = send

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
                elif method == "events.unsubscribe":
                    self.unsubscribed.append(params["subscription_id"])
                    send({"jsonrpc": "2.0", "id": rid, "result": {"unsubscribed": True}})
                elif method == "events.subscribe" and "burst" in (params.get("types") or []):
                    # Response, events and the overflow in ONE write (finding 9).
                    send(
                        {"jsonrpc": "2.0", "id": rid, "result": {"subscription_id": "sb", "at": CURSOR}},
                        ev(10, "sb"),
                        ev(11, "sb"),
                        {"jsonrpc": "2.0", "method": "events.overflow", "params": {
                            "subscription_id": "sb", "resume_from": {**CURSOR, "seq": 11}}},
                    )
                elif method == "events.subscribe" and "closing" in (params.get("types") or []):
                    send({"jsonrpc": "2.0", "id": rid, "result": {"subscription_id": "sc", "at": CURSOR}})
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

    async def test_response_events_and_overflow_in_one_chunk(self) -> None:
        c = await Client.connect(self.sock, hello=False)
        s = await c.events(types=["burst"])
        seen = []

        async def drain() -> None:
            async for e in s:
                seen.append(e.seq)

        with self.assertRaises(EventOverflow) as cm:
            await asyncio.wait_for(drain(), 2)  # a lost overflow hangs here
        self.assertEqual(cm.exception.resume_from["seq"], 11)
        self.assertEqual(seen, [10, 11])
        self.assertEqual(c.retained(), {"streams": 0, "events": 0})
        await c.close()

    async def test_closed_stream_unsubscribes_and_drops_later_events(self) -> None:
        c = await Client.connect(self.sock, hello=False)
        s = await c.events(types=["closing"])
        await s.close()
        # The server keeps sending (it has not processed the unsubscribe yet): 1,000 events.
        self.mock.send(*[ev(100 + i, "sc") for i in range(1000)])
        await self.mock.writer.drain()
        # A round trip after the burst: every event above has been handled.
        self.assertEqual((await c.server_status({}))["pid"], 1)
        self.assertIn("sc", self.mock.unsubscribed)
        self.assertEqual(c.retained(), {"streams": 0, "events": 0})
        self.assertEqual(s.queued, 0)
        self.assertFalse(hasattr(c, "_early"), "no early-event buffer")
        self.assertEqual([e async for e in s], [])
        await c.close()

    def test_generated_tables(self) -> None:
        self.assertIn("pane.split", METHODS)
        self.assertEqual(METHODS["pane.split"]["mutating"], True)
        self.assertIn("not_found", ERROR_KINDS)


class Listener:
    """A unix listener counting connections and bytes received."""

    def __init__(self, path: str):
        self.path = path
        self.conns = 0
        self.bytes = 0

    async def start(self) -> "Listener":
        async def on(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
            self.conns += 1
            while True:
                b = await reader.read(4096)
                if not b:
                    break
                self.bytes += len(b)
            writer.close()

        self.server = await asyncio.start_unix_server(on, self.path)
        return self

    async def stop(self) -> None:
        self.server.close()
        await self.server.wait_closed()


class SocketTrustTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.base = tempfile.mkdtemp(prefix="vkt-", dir="/tmp")
        self.saved = os.environ.get("VIBEKE_RUNTIME_DIR")

    async def asyncTearDown(self) -> None:
        if self.saved is None:
            os.environ.pop("VIBEKE_RUNTIME_DIR", None)
        else:
            os.environ["VIBEKE_RUNTIME_DIR"] = self.saved
        shutil.rmtree(self.base, ignore_errors=True)

    async def test_check_mirrors_the_cli(self) -> None:
        root = os.path.join(self.base, "run")
        sess = os.path.join(root, "default")
        os.makedirs(sess)
        os.chmod(root, 0o700)
        os.chmod(sess, 0o700)
        sock = os.path.join(sess, "vibeke.sock")
        listeners = [await Listener(sock).start()]
        check_socket_trust(sock, root=root)
        check_socket_trust(os.path.join(root, "other", "vibeke.sock"), root=root)  # missing: ok
        with self.assertRaisesRegex(SocketTrustError, "owned by uid"):
            check_socket_trust(sock, root=root, uid=4242)  # foreign owner
        os.chmod(sess, 0o750)
        with self.assertRaisesRegex(SocketTrustError, "need 700"):
            check_socket_trust(sock, root=root)
        os.chmod(sess, 0o700)
        plain = os.path.join(sess, "plain.sock")
        open(plain, "w").close()
        with self.assertRaisesRegex(SocketTrustError, "not a socket"):
            check_socket_trust(plain, root=root)
        elsewhere = os.path.join(self.base, "elsewhere")
        os.mkdir(elsewhere, 0o700)
        listeners.append(await Listener(os.path.join(elsewhere, "vibeke.sock")).start())
        os.symlink(elsewhere, os.path.join(root, "linked"))
        with self.assertRaisesRegex(SocketTrustError, "not a plain directory"):
            check_socket_trust(os.path.join(root, "linked", "vibeke.sock"), root=root)
        root_link = os.path.join(self.base, "rootlink")
        os.symlink(root, root_link)
        with self.assertRaisesRegex(SocketTrustError, "not a plain directory"):
            check_socket_trust(os.path.join(root_link, "default", "vibeke.sock"), root=root_link)
        shared = os.path.join(self.base, "shared")
        os.mkdir(shared)
        listeners.append(await Listener(os.path.join(shared, "x.sock")).start())
        for mode in (0o777, 0o775, 0o757):
            os.chmod(shared, mode)
            with self.assertRaisesRegex(SocketTrustError, f"mode {mode:o}"):
                check_socket_trust(os.path.join(shared, "x.sock"), root=root)
        os.chmod(shared, 0o755)
        check_socket_trust(os.path.join(shared, "x.sock"), root=root)
        for li in listeners:
            await li.stop()

    async def test_connect_refuses_before_sending_a_byte(self) -> None:
        root = os.path.join(self.base, "run")
        os.mkdir(root)
        os.chmod(root, 0o700)
        planted = os.path.join(self.base, "planted")
        os.mkdir(planted, 0o700)
        l1 = await Listener(os.path.join(planted, "vibeke.sock")).start()
        os.symlink(planted, os.path.join(root, "default"))
        os.environ["VIBEKE_RUNTIME_DIR"] = root
        with self.assertRaises(SocketTrustError):
            await Client.connect(session="default", token="secret-pane-token")
        shared = os.path.join(self.base, "shared")
        os.mkdir(shared)
        os.chmod(shared, 0o777)
        l2 = await Listener(os.path.join(shared, "x.sock")).start()
        with self.assertRaises(SocketTrustError):
            await Client.connect(os.path.join(shared, "x.sock"), token="secret-pane-token")
        await asyncio.sleep(0.05)
        for li in (l1, l2):
            self.assertEqual(li.conns, 0, "no connection")
            self.assertEqual(li.bytes, 0, "zero bytes sent")
        # insecure=True skips the check.
        c = await Client.connect(os.path.join(shared, "x.sock"), hello=False, insecure=True)
        await asyncio.sleep(0.05)
        self.assertEqual(l2.conns, 1)
        await c.close()
        await l1.stop()
        await l2.stop()


if __name__ == "__main__":
    unittest.main()
