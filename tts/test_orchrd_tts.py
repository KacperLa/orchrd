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
        await asyncio.gather(first, second)

        self.assertEqual(
            self.order,
            ["start:first", "finish:first", "start:second", "finish:second"],
        )


if __name__ == "__main__":
    unittest.main()
