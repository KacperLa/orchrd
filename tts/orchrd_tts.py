#!/usr/bin/env python3
"""Persistent Pocket TTS worker for Orchrd.

Protocol: one JSON object per Unix-socket connection. Speech requests are played
in FIFO order, and the worker replies after that request finishes. A cancel
request stops the current utterance and clears anything still waiting.
"""
import argparse
import asyncio
import json
import os
import pathlib
import subprocess
import threading
import tomllib
from dataclasses import dataclass


@dataclass
class Settings:
    socket: pathlib.Path
    language: str
    model_config: str | None
    voice: str
    quantize: bool
    player: str


class Worker:
    def __init__(self, settings: Settings):
        self.settings = settings
        self.queue: asyncio.Queue[tuple[str, asyncio.Future]] = asyncio.Queue()
        self.current_cancel: threading.Event | None = None
        self.task: asyncio.Task | None = None
        self.model = None
        self.voice_state = None

    def load(self):
        from pocket_tts import TTSModel

        kwargs = {"quantize": self.settings.quantize}
        if self.settings.model_config:
            kwargs["config"] = self.settings.model_config
        else:
            kwargs["language"] = self.settings.language
        self.model = TTSModel.load_model(**kwargs)
        self.voice_state = self.model.get_state_for_audio_prompt(self.settings.voice)

    def start(self):
        self.task = asyncio.create_task(self._playback_loop())

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=5)
            request = json.loads(line)
            operation = request.get("op")
            if operation == "cancel":
                self.cancel_all()
                response = {"ok": True}
            elif operation == "speak":
                text = str(request.get("text", "")).strip()
                if not text:
                    raise ValueError("text is empty")
                await self.enqueue_speech(text)
                response = {"ok": True, "id": request.get("id")}
            elif operation == "health":
                response = {"ok": True, "loaded": self.model is not None}
            else:
                raise ValueError(f"unknown operation: {operation}")
        except Exception as exc:
            response = {"ok": False, "error": str(exc)}
        writer.write((json.dumps(response) + "\n").encode())
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    async def enqueue_speech(self, text: str):
        completed = asyncio.get_running_loop().create_future()
        await self.queue.put((text, completed))
        await completed

    def cancel_all(self):
        if self.current_cancel:
            self.current_cancel.set()
        while not self.queue.empty():
            _text, completed = self.queue.get_nowait()
            if not completed.done():
                completed.set_exception(RuntimeError("speech cancelled"))
            self.queue.task_done()

    async def _playback_loop(self):
        while True:
            text, completed = await self.queue.get()
            cancel = threading.Event()
            self.current_cancel = cancel
            try:
                await asyncio.to_thread(self._generate_and_play, text, cancel)
                if cancel.is_set():
                    raise RuntimeError("speech cancelled")
            except Exception as exc:
                print(f"Pocket TTS playback failed: {exc}", flush=True)
                if not completed.done():
                    completed.set_exception(exc)
            else:
                if not completed.done():
                    completed.set_result(None)
            finally:
                self.current_cancel = None
                self.queue.task_done()

    def _generate_and_play(self, text: str, cancel: threading.Event):
        assert self.model is not None and self.voice_state is not None
        player = subprocess.Popen(
            [self.settings.player, "--raw", f"--rate={self.model.sample_rate}", "--channels=1", "--format=f32", "-"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            for chunk in self.model.generate_audio_stream(self.voice_state, text, copy_state=True):
                if cancel.is_set():
                    break
                data = chunk.detach().cpu().numpy().astype("<f4", copy=False).tobytes()
                player.stdin.write(data)
                player.stdin.flush()
        finally:
            if player.stdin:
                player.stdin.close()
            if cancel.is_set():
                player.terminate()
            try:
                player.wait(timeout=2)
            except subprocess.TimeoutExpired:
                player.kill()


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config")
    parser.add_argument("--socket")
    parser.add_argument("--language")
    parser.add_argument("--model-config")
    parser.add_argument("--voice")
    parser.add_argument("--quantize", action="store_true")
    parser.add_argument("--player", default="pw-play")
    args = parser.parse_args()
    config = {}
    if args.config:
        with open(args.config, "rb") as handle:
            config = tomllib.load(handle).get("tts", {})
    runtime = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR", "/tmp")) / "orchrd"
    socket = args.socket or config.get("socket") or str(runtime / "tts.sock")
    language = args.language or config.get("language", "english_2026-04")
    model_config = args.model_config or config.get("model_config")
    voice = args.voice or config.get("voice")
    quantize = args.quantize or bool(config.get("quantize", False))
    if not voice:
        parser.error("Pocket TTS voice is required in --voice or [tts].voice")
    settings = Settings(pathlib.Path(socket), language, model_config, voice, quantize, args.player)
    settings.socket.parent.mkdir(parents=True, exist_ok=True)
    settings.socket.unlink(missing_ok=True)
    worker = Worker(settings)
    await asyncio.to_thread(worker.load)
    worker.start()
    server = await asyncio.start_unix_server(worker.handle, path=settings.socket)
    os.chmod(settings.socket, 0o600)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
