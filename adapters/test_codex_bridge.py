import asyncio
import json
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock
from unittest.mock import patch

from codex_bridge import Adapter, Pending, ProtocolError, Rpc, Session


def question(request_id=10, count=1):
    return {"id": request_id, "method": "item/tool/requestUserInput", "params": {
        "threadId": "thread-one", "turnId": "turn-one", "itemId": "item-one", "isBlocking": True,
        "questions": [{"id": f"q{i}", "header": "Scope", "question": "What should I check?",
                       "isOther": True, "options": [{"label": "Code", "description": "Source files"},
                                                     {"label": "Email", "description": "Inbox"}]}
                      for i in range(count)]}}


def answer(pending, option="1"):
    return {"question_id": pending.question()["question_id"], "answer": {"kind": "choices", "option_ids": [option]}}


class Questions(unittest.TestCase):
    def test_exact_ids_custom_answers_and_multiple_questions(self):
        pending = Pending(question(count=2))
        first = answer(pending)
        self.assertIsNone(pending.answer(first))
        with self.assertRaises(ProtocolError):
            pending.answer(first)
        result = pending.answer({"question_id": pending.question()["question_id"],
                                 "answer": {"kind": "custom", "text": "Do not edit files."}})
        self.assertEqual(result, {"answers": {"q0": {"answers": ["Email"]},
                                               "q1": {"answers": ["Do not edit files."]}}})

    def test_invalid_secret_and_stale_answers(self):
        pending = Pending(question())
        with self.assertRaises(ProtocolError):
            pending.answer(answer(pending, "999"))
        value = answer(pending)
        value["answer"]["option_ids"] = ["0", "1"]
        with self.assertRaises(ProtocolError):
            pending.answer(value)
        pending.resolved.set()
        with self.assertRaises(ProtocolError):
            pending.answer(answer(pending))
        secret = question()
        secret["params"]["questions"][0]["isSecret"] = True
        with self.assertRaises(ProtocolError):
            Pending(secret)

    def test_permission_cannot_be_answered_as_question(self):
        pending = Pending({"id": 3, "method": "item/commandExecution/requestApproval",
                           "params": {"command": "touch file", "reason": "Need write access"}})
        self.assertIn("touch file", pending.question()["body"])
        with self.assertRaises(ProtocolError):
            pending.answer(answer(pending, "accept"))


class Sessions(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.rpc = SimpleNamespace(call=AsyncMock(return_value={}), send=AsyncMock())
        self.adapter = SimpleNamespace(rpc=self.rpc, generation="generation")
        self.session = Session(self.adapter, {"id": "thread-one", "name": "Builder", "cwd": "/tmp", "status": {"type": "idle"}})
        self.session.emit = AsyncMock()

    async def test_prompt_steer_and_cancel_use_the_same_thread(self):
        await self.session.command({"action": "prompt", "value": {"text": "Check tests"}})
        self.rpc.call.assert_awaited_with("turn/start", {"threadId": "thread-one", "input": [{"type": "text", "text": "Check tests"}]})
        await self.session.event({"method": "turn/started", "params": {"turn": {"id": "turn-one"}}})
        await self.session.command({"action": "prompt", "value": {"text": "Only unit tests"}})
        self.rpc.call.assert_awaited_with("turn/steer", {"threadId": "thread-one", "expectedTurnId": "turn-one", "input": [{"type": "text", "text": "Only unit tests"}]})
        await self.session.command({"action": "cancel"})
        self.rpc.call.assert_awaited_with("turn/interrupt", {"threadId": "thread-one", "turnId": "turn-one"})

    async def test_question_answer_waits_for_server_resolution(self):
        await self.session.event(question())
        pending = self.session.pending[0]
        task = asyncio.create_task(self.session.command({"action": "answer_question", "value": answer(pending)}))
        await asyncio.sleep(0)
        self.assertFalse(task.done())
        self.rpc.send.assert_awaited_with({"id": 10, "result": {"answers": {"q0": {"answers": ["Email"]}}}})
        await self.session.event({"method": "serverRequest/resolved", "params": {"requestId": 10}})
        await task
        self.assertEqual(self.session.pending, [])
        self.assertTrue(any(call.args[0]["event"] == "question_resolved" for call in self.session.emit.await_args_list))

    async def test_terminal_answer_invalidates_voice_answer(self):
        await self.session.event(question())
        value = answer(self.session.pending[0])
        await self.session.event({"method": "serverRequest/resolved", "params": {"requestId": 10}})
        with self.assertRaises(ProtocolError):
            await self.session.command({"action": "answer_question", "value": value})
        self.rpc.send.assert_not_awaited()

    async def test_partial_answers_do_not_claim_agent_is_working(self):
        await self.session.event(question(count=2))
        await self.session.command({"action": "answer_question", "value": answer(self.session.pending[0])})
        self.rpc.send.assert_not_awaited()
        last = self.session.emit.await_args.args[0]
        self.assertEqual(last["event"], "permission")
        self.assertTrue(last["question_id"].endswith(":1"))

    async def test_generic_approval_cannot_select_a_question_option(self):
        await self.session.event(question())
        with self.assertRaises(ProtocolError):
            await self.session.command({"action": "permission", "value": {"approve": True}})
        with self.assertRaises(ProtocolError):
            await self.session.command({"action": "prompt", "value": {"text": "new task"}})
        self.rpc.send.assert_not_awaited()
        self.rpc.call.assert_not_awaited()

    async def test_permission_preserves_request_id_and_approves_once(self):
        await self.session.event({"id": "approve-1", "method": "item/fileChange/requestApproval", "params": {"turnId": "turn-one"}})
        task = asyncio.create_task(self.session.command({"action": "permission", "value": {"approve": True}}))
        await asyncio.sleep(0)
        self.rpc.send.assert_awaited_with({"id": "approve-1", "result": {"decision": "accept"}})
        await self.session.event({"method": "serverRequest/resolved", "params": {"requestId": "approve-1"}})
        await task

    async def test_final_results_not_tool_output_and_not_duplicated(self):
        await self.session.event({"method": "turn/started", "params": {"turn": {"id": "turn-one"}}})
        for item in [{"type": "commandExecution", "id": "shell", "text": "private shell output"},
                     {"type": "agentMessage", "id": "reply", "text": "Tests pass."}]:
            await self.session.event({"method": "item/completed", "params": {"turnId": "turn-one", "item": item}})
        event = {"method": "turn/completed", "params": {"turn": {"id": "turn-one", "status": "completed"}}}
        await self.session.event(event)
        await self.session.event(event)
        outputs = [c.args[0] for c in self.session.emit.await_args_list if c.args[0]["event"] == "finished"]
        self.assertEqual(outputs, [{"event": "finished", "output": "Tests pass."}])

    async def test_failure_never_reports_success(self):
        await self.session.event({"method": "turn/completed", "params": {"turn": {"id": "turn-one", "status": "failed"}}})
        self.assertIn("failed", self.session.emit.await_args.args[0]["output"])

    async def test_final_answer_excludes_progress_commentary(self):
        await self.session.event({"method": "turn/started", "params": {"turn": {"id": "t"}}})
        for phase, text in [("commentary", "Checking tests."), ("final_answer", "Tests pass.")]:
            await self.session.event({"method": "item/completed", "params": {"turnId": "t",
                "item": {"type": "agentMessage", "id": phase, "phase": phase, "text": text}}})
        await self.session.event({"method": "turn/completed", "params": {"turn": {"id": "t", "status": "completed"}}})
        self.assertEqual(self.session.emit.await_args.args[0], {"event": "finished", "output": "Tests pass."})


class Discovery(unittest.IsolatedAsyncioTestCase):
    async def test_unresumable_thread_is_not_registered(self):
        rpc = SimpleNamespace(call=AsyncMock(side_effect=[{"data": ["empty"]},
            {"thread": {"id": "empty", "source": "cli"}}, ProtocolError("No rollout")]))
        adapter = Adapter(rpc, "/unused")
        with patch.object(Session, "connect", new_callable=AsyncMock) as connect:
            await adapter.discover()
            connect.assert_not_awaited()
        self.assertEqual(adapter.sessions, {})
        self.assertEqual(adapter.joining, set())


class RpcTransport(unittest.IsolatedAsyncioTestCase):
    async def test_interleaved_events_responses_and_disconnect(self):
        reader = asyncio.StreamReader()
        writer = SimpleNamespace(write=lambda data: sent.append(json.loads(data)), drain=AsyncMock())
        sent = []
        rpc = Rpc(reader, writer)
        reading = asyncio.create_task(rpc.read())
        calling = asyncio.create_task(rpc.call("thread/loaded/list", {}))
        await asyncio.sleep(0)
        reader.feed_data(b'{"method":"thread/status/changed","params":{}}\n')
        reader.feed_data((json.dumps({"id": sent[0]["id"], "result": {"data": ["one"]}}) + "\n").encode())
        self.assertEqual(await calling, {"data": ["one"]})
        self.assertEqual((await rpc.events.get())["method"], "thread/status/changed")
        calling = asyncio.create_task(rpc.call("turn/start", {}))
        await asyncio.sleep(0)
        reader.feed_eof()
        with self.assertRaises(ConnectionError):
            await calling
        with self.assertRaises(ConnectionError):
            await reading
        self.assertEqual(len(sent), 2)  # no automatic mutation retry


if __name__ == "__main__":
    unittest.main()
