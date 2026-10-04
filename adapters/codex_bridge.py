#!/usr/bin/env python3
"""Connect existing Codex daemon threads to Orchard. Uses WebSockets over a local Unix socket."""
import argparse
import asyncio
import contextlib
import json
import logging
import os
from pathlib import Path
import uuid

LOG = logging.getLogger("orchrd.codex")
LIMIT = 8 * 1024 * 1024
APPROVALS = {"item/commandExecution/requestApproval", "item/fileChange/requestApproval"}


class ProtocolError(Exception):
    pass


def encode(value):
    return (json.dumps(value, ensure_ascii=False) + "\n").encode()


class Rpc:
    def __init__(self, reader, writer=None):
        self.reader, self.writer = reader, writer
        self.sequence = 0
        self.pending = {}
        self.events = asyncio.Queue()
        self.lock = asyncio.Lock()

    async def send(self, value):
        async with self.lock:
            if self.writer is None:
                await self.reader.send(json.dumps(value))
            else:
                self.writer.write(encode(value))
                await self.writer.drain()

    async def call(self, method, params):
        self.sequence += 1
        request_id = f"orchrd-{self.sequence}"
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        try:
            await self.send({"id": request_id, "method": method, "params": params})
            return await asyncio.wait_for(future, 8)
        finally:
            self.pending.pop(request_id, None)
            if not future.done():
                future.cancel()
            elif not future.cancelled():
                future.exception()

    async def read(self):
        try:
            while True:
                line = await self.reader.recv() if self.writer is None else await self.reader.readline()
                if not line:
                    break
                message = json.loads(line)
                if "method" in message:
                    await self.events.put(message)
                elif future := self.pending.get(message.get("id")):
                    if not future.done():
                        if "error" in message:
                            # Do not echo server bodies, which can contain task data.
                            future.set_exception(ProtocolError("Codex rejected the request"))
                        else:
                            future.set_result(message.get("result", {}))
            raise ConnectionError("Codex proxy disconnected")
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(ConnectionError("Codex proxy disconnected"))


def status_of(value):
    if value.get("type") == "active":
        if any(x in value.get("activeFlags", []) for x in ("waitingOnApproval", "waitingOnUserInput")):
            return "blocked"
        return "working"
    return "idle"


class Pending:
    def __init__(self, message):
        self.rpc_id = message["id"]
        self.method = message["method"]
        self.params = message["params"]
        self.index = 0
        self.answers = {}
        self.token = str(uuid.uuid4())
        self.resolved = asyncio.Event()
        if self.method == "item/tool/requestUserInput":
            questions = self.params.get("questions", [])
            if not questions or any(q.get("isSecret") for q in questions):
                raise ProtocolError("This question must be answered in the Codex terminal")
            ids = [q["id"] for q in questions]
            if len(set(ids)) != len(ids):
                raise ProtocolError("Codex sent duplicate question IDs")
        elif self.method not in APPROVALS:
            raise ProtocolError("This request must be answered in the Codex terminal")

    def question(self):
        qid = f"{self.token}:{self.index}"
        if self.method in APPROVALS:
            return {"event": "permission", "question_id": qid,
                    "question_kind": "permission", "title": "Codex approval",
                    "body": "\n".join(str(self.params[k]) for k in ("reason", "command", "cwd") if self.params.get(k)) or "Codex requests permission to change files.",
                    "options": [{"id": "accept", "label": "Allow once", "kind": "allow_once"},
                                {"id": "decline", "label": "Deny", "kind": "reject_once"}],
                    "multiple": False, "allow_custom": False}
        q = self.params["questions"][self.index]
        options = [{"id": str(i), "label": o["label"], "description": o.get("description", "")}
                   for i, o in enumerate(q.get("options") or [])]
        return {"event": "permission", "question_id": qid, "question_kind": "question",
                "title": q.get("header", "Codex question"), "body": q["question"],
                "options": options, "multiple": False,
                "allow_custom": bool(q.get("isOther")) or not options}

    def answer(self, value):
        q = self.question()
        if self.resolved.is_set() or value.get("question_id") != q["question_id"]:
            raise ProtocolError("That question is no longer pending")
        if q["question_kind"] != "question":
            raise ProtocolError("Use permission controls for this request")
        answer = value.get("answer", {})
        if answer.get("kind") == "choices":
            ids = answer.get("option_ids", [])
            options = {o["id"]: o["label"] for o in q["options"]}
            if len(ids) != 1 or ids[0] not in options:
                raise ProtocolError("Invalid question choice")
            text = options[ids[0]]
        elif answer.get("kind") == "custom" and q["allow_custom"]:
            text = answer.get("text", "")
            if not isinstance(text, str) or not text.strip() or len(text) > 16000:
                raise ProtocolError("Invalid custom answer")
        else:
            raise ProtocolError("Unsupported question answer")
        original = self.params["questions"][self.index]
        self.answers[original["id"]] = {"answers": [text]}
        if self.index + 1 < len(self.params["questions"]):
            self.index += 1
            return None
        return {"answers": self.answers}


class Session:
    def __init__(self, adapter, thread):
        self.adapter, self.thread = adapter, thread
        self.id = thread["id"]
        self.bridge_id = f"codex:{self.id}:{adapter.generation}"
        self.status = status_of(thread.get("status", {}))
        self.turn_id = None
        self.messages = {}
        self.final_message = None
        self.completed = set()
        self.pending = []
        self.reader = self.writer = self.task = None
        self.write_lock = asyncio.Lock()
        self.ready = asyncio.Event()

    @property
    def name(self):
        title = self.thread.get("name") or Path(self.thread.get("cwd", "")).name or "session"
        return f"Codex {title[:70]} {self.id[-6:]}"

    async def emit(self, message):
        async with self.write_lock:
            self.writer.write(encode(message))
            await self.writer.drain()

    async def connect(self):
        self.reader, self.writer = await asyncio.open_unix_connection(self.adapter.socket, limit=LIMIT)
        await self.emit({"event": "register", "id": self.bridge_id, "name": self.name,
                         "kind": "codex", "cwd": self.thread.get("cwd", ""), "status": self.status})
        self.task = asyncio.create_task(self.commands())

    async def close(self):
        if self.task:
            self.task.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self.task
        if self.writer:
            self.writer.close()
            with contextlib.suppress(Exception):
                await self.writer.wait_closed()

    async def publish_status(self):
        await self.emit({"event": "status", "status": "blocked" if self.pending else self.status})

    async def commands(self):
        while line := await self.reader.readline():
            command = json.loads(line)
            try:
                await self.command(command)
            except (ProtocolError, ConnectionError, TimeoutError) as error:
                await self.emit({"event": "receipt", "request_id": command.get("request_id"),
                                 "ok": False, "error": str(error) or "Codex did not confirm the request; it has not been retried"})
            else:
                await self.emit({"event": "receipt", "request_id": command.get("request_id"), "ok": True})
        raise ConnectionError("Orchard disconnected")

    async def command(self, command):
        value = command.get("value", {})
        action = command.get("action")
        rpc = self.adapter.rpc
        if action == "prompt":
            if self.pending or self.status == "blocked":
                raise ProtocolError("Codex is waiting for an answer")
            text = value.get("text", "")
            if not isinstance(text, str) or not text.strip():
                raise ProtocolError("Empty prompt")
            params = {"threadId": self.id, "input": [{"type": "text", "text": text}]}
            if self.status == "working":
                if not self.turn_id:
                    raise ProtocolError("Codex is already working; wait until this turn finishes")
                params["expectedTurnId"] = self.turn_id
                await rpc.call("turn/steer", params)
            else:
                await rpc.call("turn/start", params)
        elif action == "cancel":
            if not self.turn_id:
                raise ProtocolError("No known active Codex turn to cancel")
            await rpc.call("turn/interrupt", {"threadId": self.id, "turnId": self.turn_id})
        elif action in ("answer_question", "permission"):
            if not self.pending:
                raise ProtocolError("Codex has no supported pending question")
            pending = self.pending[0]
            if action == "permission":
                if pending.method not in APPROVALS or not isinstance(value.get("approve"), bool):
                    raise ProtocolError("This request is not a yes/no permission prompt")
                response = {"decision": "accept" if value["approve"] else "decline"}
            else:
                response = pending.answer(value)
                if response is None:
                    await self.emit(pending.question())
                    return
            await rpc.send({"id": pending.rpc_id, "result": response})
            # Server resolution (including another client's answer) clears the prompt.
            # Do not manufacture a working transition or retry a timed-out answer.
            await asyncio.wait_for(pending.resolved.wait(), 7)
        else:
            raise ProtocolError("Unsupported Codex command")

    async def event(self, message):
        method, p = message["method"], message.get("params", {})
        if "id" in message:
            if any(x.rpc_id == message["id"] for x in self.pending):
                return
            try:
                pending = Pending(message)
            except ProtocolError:
                LOG.warning("Codex request requires terminal input (%s)", method)
                await self.emit({"event": "message", "text": "Codex needs input in its terminal; this request is not supported by voice."})
                return
            self.pending.append(pending)
            self.turn_id = p.get("turnId", self.turn_id)
            if len(self.pending) == 1:
                await self.emit(pending.question())
        elif method == "serverRequest/resolved":
            matching = [x for x in self.pending if x.rpc_id == p.get("requestId")]
            for pending in matching:
                was_first = self.pending[0] is pending
                self.pending.remove(pending)
                pending.resolved.set()
                if was_first:
                    await self.emit({"event": "question_resolved", "question_id": pending.question()["question_id"]})
                    if self.pending:
                        await self.emit(self.pending[0].question())
                    else:
                        await self.publish_status()
        elif method == "thread/status/changed":
            self.status = status_of(p["status"])
            await self.publish_status()
        elif method == "thread/name/updated":
            self.thread["name"] = p.get("threadName") or p.get("name")
            await self.emit({"event": "metadata", "name": self.name})
        elif method == "turn/started":
            self.turn_id = p["turn"]["id"]
            self.messages.clear()
            self.final_message = None
            self.status = "working"
            await self.publish_status()
        elif method == "item/agentMessage/delta":
            if p.get("turnId") == self.turn_id:
                key = p["itemId"]
                self.messages[key] = (self.messages.get(key, "") + p.get("delta", ""))[:500000]
        elif method == "item/completed" and p.get("turnId") == self.turn_id:
            item = p.get("item", {})
            if item.get("type") == "agentMessage":
                self.messages[item["id"]] = item.get("text", "")[:500000]
                if item.get("phase") == "final_answer":
                    self.final_message = self.messages[item["id"]]
                await self.emit({"event": "message", "text": self.messages[item["id"]]})
        elif method == "turn/completed":
            turn = p["turn"]
            if self.turn_id and turn["id"] != self.turn_id:
                return
            if turn["id"] in self.completed:
                return
            self.completed.add(turn["id"])
            if len(self.completed) > 256:
                self.completed = {turn["id"]}
            for pending in self.pending:
                pending.resolved.set()
            self.pending.clear()
            text = "\n\n".join(self.messages.values()) if turn["id"] == self.turn_id else ""
            if turn["id"] == self.turn_id and self.final_message is not None:
                text = self.final_message
            for item in turn.get("items", []):
                if item.get("type") == "agentMessage" and item.get("phase") == "final_answer":
                    text = item.get("text", text)
            if turn.get("status") == "failed":
                text = "Codex's turn failed. Check the Codex terminal for details."
            elif turn.get("status") == "interrupted":
                text = "Codex stopped working."
            self.turn_id = None
            self.status = "idle"
            await self.emit({"event": "finished", "output": text[:900000]})
            self.messages.clear()


class Adapter:
    def __init__(self, rpc, socket, poll_seconds=3):
        self.rpc, self.socket, self.poll_seconds = rpc, socket, poll_seconds
        self.generation = uuid.uuid4().hex[:12]
        self.sessions = {}
        self.backlog = {}
        self.joining = set()

    async def discover(self):
        cursor, loaded = None, set()
        while True:
            params = {"limit": 100}
            if cursor:
                params["cursor"] = cursor
            page = await self.rpc.call("thread/loaded/list", params)
            loaded.update(page.get("data", []))
            cursor = page.get("nextCursor")
            if not cursor:
                break
        for tid in loaded - self.sessions.keys():
            metadata = await self.rpc.call("thread/read", {"threadId": tid, "includeTurns": False})
            thread = metadata["thread"]
            # Delegated workers are not independent voice targets.
            if thread.get("parentThreadId") or isinstance(thread.get("source"), dict):
                continue
            self.joining.add(tid)
            try:
                # Rejoin first: listing a thread does not guarantee it can be resumed.
                joined = await self.rpc.call("thread/resume", {"threadId": tid, "excludeTurns": True})
                session = Session(self, joined["thread"])
                await session.connect()
                self.sessions[tid] = session
                session.ready.set()
            except ProtocolError:
                self.backlog.pop(tid, None)
                continue
            finally:
                self.joining.discard(tid)
            for event in self.backlog.pop(tid, []):
                await session.event(event)
        for tid in self.sessions.keys() - loaded:
            await self.sessions.pop(tid).close()
            self.backlog.pop(tid, None)
        for session in self.sessions.values():
            if session.task.done():
                await session.task

    async def events(self):
        while True:
            message = await self.rpc.events.get()
            p = message.get("params", {})
            tid = p.get("threadId") or p.get("thread", {}).get("id")
            if tid in self.sessions:
                await self.sessions[tid].event(message)
            elif tid and message["method"] == "serverRequest/resolved":
                self.backlog[tid] = [e for e in self.backlog.get(tid, []) if e.get("id") != p.get("requestId")]
            elif tid in self.joining or (tid and "id" in message):
                queue = self.backlog.setdefault(tid, [])
                if len(queue) < 32:
                    queue.append(message)

    async def run(self):
        await self.rpc.call("initialize", {"clientInfo": {"name": "orchrd", "title": "Orchard", "version": "0.1.0"},
                                           "capabilities": {"experimentalApi": True}})
        await self.rpc.send({"method": "initialized"})
        async def poll():
            while True:
                await self.discover()
                await asyncio.sleep(self.poll_seconds)
        async with asyncio.TaskGroup() as group:
            group.create_task(self.events())
            group.create_task(poll())

    async def close(self):
        for session in self.sessions.values():
            await session.close()


async def run_once(args):
    from websockets.asyncio.client import unix_connect
    async with unix_connect(args.codex_socket, open_timeout=8, max_size=LIMIT) as connection:
        rpc = Rpc(connection)
        adapter = Adapter(rpc, args.orchrd_socket, args.poll_seconds)
        try:
            async with asyncio.TaskGroup() as group:
                group.create_task(rpc.read())
                group.create_task(adapter.run())
        finally:
            await adapter.close()


async def run(args):
    while True:
        try:
            await run_once(args)
        except (ExceptionGroup, OSError, ProtocolError) as error:
            LOG.warning("Adapter disconnected (%s); retrying in 5 seconds", type(error).__name__)
        await asyncio.sleep(5)


def main():
    runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/orchrd-{os.getuid()}"))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-socket", default=str(Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "app-server-control/app-server-control.sock"), help="Shared Codex Unix socket")
    parser.add_argument("--orchrd-socket", default=str(runtime / "orchrd/agents.sock"))
    parser.add_argument("--poll-seconds", type=float, default=3)
    args = parser.parse_args()
    if args.poll_seconds <= 0:
        parser.error("--poll-seconds must be positive")
    logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")
    with contextlib.suppress(KeyboardInterrupt):
        asyncio.run(run(args))


if __name__ == "__main__":
    main()
