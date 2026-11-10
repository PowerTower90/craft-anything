"""Shared helpers for the voice service tests.

Run from the repository root with the voice Python:
    <voice python> -I -m pytest crates/engines/src/voice/scripts/tests -q
Real-model tests (GPU, slow) run only when CUDA is available; set VOICE_REAL=0 to skip them.
"""
import asyncio
import importlib.util
import json
import os
import sys
import threading
import time

import pytest

SCRIPTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVICE_PY = os.path.join(SCRIPTS, "voice_service.py")
REPO = os.path.abspath(os.path.join(SCRIPTS, "..", "..", "..", "..", ".."))


def load_service_module():
    if "voice_service" in sys.modules:
        return sys.modules["voice_service"]
    spec = importlib.util.spec_from_file_location("voice_service", SERVICE_PY)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["voice_service"] = mod
    spec.loader.exec_module(mod)
    return mod


vs = load_service_module()


class Client:
    """A small synchronous WebSocket client (aiohttp on a background loop) that records every event."""

    def __init__(self, port, host="127.0.0.1"):
        self.url = f"ws://{host}:{port}/ws"
        self.events = []
        self.cv = threading.Condition()
        self.closed = threading.Event()
        self.loop = asyncio.new_event_loop()
        self.ready = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        if not self.ready.wait(10):
            raise RuntimeError("could not connect to the voice service")

    def _run(self):
        asyncio.set_event_loop(self.loop)
        self.loop.run_until_complete(self._main())

    async def _main(self):
        import aiohttp
        async with aiohttp.ClientSession() as s:
            async with s.ws_connect(self.url) as ws:
                self.ws = ws
                self.ready.set()
                async for msg in ws:
                    if msg.type == aiohttp.WSMsgType.TEXT:
                        ev = json.loads(msg.data)
                        ev["_t"] = time.perf_counter()
                        with self.cv:
                            self.events.append(ev)
                            self.cv.notify_all()
                self.close_code = ws.close_code
        self.closed.set()

    def send(self, **cmd):
        cmd_t = time.perf_counter()
        fut = asyncio.run_coroutine_threadsafe(self.ws.send_str(json.dumps(cmd)), self.loop)
        fut.result(5)
        return cmd_t

    def wait(self, type_, pred=None, timeout=30.0, after=0.0):
        """Wait for the first event of `type_` (received at or after monotonic time `after`) matching pred."""
        end = time.perf_counter() + timeout
        with self.cv:
            while True:
                for ev in self.events:
                    if ev["type"] == type_ and ev["_t"] >= after and (pred is None or pred(ev)):
                        return ev
                left = end - time.perf_counter()
                if left <= 0:
                    raise TimeoutError(f"no {type_!r} event within {timeout}s; got {[e['type'] for e in self.events][-30:]}")
                self.cv.wait(left)

    def of(self, type_, after=0.0):
        with self.cv:
            return [e for e in self.events if e["type"] == type_ and e["_t"] >= after]

    def close(self):
        if not self.closed.is_set():
            asyncio.run_coroutine_threadsafe(self.ws.close(), self.loop)
            self.closed.wait(5)


class InProcess:
    """The real service (VoiceService + Server) in this process, with a fake microphone/speaker."""

    def __init__(self, models_dir, stub=True, device="cpu", allow_inject=False, aec=False):
        self.audio = vs.FakeAudioIO()
        self.svc = vs.VoiceService(models_dir, device, self.audio, stub_models=stub, allow_inject=allow_inject)
        # The stand-in models hear pure tones, which the echo canceller's noise suppressor would
        # dampen; the tests that are about echo cancellation turn it on.
        self.svc.cfg["echo_cancellation"] = aec
        self.server = vs.Server(self.svc, "127.0.0.1", 0)
        self.port = None
        ready = threading.Event()

        def on_ready(port):
            self.port = port
            self.svc.open_mic()
            threading.Thread(target=self.svc.load_models, daemon=True).start()
            ready.set()

        self.thread = threading.Thread(target=lambda: asyncio.run(self.server.run(on_ready)), daemon=True)
        self.thread.start()
        if not ready.wait(30):
            raise RuntimeError("server did not start")

    def wait_ready(self, timeout=600):
        if not self.svc.ready_evt.wait(timeout) or not self.svc.models_ready:
            raise RuntimeError(f"models did not load: {self.svc.loading}")

    def close(self):
        self.server.request_shutdown()
        self.thread.join(10)
        self.svc.shutdown()


def tone(freq, seconds, sr=16000, amp=0.3, vibrato=0.0):
    """A harmonic-rich 'voice' for the stub models: different freq -> different stub speaker."""
    import numpy as np
    t = np.arange(int(sr * seconds)) / sr
    f = freq * (1 + vibrato * np.sin(2 * np.pi * 5 * t))
    phase = 2 * np.pi * np.cumsum(f) / sr
    x = sum(np.sin(k * phase) / k for k in range(1, 6))
    return (amp * x / np.max(np.abs(x))).astype(np.float32)


def silence(seconds, sr=16000):
    import numpy as np
    return np.zeros(int(sr * seconds), dtype=np.float32)


@pytest.fixture
def tmp_models(tmp_path):
    d = tmp_path / "models"
    d.mkdir()
    return str(d)


def write_wav(path, audio, sr=16000):
    """A 16-bit mono WAV of `audio` (floats in -1..1)."""
    import wave
    import numpy as np
    pcm = (np.clip(audio, -1, 1) * 32767).astype("<i2").tobytes()
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(pcm)
    return str(path)
