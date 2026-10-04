"""Opt-in local protocol smoke test. Never sends an inference prompt."""
import asyncio
import contextlib
import json
import os
from pathlib import Path
import tempfile
import unittest

from codex_bridge import Adapter, Rpc


@unittest.skipUnless(os.environ.get("ORCHRD_CODEX_SMOKE"), "requires installed Codex and local sockets")
class LiveCodex(unittest.IsolatedAsyncioTestCase):
    async def test_shared_session_discovery(self):
        from websockets.asyncio.client import unix_connect
        with tempfile.TemporaryDirectory(prefix="orchrd-codex-smoke-") as directory:
            socket = str(Path(directory) / "codex.sock")
            orchard = str(Path(directory) / "orchrd.sock")
            registered = asyncio.get_running_loop().create_future()
            peers = []
            async def accept(reader, writer):
                peers.append(writer)
                value = json.loads(await reader.readline())
                if not registered.done():
                    registered.set_result(value)
            listener = await asyncio.start_unix_server(accept, orchard)
            server = await asyncio.create_subprocess_exec(
                "codex", "app-server", "--listen", "unix://" + socket,
                env={**os.environ, "CODEX_HOME": directory},
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
            adapter = None
            tasks = []
            try:
                for _ in range(100):
                    if Path(socket).exists():
                        break
                    await asyncio.sleep(.1)
                async with unix_connect(socket) as terminal, unix_connect(socket) as voice:
                    terminal_rpc, voice_rpc = Rpc(terminal), Rpc(voice)
                    tasks.append(asyncio.create_task(terminal_rpc.read()))
                    tasks.append(asyncio.create_task(voice_rpc.read()))
                    await terminal_rpc.call("initialize", {"clientInfo": {"name": "orchrd_test_terminal", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}})
                    await terminal_rpc.send({"method": "initialized"})
                    result = await terminal_rpc.call("thread/start", {"cwd": directory})
                    thread = result["thread"]
                    # Empty threads have no rollout to rejoin. Append local history
                    # without starting a turn or invoking a model.
                    await terminal_rpc.call("thread/inject_items", {
                        "threadId": thread["id"], "items": [{"type": "message",
                        "role": "user", "content": [{"type": "input_text",
                        "text": "Local protocol fixture; no action required."}]}]})
                    adapter = Adapter(voice_rpc, orchard, .1)
                    tasks.append(asyncio.create_task(adapter.run()))
                    record = await asyncio.wait_for(registered, 15)
                    self.assertEqual(record["event"], "register")
                    self.assertEqual(record["kind"], "codex")
                    self.assertIn(thread["id"], record["id"])
                    self.assertEqual(record["cwd"], directory)
                    await asyncio.wait_for(adapter.sessions[thread["id"]].ready.wait(), 8)
                    # Both connections address the same in-memory thread.
                    check = await terminal_rpc.call("thread/read", {"threadId": thread["id"], "includeTurns": False})
                    self.assertEqual(check["thread"]["id"], thread["id"])
                    print("Live Codex: shared thread discovered and registered with Orchard; no inference sent.")
            finally:
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
                if adapter:
                    await adapter.close()
                for peer in peers:
                    peer.close()
                    await peer.wait_closed()
                listener.close()
                await listener.wait_closed()
                if server.returncode is None:
                    with contextlib.suppress(ProcessLookupError):
                        server.terminate()
                    try:
                        await asyncio.wait_for(server.wait(), 3)
                    except TimeoutError:
                        server.kill()
                        await server.wait()
