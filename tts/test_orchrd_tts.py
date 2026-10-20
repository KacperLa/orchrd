import asyncio
import pathlib
import time
import unittest

from orchrd_tts import Settings, Worker


class PlaybackQueueTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        settings = Settings(
            pathlib.Path("/tmp/unused.sock"),
            "test",
            None,
            "test",
            False,
            "test",
        )
        self.worker = Worker(settings)
        self.order = []

        def generate(text, _cancel):
            self.order.append(f"start:{text}")
            time.sleep(0.02)
            self.order.append(f"finish:{text}")

        self.worker._generate_and_play = generate
        self.worker.start()

    async def asyncTearDown(self):
        self.worker.task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await self.worker.task

    async def test_speech_plays_in_submission_order(self):
        first = asyncio.create_task(self.worker.enqueue_speech("first"))
        await asyncio.sleep(0)
        second = asyncio.create_task(self.worker.enqueue_speech("second"))
        await asyncio.wait_for(asyncio.gather(first, second), 5)

        self.assertEqual(
            self.order,
            ["start:first", "finish:first", "start:second", "finish:second"],
        )


class QueueBoundsTests(unittest.IsolatedAsyncioTestCase):
    async def test_full_queue_rejects_and_cancel_clears_waiters(self):
        worker = Worker(Settings(pathlib.Path("/tmp/unused.sock"), "test", None, "test", False, "test"))
        worker.queue = asyncio.Queue(maxsize=2)
        tasks = [asyncio.create_task(worker.enqueue_speech(text)) for text in ("one", "two")]
        await asyncio.sleep(0)
        with self.assertRaisesRegex(RuntimeError, "queue is full"):
            await worker.enqueue_speech("three")
        self.assertEqual(worker.queue.qsize(), 2)
        worker.cancel_all()
        results = await asyncio.wait_for(asyncio.gather(*tasks, return_exceptions=True), 1)
        self.assertTrue(all(isinstance(result, RuntimeError) for result in results))
        self.assertTrue(worker.queue.empty())

    async def test_oversized_text_never_enters_queue(self):
        worker = Worker(Settings(pathlib.Path("/tmp/unused.sock"), "test", None, "test", False, "test"))
        with self.assertRaisesRegex(ValueError, "64000 bytes"):
            await worker.enqueue_speech("é" * 32001)
        self.assertTrue(worker.queue.empty())


if __name__ == "__main__":
    unittest.main()
