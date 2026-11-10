"""Craft Anything voice service: microphone, VAD, push-to-talk / wake word, streaming Whisper STT,
speaker verification and streaming Qwen3-TTS with barge-in, behind one local WebSocket.

    <voice python> -I voice_service.py --host 127.0.0.1 --port 8191 --models-dir <dir>
                   [--device cuda:0|cpu] [--parent-pid <pid>]

Prints exactly one line `READY {"port":8191,"version":1}` to stdout once the socket listens (models keep
loading in the background; `state.models_ready` says when they are usable), logs to stderr, serves
`ws://host:port/ws` and `GET /health`. The protocol is documented in docs/voice-protocol.md.

Everything runs locally. Models (downloaded on first use into --models-dir unless already in the
Hugging Face cache): Whisper large-v3-turbo (STT), Silero VAD (bundled with the silero-vad package),
SpeechBrain ECAPA-TDNN (speaker verification), Qwen3-TTS 1.7B Base + VoiceDesign (TTS), openWakeWord
built-ins (wake word; any other phrase is spotted with Whisper).

This file is self-contained on purpose (it is run with `python -I`, so sibling modules would not be
importable): audio I/O, models, the listening pipeline, the speaking pipeline and the server.
"""
import argparse
import asyncio
import collections
import difflib
import heapq
import itertools
import json
import logging
import os
import re
import sys
import threading
import time

import numpy as np

VERSION = 1
SR_IN = 16000            # microphone / VAD / STT / speaker sample rate
BLOCK = 512              # 32 ms at 16 kHz (Silero VAD's frame)
BLOCK_S = BLOCK / SR_IN
PRE_ROLL_BLOCKS = 10     # 320 ms of audio kept before a capture starts
START_BLOCKS = 3         # ~96 ms of voiced audio starts a VAD capture
VAD_ON, VAD_OFF = 0.5, 0.35
MAX_CAPTURE_S = 60.0
WAKE_WAIT_S = 6.0        # after a wake word, how long to wait for the command to start
ECHO_HOLDOFF_S = 0.35    # after playback ends, ignore speech starts this long (room echo)
PARTIAL_EVERY_S = 0.5
ENROLL_NEEDED_S = 20.0
ENROLL_MIN_S = 6.0

WHISPER = "openai/whisper-large-v3-turbo"
QWEN_BASE = "Qwen/Qwen3-TTS-12Hz-1.7B-Base"
QWEN_DESIGN = "Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign"
ECAPA = "speechbrain/spkrec-ecapa-voxceleb"
OWW_BUILTINS = {"alexa": "alexa", "hey jarvis": "hey_jarvis", "hey mycroft": "hey_mycroft", "hey rhasspy": "hey_rhasspy"}
DEFAULT_VOICE_DESC = ("A warm, clear, friendly female voice in her thirties, natural and relaxed, "
                      "speaking at a moderate pace with gentle intonation.")
DEFAULT_VOICE_TEXT = "Hi, I'm here. Tell me what you need, and I'll get it done for you."
TTS_LANGS = {"en": "English", "zh": "Chinese", "de": "German", "it": "Italian", "pt": "Portuguese",
             "es": "Spanish", "ja": "Japanese", "ko": "Korean", "fr": "French", "ru": "Russian"}
HALLUCINATIONS = {"", "you", "thank you", "thank you.", "thanks for watching!", "thanks for watching.", "bye.", "."}

log = logging.getLogger("voice")


# ---------------------------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------------------------

def resample(x, sr_from, sr_to):
    if sr_from == sr_to or len(x) == 0:
        return np.asarray(x, dtype=np.float32)
    from math import gcd
    from scipy.signal import resample_poly
    g = gcd(int(sr_from), int(sr_to))
    return resample_poly(np.asarray(x, dtype=np.float32), sr_to // g, sr_from // g).astype(np.float32)


def load_wav(path, sr=None):
    import soundfile as sf
    x, r = sf.read(path, dtype="float32", always_2d=True)
    x = x.mean(axis=1)
    if sr is not None and r != sr:
        x, r = resample(x, r, sr), sr
    return x.astype(np.float32), r


def norm_words(text):
    return re.sub(r"[^\w\s']", " ", (text or "").lower()).split()


def split_sentences(text, max_chars=220):
    """Split text into speakable sentences; very long sentences are split at commas/semicolons."""
    out = []
    for para in re.split(r"\n\s*\n|\n", text or ""):
        para = re.sub(r"\s+", " ", para).strip()
        if not para:
            continue
        for s in re.split("(?<=[.!?\u2026\u3002\uff01\uff1f])\\s+", para):
            s = s.strip()
            while len(s) > max_chars:
                cut = max(s.rfind(c, 0, max_chars) for c in (",", ";", ":", " \u2013", " -"))
                if cut < max_chars // 3:
                    cut = s.rfind(" ", 0, max_chars)
                if cut <= 0:
                    cut = max_chars
                out.append(s[:cut + 1].strip())
                s = s[cut + 1:].strip()
            if s:
                out.append(s)
    return [s for s in out if re.search(r"\w", s)]


def match_wake(text, phrase):
    """If `text` starts with the wake phrase (fuzzy), return the rest of the text, else None."""
    want = norm_words(phrase)
    words = norm_words(text)
    if not want or not words:
        return None
    target = " ".join(want)
    best, best_end = 0.0, None
    for start in range(0, min(3, len(words))):          # allow a filler word before it ("okay hey craft")
        for end in range(start + 1, min(len(words), start + len(want) + 2) + 1):
            r = difflib.SequenceMatcher(None, " ".join(words[start:end]), target).ratio()
            if r > best:
                best, best_end = r, end
    if best < 0.75 or best_end is None:
        return None
    # rest of the original text after the matched words
    raw = re.findall(r"[\w']+|[^\w\s']+|\s+", text)
    seen, cut = 0, len(raw)
    for i, tok in enumerate(raw):
        if re.match(r"[\w']+", tok):
            seen += 1
            if seen == best_end:
                cut = i + 1
                break
    rest = "".join(raw[cut:]).lstrip(" ,.!?;:-").strip()
    return rest


def pid_alive(pid, create_time=None):
    try:
        import psutil
        p = psutil.Process(pid)
        if create_time is not None and abs(p.create_time() - create_time) > 1.0:
            return False
        return p.is_running() and p.status() != psutil.STATUS_ZOMBIE
    except Exception:
        return False


def resolve_model(repo, models_dir, download=True):
    """Local snapshot path: the default HF cache first, then <models_dir>/hf (downloaded there if missing)."""
    from huggingface_hub import snapshot_download
    try:
        return snapshot_download(repo, local_files_only=True)
    except Exception:
        pass
    cache = os.path.join(models_dir, "hf")
    try:
        return snapshot_download(repo, cache_dir=cache, local_files_only=True)
    except Exception:
        if not download:
            return None
    log.info("downloading %s into %s", repo, cache)
    return snapshot_download(repo, cache_dir=cache)


def check_models(models_dir):
    out = {}
    for key, repo in (("stt", WHISPER), ("tts", QWEN_BASE), ("voice_design", QWEN_DESIGN)):
        try:
            out[key] = resolve_model(repo, models_dir, download=False) is not None
        except Exception:
            out[key] = False
    out["speaker"] = os.path.exists(os.path.join(models_dir, "speechbrain", "spkrec-ecapa-voxceleb", "embedding_model.ckpt"))
    oww = os.path.join(models_dir, "openwakeword")
    out["wake_word"] = os.path.exists(os.path.join(oww, "melspectrogram.onnx"))
    try:
        import silero_vad  # noqa: F401
        out["vad"] = True
    except Exception:
        out["vad"] = False
    out["default_voice"] = os.path.exists(os.path.join(models_dir, "voices", "default.wav"))
    out["speaker_profile"] = os.path.exists(os.path.join(models_dir, "speaker", "profile.json"))
    return out


# ---------------------------------------------------------------------------------------------
# audio I/O (behind an interface so tests can use WAV fixtures instead of a microphone)
# ---------------------------------------------------------------------------------------------

class Player:
    """Output buffer: chunks pushed by the speaker thread, pulled by the device callback."""

    def __init__(self, sr):
        self.sr = sr
        self.lock = threading.Lock()
        self.chunks = collections.deque()
        self.offset = 0
        self.pending_samples = 0
        self.epoch = 0                  # bumped by clear(), so a stale producer can't refill
        self.last_audio_t = 0.0         # wall time the last non-silent sample was handed out

    def push(self, x, epoch=None):
        x = np.asarray(x, dtype=np.float32)
        if x.size == 0:
            return
        with self.lock:
            if epoch is not None and epoch != self.epoch:
                return
            self.chunks.append(x)
            self.pending_samples += x.size

    def read(self, n):
        out = np.zeros(n, dtype=np.float32)
        got = 0
        with self.lock:
            while got < n and self.chunks:
                c = self.chunks[0]
                take = min(n - got, c.size - self.offset)
                out[got:got + take] = c[self.offset:self.offset + take]
                got += take
                self.offset += take
                if self.offset >= c.size:
                    self.chunks.popleft()
                    self.offset = 0
            self.pending_samples -= got
            if got:
                self.last_audio_t = time.monotonic()
        return out

    def clear(self):
        with self.lock:
            self.chunks.clear()
            self.offset = 0
            self.pending_samples = 0
            self.epoch += 1
            return self.epoch

    def pending(self):
        with self.lock:
            return self.pending_samples


class AudioIO:
    """Interface. Input blocks are float32 mono at 16 kHz, BLOCK samples each."""
    out_sr = 24000

    def __init__(self):
        self.player = Player(self.out_sr)

    def list_devices(self):
        return [], []

    def open_input(self, device, on_block):
        raise NotImplementedError

    def close_input(self):
        pass

    def set_output_device(self, device):
        pass

    def play(self, samples, sr, epoch=None):
        self.ensure_output()
        self.player.push(resample(samples, sr, self.player.sr), epoch)

    def ensure_output(self):
        pass

    def flush(self):
        return self.player.clear()

    def pending_seconds(self):
        return self.player.pending() / float(self.player.sr)

    def close(self):
        self.close_input()


class SoundDeviceIO(AudioIO):
    """Real microphone and speakers through PortAudio (sounddevice)."""

    def __init__(self):
        import sounddevice as sd
        self.sd = sd
        self.out_device = None
        self.out_stream = None
        self.in_stream = None
        self.out_lock = threading.Lock()
        super().__init__()

    def _hostapi(self):
        try:
            d = self.sd.query_devices(kind="input")
            return d["hostapi"]
        except Exception:
            try:
                return self.sd.query_devices(kind="output")["hostapi"]
            except Exception:
                return 0

    def list_devices(self):
        api = self._hostapi()
        ins, outs = [], []
        for d in self.sd.query_devices():
            if d["hostapi"] != api:
                continue
            if d["max_input_channels"] > 0:
                ins.append(d["name"])
            if d["max_output_channels"] > 0:
                outs.append(d["name"])
        return ins, outs

    def _find(self, name, kind):
        if not name:
            return None
        api = self._hostapi()
        key = "max_input_channels" if kind == "input" else "max_output_channels"
        devs = [(i, d) for i, d in enumerate(self.sd.query_devices()) if d[key] > 0]
        for pref in (lambda d: d["hostapi"] == api and d["name"] == name,
                     lambda d: d["name"] == name,
                     lambda d: d["hostapi"] == api and name.lower() in d["name"].lower(),
                     lambda d: name.lower() in d["name"].lower()):
            for i, d in devs:
                if pref(d):
                    return i
        raise ValueError(f"no {kind} device named {name!r}")

    def open_input(self, device, on_block):
        self.close_input()
        idx = self._find(device, "input")
        buf = {"x": np.zeros(0, dtype=np.float32)}

        def make(rate):
            def cb(indata, frames, t, status):
                x = indata[:, 0].astype(np.float32) if indata.ndim == 2 else indata.astype(np.float32)
                if rate != SR_IN:
                    x = resample(x, rate, SR_IN)
                b = np.concatenate([buf["x"], x])
                n = (b.size // BLOCK) * BLOCK
                for i in range(0, n, BLOCK):
                    on_block(b[i:i + BLOCK].copy())
                buf["x"] = b[n:]
            return cb

        try:
            st = self.sd.InputStream(samplerate=SR_IN, blocksize=BLOCK, channels=1, dtype="float32",
                                     device=idx, callback=make(SR_IN))
        except Exception:
            info = self.sd.query_devices(idx if idx is not None else self.sd.default.device[0])
            rate = int(info["default_samplerate"])
            st = self.sd.InputStream(samplerate=rate, blocksize=int(rate * BLOCK_S), channels=1,
                                     dtype="float32", device=idx, callback=make(rate))
        st.start()
        self.in_stream = st

    def close_input(self):
        st, self.in_stream = self.in_stream, None
        if st is not None:
            try:
                st.stop()
                st.close()
            except Exception:
                pass

    def set_output_device(self, device):
        with self.out_lock:
            new = self._find(device, "output") if device else None
            if new == self.out_device and self.out_stream is not None:
                return
            self.out_device = new
            self._close_output()

    def _close_output(self):
        st, self.out_stream = self.out_stream, None
        if st is not None:
            try:
                st.stop()
                st.close()
            except Exception:
                pass

    def ensure_output(self):
        with self.out_lock:
            if self.out_stream is not None:
                return

            def cb(outdata, frames, t, status):
                outdata[:, 0] = self.player.read(frames)
                if outdata.shape[1] > 1:
                    outdata[:, 1:] = outdata[:, :1]

            try:
                st = self.sd.OutputStream(samplerate=self.player.sr, channels=1, dtype="float32",
                                          device=self.out_device, callback=cb, blocksize=0)
            except Exception:
                info = self.sd.query_devices(self.out_device if self.out_device is not None else self.sd.default.device[1])
                rate = int(info["default_samplerate"])
                self.player.clear()
                self.player.sr = rate
                st = self.sd.OutputStream(samplerate=rate, channels=min(2, int(info["max_output_channels"]) or 1),
                                          dtype="float32", device=self.out_device, callback=cb, blocksize=0)
            st.start()
            self.out_stream = st

    def close(self):
        self.close_input()
        with self.out_lock:
            self._close_output()


class FakeAudioIO(AudioIO):
    """A virtual microphone fed from arrays/WAV files in real time, and a virtual speaker that records."""

    def __init__(self, inputs=("Fake Microphone",), outputs=("Fake Speakers",)):
        super().__init__()
        self.inputs, self.outputs = list(inputs), list(outputs)
        self.feed_q = collections.deque()
        self.feed_lock = threading.Lock()
        self.on_block = None
        self.running = True
        self.played = []                 # (wall time, samples) of non-silent output
        self.first_audio_t = None        # perf_counter time of the first non-silent output since reset_played()
        self.noise = 1e-4
        self.in_thread = threading.Thread(target=self._in_loop, daemon=True, name="fake-mic")
        self.out_thread = threading.Thread(target=self._out_loop, daemon=True, name="fake-speaker")
        self.in_thread.start()
        self.out_thread.start()

    def list_devices(self):
        return list(self.inputs), list(self.outputs)

    def open_input(self, device, on_block):
        if device and device not in self.inputs:
            raise ValueError(f"no input device named {device!r}")
        self.on_block = on_block

    def close_input(self):
        self.on_block = None

    def set_output_device(self, device):
        if device and device not in self.outputs:
            raise ValueError(f"no output device named {device!r}")

    def feed(self, audio, sr=SR_IN):
        """Queue audio (array or WAV path) to be 'spoken into the microphone' in real time."""
        if isinstance(audio, str):
            audio, sr = load_wav(audio)
        audio = resample(np.asarray(audio, dtype=np.float32), sr, SR_IN)
        with self.feed_lock:
            for i in range(0, audio.size, BLOCK):
                b = audio[i:i + BLOCK]
                if b.size < BLOCK:
                    b = np.pad(b, (0, BLOCK - b.size))
                self.feed_q.append(b)
        return audio.size / SR_IN

    def feed_remaining_s(self):
        with self.feed_lock:
            return len(self.feed_q) * BLOCK_S

    def reset_played(self):
        self.played = []
        self.first_audio_t = None

    def played_audio(self):
        if not self.played:
            return np.zeros(0, dtype=np.float32)
        return np.concatenate([p for _, p in self.played])

    def _in_loop(self):
        rng = np.random.default_rng(1)
        nxt = time.perf_counter()
        while self.running:
            with self.feed_lock:
                b = self.feed_q.popleft() if self.feed_q else None
            if b is None:
                b = (rng.standard_normal(BLOCK) * self.noise).astype(np.float32)
            cb = self.on_block
            if cb is not None:
                cb(b)
            nxt += BLOCK_S
            d = nxt - time.perf_counter()
            if d > 0:
                time.sleep(d)
            else:
                nxt = time.perf_counter()

    def _out_loop(self):
        step = 0.02
        nxt = time.perf_counter()
        while self.running:
            x = self.player.read(int(self.player.sr * step))
            if np.any(x):
                now = time.perf_counter()
                if self.first_audio_t is None:
                    self.first_audio_t = now
                self.played.append((now, x))
            nxt += step
            d = nxt - time.perf_counter()
            if d > 0:
                time.sleep(d)
            else:
                nxt = time.perf_counter()

    def close(self):
        self.running = False
        self.on_block = None


# ---------------------------------------------------------------------------------------------
# models
# ---------------------------------------------------------------------------------------------

class SileroVAD:
    def __init__(self):
        import torch
        from silero_vad import load_silero_vad
        torch.set_num_threads(max(1, min(4, os.cpu_count() or 1)))
        self.torch = torch
        self.model = load_silero_vad(onnx=False)

    def reset(self):
        self.model.reset_states()

    def prob(self, block):
        with self.torch.inference_mode():
            return float(self.model(self.torch.from_numpy(block), SR_IN).item())


class WhisperSTT:
    def __init__(self, models_dir, device):
        import torch
        from transformers import WhisperForConditionalGeneration, WhisperProcessor
        self.torch = torch
        path = resolve_model(WHISPER, models_dir)
        self.dev = device
        self.dtype = torch.float16 if device.startswith("cuda") else torch.float32
        self.proc = WhisperProcessor.from_pretrained(path)
        self.model = WhisperForConditionalGeneration.from_pretrained(
            path, torch_dtype=self.dtype, attn_implementation="sdpa").to(device).eval()
        self.lock = threading.Lock()

    def transcribe(self, audio, language=None):
        torch = self.torch
        audio = np.asarray(audio, dtype=np.float32)
        long_form = audio.size > 30 * SR_IN
        kw = dict(sampling_rate=SR_IN, return_tensors="pt")
        if long_form:
            kw.update(truncation=False, padding="longest", return_attention_mask=True)
        feats = self.proc.feature_extractor(audio, **kw)
        x = feats.input_features.to(self.dev, self.dtype)
        gen = dict(task="transcribe", max_new_tokens=440 if not long_form else None)
        if long_form:
            gen.update(attention_mask=feats.attention_mask.to(self.dev), return_timestamps=True)
            gen.pop("max_new_tokens")
        lang = language
        with self.lock, torch.inference_mode():
            if not lang:
                try:
                    tok = self.model.detect_language(x[:, :, :3000])
                    lang = self.proc.tokenizer.decode(tok[0]).strip("<|>") or None
                except Exception:
                    lang = None
            if lang:
                gen["language"] = lang
            ids = self.model.generate(input_features=x, **gen)
        text = self.proc.batch_decode(ids, skip_special_tokens=True)[0].strip()
        return text, lang or "en"


class EcapaSpeaker:
    """SpeechBrain ECAPA-TDNN speaker embeddings (192-d, cosine similarity)."""

    def __init__(self, models_dir, device="cpu"):
        import torch
        from speechbrain.inference.speaker import EncoderClassifier
        try:
            from speechbrain.utils.fetching import LocalStrategy
            extra = {"local_strategy": LocalStrategy.COPY}
        except Exception:
            extra = {}
        self.torch = torch
        save = os.path.join(models_dir, "speechbrain", "spkrec-ecapa-voxceleb")
        self.model = EncoderClassifier.from_hparams(source=ECAPA, savedir=save, run_opts={"device": device}, **extra)
        self.model.eval()
        self.lock = threading.Lock()

    def embed(self, audio):
        audio = np.asarray(audio, dtype=np.float32)
        if audio.size < SR_IN // 4:
            audio = np.pad(audio, (0, SR_IN // 4 - audio.size))
        with self.lock, self.torch.inference_mode():
            e = self.model.encode_batch(self.torch.from_numpy(audio)[None]).reshape(-1).float().cpu().numpy()
        n = np.linalg.norm(e)
        return e / n if n > 0 else e


class OwwWake:
    """openWakeWord built-in model (or a custom .onnx path)."""

    def __init__(self, models_dir, name_or_path):
        import openwakeword
        from openwakeword.model import Model
        from openwakeword.utils import download_models
        d = os.path.join(models_dir, "openwakeword")
        if os.path.isfile(name_or_path):
            model_path = name_or_path
            self.key = os.path.splitext(os.path.basename(name_or_path))[0]
        else:
            fname = os.path.basename(openwakeword.MODELS[name_or_path]["download_url"]).replace(".tflite", ".onnx")
            model_path = os.path.join(d, fname)
            if not os.path.exists(model_path) or not os.path.exists(os.path.join(d, "melspectrogram.onnx")):
                os.makedirs(d, exist_ok=True)
                download_models([name_or_path], target_directory=d)
            self.key = os.path.splitext(fname)[0]
        self.model = Model(wakeword_models=[model_path], inference_framework="onnx",
                           melspec_model_path=os.path.join(d, "melspectrogram.onnx"),
                           embedding_model_path=os.path.join(d, "embedding_model.onnx"))
        self.buf = np.zeros(0, dtype=np.int16)

    def reset(self):
        try:
            self.model.reset()
        except Exception:
            pass
        self.buf = np.zeros(0, dtype=np.int16)

    def score(self, block):
        """Feed one 16 kHz float block; returns the latest score (0..1) every 80 ms, else None."""
        self.buf = np.concatenate([self.buf, (np.clip(block, -1, 1) * 32767).astype(np.int16)])
        if self.buf.size < 1280:
            return None
        x, self.buf = self.buf[:1280], self.buf[1280:]
        pred = self.model.predict(x)
        return float(max(pred.values())) if pred else 0.0


class _Captured(Exception):
    pass


class _StaticKV:
    """Fixed-size KV cache with the `update` interface the Qwen3-TTS attention layers call."""

    def __init__(self, n_layers, n_kv, max_len, head_dim, dtype, dev):
        import torch
        self.k = [torch.zeros(1, n_kv, max_len, head_dim, dtype=dtype, device=dev) for _ in range(n_layers)]
        self.v = [torch.zeros(1, n_kv, max_len, head_dim, dtype=dtype, device=dev) for _ in range(n_layers)]

    def update(self, k, v, layer_idx, cache_kwargs):
        pos = cache_kwargs["cache_position"]
        self.k[layer_idx].index_copy_(2, pos, k)
        self.v[layer_idx].index_copy_(2, pos, v)
        return self.k[layer_idx], self.v[layer_idx]


def _run_layers(model, x, pos_emb, cache_pos, mask, kv):
    for layer in model.layers:
        out = layer(x, attention_mask=mask, past_key_values=kv, cache_position=cache_pos, position_embeddings=pos_emb)
        x = out[0] if isinstance(out, tuple) else out
    return model.norm(x)


def _sample(logits, temperature, top_k, do_sample):
    import torch
    logits = logits.float()
    if not do_sample:
        return logits.argmax(-1, keepdim=True)
    logits = logits / max(temperature, 1e-5)
    if top_k:
        v, _ = torch.topk(logits, min(int(top_k), logits.shape[-1]))
        logits = logits.masked_fill(logits < v[..., -1:], float("-inf"))
    probs = logits.softmax(-1)
    q = torch.empty_like(probs).exponential_(1)       # Gumbel-max: same distribution as multinomial
    return (probs / q).argmax(-1, keepdim=True)


class FastQwenTalker:
    """Streaming frame generator for Qwen3-TTS (12 Hz tokenizer).

    The stock `generate` is a Hugging Face generate loop that calls a second generate per audio frame;
    on Windows/WDDM its kernel-launch overhead makes it slower than real time. This runs the same
    modules frame by frame with static KV caches and replays one CUDA graph per frame (code predictor:
    15 codebooks + one talker step), yielding each frame's 16 codes as soon as it exists. The prompt
    is built by the library itself (we intercept the talker.generate call it makes).
    """

    def __init__(self, tts, max_len=1024):
        import torch
        self.torch = torch
        self.tts = tts
        self.talker = tts.model.talker
        self.cp = self.talker.code_predictor
        tc, cc = self.talker.config, self.cp.config
        p = next(self.talker.parameters())
        self.dev, self.dtype = p.device, p.dtype
        self.max_len = max_len
        self.ngroups = tc.num_code_groups
        self.kv = _StaticKV(tc.num_hidden_layers, tc.num_key_value_heads, max_len, tc.head_dim, self.dtype, self.dev)
        self.cpkv = _StaticKV(cc.num_hidden_layers, cc.num_key_value_heads, self.ngroups, cc.head_dim, self.dtype, self.dev)
        g = getattr(tts, "generate_defaults", {}) or {}
        self.sub_temp = float(g.get("subtalker_temperature", 0.9))
        self.sub_k = int(g.get("subtalker_top_k", 50))
        self.sub_sample = bool(g.get("subtalker_dosample", True))
        D = tc.hidden_size
        z = lambda *s, dt=None: torch.zeros(*s, dtype=dt or self.dtype, device=self.dev)  # noqa: E731
        self.s_tok = z(1, 1, dt=torch.long)
        self.s_past = z(1, 1, D)
        self.s_text = z(1, 1, D)
        self.s_pos = z(1, dt=torch.long)
        self.s_codes = z(1, self.ngroups, dt=torch.long)
        self.s_logits = z(1, tc.vocab_size, dt=torch.float32)
        self.s_hidden = z(1, 1, D)
        self.ar = torch.arange(max_len, device=self.dev)
        self.cp_pos0 = torch.arange(2, device=self.dev)
        self.cp_pos = [torch.tensor([i], device=self.dev) for i in range(self.ngroups)]
        car = torch.arange(self.ngroups, device=self.dev)
        self.cp_mask0 = (car[None, :] <= self.cp_pos0[:, None])[None, None]
        self.cp_masks = [(car <= i)[None, None, None, :] for i in range(self.ngroups)]
        self.graph = None

    def _frame(self):
        torch, talker, cp = self.torch, self.talker, self.cp
        last_id = talker.get_input_embeddings()(self.s_tok)
        x = cp.small_to_mtp_projection(torch.cat([self.s_past, last_id], 1))
        pe = cp.model.rotary_emb(x, self.cp_pos0[None])
        h = _run_layers(cp.model, x, pe, self.cp_pos0, self.cp_mask0, self.cpkv)
        tok = _sample(cp.lm_head[0](h[:, -1]), self.sub_temp, self.sub_k, self.sub_sample)
        codes = [self.s_tok.view(1, 1), tok]
        emb = last_id + cp.get_input_embeddings()[0](tok)
        for j in range(1, self.ngroups - 1):
            pos = self.cp_pos[j + 1]
            x = cp.small_to_mtp_projection(cp.get_input_embeddings()[j - 1](tok))
            pe = cp.model.rotary_emb(x, pos[None])
            h = _run_layers(cp.model, x, pe, pos, self.cp_masks[j + 1], self.cpkv)
            tok = _sample(cp.lm_head[j](h[:, -1]), self.sub_temp, self.sub_k, self.sub_sample)
            codes.append(tok)
            emb = emb + cp.get_input_embeddings()[j](tok)
        self.s_codes.copy_(torch.cat(codes, 1))
        x = emb + self.s_text
        mask = (self.ar <= self.s_pos)[None, None, None, :]
        pe = talker.model.rotary_emb(x, self.s_pos.view(1, 1, 1).expand(3, 1, 1))
        h = _run_layers(talker.model, x, pe, self.s_pos, mask, self.kv)
        self.s_hidden.copy_(h)
        self.s_logits.copy_(talker.codec_head(h)[:, -1].float())

    def capture(self):
        torch = self.torch
        if self.dev.type != "cuda":
            return
        s = torch.cuda.Stream(device=self.dev)
        s.wait_stream(torch.cuda.current_stream(self.dev))
        with torch.inference_mode(), torch.cuda.stream(s):
            for _ in range(3):
                self._frame()
        torch.cuda.current_stream(self.dev).wait_stream(s)
        g = torch.cuda.CUDAGraph()
        with torch.inference_mode(), torch.cuda.graph(g):
            self._frame()
        self.graph = g

    def _prompt(self, text, prompt, language):
        talker, cap = self.talker, {}

        def grab(**kw):
            cap.update(kw)
            raise _Captured()

        talker.generate = grab
        try:
            self.tts.generate_voice_clone(text=text, language=language, voice_clone_prompt=prompt)
        except _Captured:
            pass
        finally:
            del talker.generate
        if not cap:
            raise RuntimeError("could not build the TTS prompt")
        return cap

    def frames(self, text, prompt, language="English", cancel=None):
        torch = self.torch
        with torch.inference_mode():
            cap = self._prompt(text, prompt, language)
            E, T, P = cap["inputs_embeds"], cap["trailing_text_hidden"], cap["tts_pad_embed"]
            eos = int(cap["eos_token_id"])
            supp = torch.tensor(cap["suppress_tokens"], device=self.dev)
            rp = float(cap["repetition_penalty"])
            temp, top_k, do_sample = float(cap["temperature"]), int(cap["top_k"]), bool(cap["do_sample"])
            L = E.shape[1]
            limit = self.max_len - L - 1
            if limit <= 8:
                raise ValueError("sentence too long for one TTS chunk")
            pos = torch.arange(L, device=self.dev)
            mask = (self.ar[None, :] <= pos[:, None])[None, None]
            pe = self.talker.model.rotary_emb(E, pos.view(1, 1, -1).expand(3, 1, -1))
            h = _run_layers(self.talker.model, E, pe, pos, mask, self.kv)
            logits = self.talker.codec_head(h[:, -1]).float()
            self.s_hidden.copy_(h[:, -1:])
            seen = torch.zeros(1, logits.shape[-1], dtype=torch.bool, device=self.dev)
            n_text = T.shape[1]
        for step in range(limit + 1):
            if cancel is not None and cancel.is_set():
                return
            with torch.inference_mode():
                lg = logits.clone()
                lg[:, supp] = float("-inf")
                if step < 2:
                    lg[:, eos] = float("-inf")
                if rp != 1.0:
                    lg = torch.where(seen, torch.where(lg < 0, lg * rp, lg / rp), lg)
                tok = _sample(lg, temp, top_k, do_sample)
                t = int(tok)
                if t == eos or step == limit:
                    return
                seen[0, t] = True
                self.s_tok.copy_(tok)
                self.s_past.copy_(self.s_hidden)
                self.s_text.copy_(T[:, step:step + 1] if step < n_text else P.reshape(1, 1, -1))
                self.s_pos.fill_(L + step)
                if self.graph is not None:
                    self.graph.replay()
                else:
                    self._frame()
                logits = self.s_logits
                codes = self.s_codes[0].clone()
            yield codes


class QwenTTS:
    """Qwen3-TTS Base, voice-cloned from the assistant voice (ref_audio + ref_text), streamed."""
    sr = 24000

    def __init__(self, models_dir, device):
        import torch
        from qwen_tts import Qwen3TTSModel
        self.torch = torch
        self.dev = device
        dtype = torch.bfloat16 if device.startswith("cuda") else torch.float32
        path = resolve_model(QWEN_BASE, models_dir)
        self.model = Qwen3TTSModel.from_pretrained(path, device_map=device, dtype=dtype)
        self.talker = FastQwenTalker(self.model)
        self.talker.capture()
        self.decoder = self.model.model.speech_tokenizer.model.decoder
        self.up = int(self.model.model.speech_tokenizer.model.get_decode_upsample_rate())
        self.sr = int(self.model.model.speech_tokenizer.model.get_output_sample_rate())
        try:
            self.langs = {s.lower() for s in (self.model.get_supported_languages() or [])}
        except Exception:
            self.langs = set()
        self.prompt = None
        self.ref_codes = None
        self.lock = threading.Lock()

    def set_voice(self, ref_audio, ref_text):
        with self.lock, self.torch.inference_mode():
            prompt = self.model.create_voice_clone_prompt(ref_audio=ref_audio, ref_text=ref_text)
            self.prompt = prompt
            rc = prompt[0].ref_code
            self.ref_codes = rc.to(self.talker.dev) if rc is not None else None

    def language(self, code):
        name = TTS_LANGS.get((code or "").lower()[:2], None)
        if name and (not self.langs or name.lower() in self.langs):
            return name
        return "Auto" if "auto" in self.langs else "English"

    def _decode(self, ctx, new):
        torch = self.torch
        codes = new if ctx is None or ctx.shape[0] == 0 else torch.cat([ctx, new], 0)
        n_ctx = 0 if ctx is None else ctx.shape[0]
        with torch.inference_mode():
            wav = self.decoder(codes.T[None].clamp(min=0))[0, 0, n_ctx * self.up:]
        return wav.float().cpu().numpy()

    def stream(self, text, language="en", cancel=None, first_frames=4, chunk_frames=8, voice=None):
        """Yield float32 audio chunks (self.sr) for one sentence as soon as frames are generated.
        `voice` = (prompt, ref_codes) overrides the assistant voice (used by tests and previews)."""
        torch = self.torch
        with self.lock:
            prompt, ctx_src = voice if voice is not None else (self.prompt, self.ref_codes)
            if prompt is None:
                raise RuntimeError("no assistant voice set")
            frames, done = [], 0
            lead = [True]               # still inside the leading silence the model often produces

            def out(wav):
                if lead[0]:
                    on = np.flatnonzero(np.abs(wav) > 0.015)
                    if on.size == 0:
                        return None
                    wav = wav[max(0, int(on[0]) - int(0.06 * self.sr)):]
                    lead[0] = False
                return wav

            for f in self.talker.frames(text, prompt, self.language(language), cancel):
                frames.append(f)
                n = len(frames)
                if (n - done >= first_frames) if lead[0] else (n - done >= chunk_frames):
                    allf = torch.stack(frames)
                    hist = allf[:done] if ctx_src is None else torch.cat([ctx_src, allf[:done]], 0)
                    w = out(self._decode(hist[-25:], allf[done:n]))
                    done = n
                    if w is not None:
                        yield w
            if cancel is not None and cancel.is_set():
                return
            if len(frames) > done:
                allf = torch.stack(frames)
                hist = allf[:done] if ctx_src is None else torch.cat([ctx_src, allf[:done]], 0)
                w = out(self._decode(hist[-25:], allf[done:]))
                if w is not None:
                    yield w

    def voice_of(self, ref_audio, ref_text):
        """A (prompt, ref_codes) pair for `stream(voice=...)`."""
        with self.lock, self.torch.inference_mode():
            prompt = self.model.create_voice_clone_prompt(ref_audio=ref_audio, ref_text=ref_text)
            rc = prompt[0].ref_code
            return prompt, (rc.to(self.talker.dev) if rc is not None else None)

    def synthesize(self, text, language="en", voice=None):
        """Whole-text synthesis (all sentences), returned as one float32 array at self.sr."""
        parts = [c for s in split_sentences(text) for c in self.stream(s, language, voice=voice)]
        return np.concatenate(parts) if parts else np.zeros(0, dtype=np.float32)


def design_voice_file(models_dir, device, description, sample_text, language, out_path):
    """Design a new voice with Qwen3-TTS VoiceDesign (loaded for this call only, then freed)."""
    import torch
    import soundfile as sf
    from qwen_tts import Qwen3TTSModel
    dtype = torch.bfloat16 if device.startswith("cuda") else torch.float32
    path = resolve_model(QWEN_DESIGN, models_dir)
    model = Qwen3TTSModel.from_pretrained(path, device_map=device, dtype=dtype)
    try:
        wavs, sr = model.generate_voice_design(text=sample_text, instruct=description, language=language)
        os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
        sf.write(out_path, wavs[0], sr)
        with open(os.path.splitext(out_path)[0] + ".json", "w", encoding="utf-8") as f:
            json.dump({"version": 1, "description": description, "ref_text": sample_text,
                       "language": language, "created": time.time()}, f)
        return len(wavs[0]) / sr
    finally:
        del model
        if device.startswith("cuda"):
            torch.cuda.empty_cache()


# ---------------------------------------------------------------------------------------------
# stand-in models (--stub-models): deterministic, CPU-only, for protocol tests without a GPU
# ---------------------------------------------------------------------------------------------

class StubVAD:
    def reset(self):
        pass

    def prob(self, block):
        return 1.0 if float(np.sqrt(np.mean(block ** 2))) > 0.02 else 0.0


class StubSTT:
    def transcribe(self, audio, language=None):
        voiced = float(np.mean(np.abs(audio) > 0.02)) * audio.size / SR_IN
        return f"stub speech {voiced:.1f} seconds", language or "en"


class StubSpeaker:
    """Embedding = normalised log band energies; tones of different pitch get different 'voices'."""

    def embed(self, audio):
        spec = np.abs(np.fft.rfft(np.asarray(audio, dtype=np.float32)[: SR_IN * 4], n=8192))
        bands = np.array([b.mean() for b in np.array_split(spec[:2048], 32)]) + 1e-6
        e = np.log(bands)
        e = e - e.mean()
        n = np.linalg.norm(e)
        return e / n if n > 0 else e


class StubTTS:
    sr = 24000

    def set_voice(self, ref_audio, ref_text):
        self.voice = ref_audio

    def stream(self, text, language="en", cancel=None, **_):
        n = max(1, len(text) // 8)
        t = np.arange(int(self.sr * 0.1)) / self.sr
        for i in range(n):
            if cancel is not None and cancel.is_set():
                return
            time.sleep(0.02)
            yield (0.2 * np.sin(2 * np.pi * (300 + 20 * (i % 5)) * t)).astype(np.float32)


def stub_design(models_dir, device, description, sample_text, language, out_path):
    import soundfile as sf
    sr = 24000
    t = np.arange(sr * 2) / sr
    f0 = 150 + (sum(map(ord, description)) % 200)
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    sf.write(out_path, (0.2 * np.sin(2 * np.pi * f0 * t)).astype(np.float32), sr)
    with open(os.path.splitext(out_path)[0] + ".json", "w", encoding="utf-8") as f:
        json.dump({"version": 1, "description": description, "ref_text": sample_text, "language": language}, f)
    return 2.0


# ---------------------------------------------------------------------------------------------
# the service
# ---------------------------------------------------------------------------------------------

class Capture:
    _ids = itertools.count(1)

    def __init__(self, kind, pre_blocks=(), pre_voiced=()):
        self.id = next(Capture._ids)
        self.kind = kind                    # vad | ptt | wake | kws | enroll
        self.blocks = list(pre_blocks)
        self.voiced = list(pre_voiced) if pre_voiced else [False] * len(self.blocks)
        self.lock = threading.Lock()
        self.started = time.monotonic()
        self.last_voice = self.started
        self.heard_speech = any(self.voiced)
        self.done = False
        self.partial_pending = False
        self.last_partial_req = 0.0
        self.last_partial_text = ""
        self.wake_found = False             # kws: the wake phrase was seen in a partial

    def add(self, block, voiced):
        with self.lock:
            self.blocks.append(block)
            self.voiced.append(voiced)
        if voiced:
            self.last_voice = time.monotonic()
            self.heard_speech = True

    def audio(self, trim=False):
        with self.lock:
            blocks, voiced = list(self.blocks), list(self.voiced)
        if not blocks:
            return np.zeros(0, dtype=np.float32)
        if trim and any(voiced):
            last = max(i for i, v in enumerate(voiced) if v)
            blocks = blocks[: min(len(blocks), last + 8)]        # keep ~250 ms after the last voiced block
        return np.concatenate(blocks)

    def voiced_audio(self):
        with self.lock:
            blocks, voiced = list(self.blocks), list(self.voiced)
        keep = [False] * len(voiced)
        for i, v in enumerate(voiced):
            if v:
                for j in range(max(0, i - 2), min(len(voiced), i + 3)):
                    keep[j] = True
        sel = [b for b, k in zip(blocks, keep) if k]
        return np.concatenate(sel) if sel else np.zeros(0, dtype=np.float32)

    def voiced_seconds(self):
        with self.lock:
            return sum(self.voiced) * BLOCK_S

    def seconds(self):
        with self.lock:
            return len(self.blocks) * BLOCK_S


class VoiceService:
    def __init__(self, models_dir, device="cuda:0", audio=None, stub_models=False, allow_inject=False):
        self.models_dir = os.path.abspath(models_dir)
        # tests only: a WAV may be fed in as if spoken into the microphone (--allow-inject)
        self.allow_inject = allow_inject
        self.injecting = False
        os.makedirs(self.models_dir, exist_ok=True)
        self.device = device
        self.stub = stub_models
        self.audio = audio
        self.emit_cb = None
        self.lock = threading.RLock()
        self.cfg = {"listen_mode": "push_to_talk", "wake_word": "hey craft", "input_device": None,
                    "output_device": None, "speaker_threshold": 0.55, "language": "en", "tts_voice": None,
                    "vad_silence_ms": 700}
        # models
        self.vad = self.stt = self.speaker = self.tts = self.wake = None
        self.wake_engine = None            # "openwakeword" | "whisper" | None
        self.models = {"vad": "pending", "stt": "pending", "speaker": "pending", "tts": "pending", "wake_word": "off"}
        self.models_ready = False
        self.load_failed = False
        self.ready_evt = threading.Event()
        self.loading = "starting"
        # listening state
        self.listening = False
        self.capture = None
        self.ptt_down = False
        self.pre_roll = collections.deque(maxlen=PRE_ROLL_BLOCKS)
        self.pre_voiced = collections.deque(maxlen=PRE_ROLL_BLOCKS)
        self.run_voiced = 0
        self.recent_voiced = collections.deque(maxlen=12)
        self.recent_blocks = collections.deque(maxlen=24)
        self.wake_armed_until = 0.0
        self.last_wake_t = 0.0
        self.barge_checked_t = 0.0
        self.in_q = collections.deque(maxlen=2000)
        self.in_evt = threading.Event()
        # speaker profile / enrolment
        self.profile = None
        self.profile_path = os.path.join(self.models_dir, "speaker", "profile.json")
        self.enrolling = False
        self.enroll_segments = []
        self.assistant_emb = None
        # speaking
        self.speak_q = collections.deque()
        self.speak_cv = threading.Condition()
        self.current = None                 # dict(id, text, interruptible, cancel)
        self.speaking = False
        # STT jobs
        self.stt_jobs = []
        self.stt_cv = threading.Condition()
        self.stt_seq = itertools.count()
        self.stopping = threading.Event()
        self._last_state = None
        self._load_profile()
        self.threads = []
        for target, name in ((self._audio_loop, "audio"), (self._stt_loop, "stt"), (self._speak_loop, "speak")):
            t = threading.Thread(target=target, name=name, daemon=True)
            t.start()
            self.threads.append(t)

    # ----- events ------------------------------------------------------------------------------
    def emit(self, ev):
        cb = self.emit_cb
        if cb is not None:
            try:
                cb(ev)
            except Exception:
                log.exception("emit failed")

    def error(self, message, fatal=False, id=None):
        log.warning("error: %s", message)
        ev = {"type": "error", "message": str(message), "fatal": bool(fatal)}
        if id is not None:
            ev["id"] = id
        self.emit(ev)

    def state(self):
        return {"listening": bool(self.listening), "capturing": self.capture is not None and not self.capture.done,
                "speaking": bool(self.speaking), "mode": self.cfg["listen_mode"], "enrolled": self.profile is not None,
                "enrolling": bool(self.enrolling), "models_ready": bool(self.models_ready), "loading": self.loading}

    def push_state(self, force=False):
        st = self.state()
        if force or st != self._last_state:
            self._last_state = st
            self.emit(dict(type="state", **st))

    def status(self):
        st = {"type": "status", "version": VERSION, "state": self.state(), "models": dict(self.models),
              "device": self.device, "wake_engine": self.wake_engine, "wake_word": self.cfg["wake_word"],
              "config": dict(self.cfg), "voice": self._voice_desc(), "speaker_threshold": self.cfg["speaker_threshold"],
              "queue": len(self.speak_q)}
        try:
            if self.device.startswith("cuda") and not self.stub:
                import torch
                st["vram_mb"] = {"allocated": round(torch.cuda.memory_allocated() / 2 ** 20),
                                 "peak": round(torch.cuda.max_memory_allocated() / 2 ** 20),
                                 "reserved": round(torch.cuda.memory_reserved() / 2 ** 20)}
        except Exception:
            pass
        return st

    def _voice_desc(self):
        v = self.cfg.get("tts_voice")
        if v:
            return v
        p = os.path.join(self.models_dir, "voices", "default.wav")
        return {"ref_audio": p, "ref_text": DEFAULT_VOICE_TEXT, "default": True} if os.path.exists(p) else None

    # ----- loading ------------------------------------------------------------------------------
    def load_models(self):
        try:
            self._load_models()
        except Exception as e:
            log.exception("model loading failed")
            self.loading = f"failed: {e}"
            self.load_failed = True
            self.ready_evt.set()
            self.error(f"voice models failed to load: {e}", fatal=True)
        finally:
            self.push_state()

    def _set_model(self, key, status):
        self.models[key] = status
        log.info("model %s: %s", key, status)

    def _load_models(self):
        t0 = time.time()
        if self.stub:
            self.vad, self.stt, self.speaker, self.tts = StubVAD(), StubSTT(), StubSpeaker(), StubTTS()
            for k in ("vad", "stt", "speaker", "tts"):
                self._set_model(k, "ready")
        else:
            self.loading = "vad"
            self.push_state()
            self.vad = SileroVAD()
            self._set_model("vad", "ready")
            self.loading = "speech recognition"
            self.push_state()
            self.stt = WhisperSTT(self.models_dir, self.device)
            self._set_model("stt", "ready")
            self.loading = "speaker verification"
            self.push_state()
            try:
                self.speaker = EcapaSpeaker(self.models_dir, "cpu")
                self._set_model("speaker", "ready")
            except Exception as e:
                self._set_model("speaker", f"error: {e}")
                self.error(f"speaker verification unavailable: {e}")
            if not self.cfg.get("tts_voice") and not os.path.exists(os.path.join(self.models_dir, "voices", "default.wav")):
                self.loading = "designing the default assistant voice"
                self.push_state()
                try:
                    design_voice_file(self.models_dir, self.device, DEFAULT_VOICE_DESC, DEFAULT_VOICE_TEXT, "English",
                                      os.path.join(self.models_dir, "voices", "default.wav"))
                except Exception as e:
                    log.exception("default voice design failed")
                    self.error(f"the default assistant voice could not be designed: {e}")
            self.loading = "speech synthesis"
            self.push_state()
            self.tts = QwenTTS(self.models_dir, self.device)
            self._set_model("tts", "ready")
        self._apply_voice()
        self.loading = "warming up"
        self.push_state()
        self._warmup()
        self._setup_wake()
        self.loading = ""
        self.models_ready = True
        self.ready_evt.set()
        log.info("models ready in %.1fs", time.time() - t0)
        self.push_state()
        self.emit(self.status())

    def _warmup(self):
        if self.stub:
            return
        try:
            silence = np.zeros(SR_IN, dtype=np.float32)
            self.stt.transcribe(silence, self.cfg.get("language") or "en")
            if self.tts is not None and self.tts.prompt is not None:
                for _ in self.tts.stream("Ready.", "en"):
                    pass
            if self.speaker is not None:
                self.speaker.embed(silence)
        except Exception:
            log.exception("warm-up failed")

    def _apply_voice(self):
        if self.tts is None:
            return
        v = self._voice_desc()
        if not v and self.stub:
            return
        if not v:
            self.error("no assistant voice: design one with design_voice or set tts_voice")
            return
        try:
            self.tts.set_voice(v["ref_audio"], v.get("ref_text") or "")
        except Exception as e:
            self.error(f"assistant voice could not be loaded: {e}")
            return
        if self.speaker is not None:
            try:
                a, _ = load_wav(v["ref_audio"], SR_IN)
                self.assistant_emb = self.speaker.embed(a)
            except Exception:
                self.assistant_emb = None

    def _setup_wake(self):
        self.wake, self.wake_engine = None, None
        if self.cfg["listen_mode"] != "wake_word":
            self.models["wake_word"] = "off"
            return
        phrase = (self.cfg.get("wake_word") or "").strip()
        key = OWW_BUILTINS.get(" ".join(norm_words(phrase)).replace("_", " "))
        if os.path.isfile(phrase) or key:
            if self.stub:
                self.wake_engine = "whisper"
            else:
                try:
                    self.wake = OwwWake(self.models_dir, phrase if os.path.isfile(phrase) else key)
                    self.wake_engine = "openwakeword"
                except Exception as e:
                    self.error(f"openWakeWord unavailable ({e}); spotting the wake word with Whisper instead")
                    self.wake_engine = "whisper"
        else:
            self.wake_engine = "whisper"
        self.models["wake_word"] = self.wake_engine

    # ----- profile ------------------------------------------------------------------------------
    def _load_profile(self):
        try:
            with open(self.profile_path, encoding="utf-8") as f:
                p = json.load(f)
            if p.get("version") == 1 and p.get("embedding"):
                e = np.asarray(p["embedding"], dtype=np.float32)
                self.profile = e / (np.linalg.norm(e) or 1.0)
        except FileNotFoundError:
            pass
        except Exception as e:
            log.warning("speaker profile unreadable: %s", e)

    def speaker_score(self, audio):
        if self.profile is None or self.speaker is None or audio.size < SR_IN // 4:
            return None
        e = self.speaker.embed(audio)
        if e.shape != self.profile.shape:
            return None
        return float(np.dot(e, self.profile))

    # ----- config & commands ---------------------------------------------------------------------
    def configure(self, c):
        errs = []
        mode = c.get("listen_mode", self.cfg["listen_mode"])
        if mode not in ("push_to_talk", "wake_word", "always"):
            errs.append(f"unknown listen_mode {mode!r}")
            mode = self.cfg["listen_mode"]
        old = dict(self.cfg)
        self.cfg["listen_mode"] = mode
        for k in ("wake_word", "language"):
            if k in c:
                self.cfg[k] = c[k]
        if "speaker_threshold" in c:
            try:
                self.cfg["speaker_threshold"] = min(1.0, max(-1.0, float(c["speaker_threshold"])))
            except (TypeError, ValueError):
                errs.append("speaker_threshold must be a number")
        if "vad_silence_ms" in c:
            try:
                self.cfg["vad_silence_ms"] = int(min(5000, max(200, int(c["vad_silence_ms"]))))
            except (TypeError, ValueError):
                errs.append("vad_silence_ms must be a number")
        if "tts_voice" in c:
            v = c["tts_voice"]
            if v is None:
                self.cfg["tts_voice"] = None
            elif isinstance(v, dict) and v.get("ref_audio") and os.path.isfile(v["ref_audio"]):
                self.cfg["tts_voice"] = {"ref_audio": v["ref_audio"], "ref_text": v.get("ref_text") or ""}
            else:
                errs.append("tts_voice.ref_audio is not a readable file")
        if "output_device" in c:
            try:
                self.audio.set_output_device(c["output_device"])
                self.cfg["output_device"] = c["output_device"]
            except Exception as e:
                errs.append(f"output device: {e}")
        if "input_device" in c:
            self.cfg["input_device"] = c["input_device"]
        if self.cfg["input_device"] != old["input_device"] or not self.listening:
            self.open_mic(errs)
        if self.cfg["tts_voice"] != old["tts_voice"] and self.models_ready:
            threading.Thread(target=self._apply_voice, daemon=True).start()
        if (self.cfg["listen_mode"], self.cfg["wake_word"]) != (old["listen_mode"], old["wake_word"]) and self.models_ready:
            threading.Thread(target=lambda: (self._setup_wake(), self.push_state()), daemon=True).start()
        if mode != old["listen_mode"]:
            with self.lock:
                if self.capture is not None and self.capture.kind != "ptt" and not self.enrolling:
                    self.capture.done = True
                    self.capture = None
        self.push_state()
        return errs

    def open_mic(self, errs=None):
        try:
            self.audio.open_input(self.cfg["input_device"], self._on_block)
            self.listening = True
        except Exception as e:
            self.listening = False
            msg = f"microphone unavailable: {e}"
            if errs is not None:
                errs.append(msg)
            else:
                self.error(msg)
        self.push_state()

    def set_ptt(self, down):
        with self.lock:
            if down and not self.ptt_down:
                self.ptt_down = True
                self._barge_in(reason="ptt")
                if self.capture is not None and not self.capture.done:
                    self.capture.kind = "enroll" if self.enrolling else "ptt"
                else:
                    kind = "enroll" if self.enrolling else "ptt"
                    pre = list(self.pre_roll)[-6:]
                    self.capture = Capture(kind, pre, list(self.pre_voiced)[-6:])
            elif not down and self.ptt_down:
                self.ptt_down = False
                if self.capture is not None and self.capture.kind in ("ptt", "enroll") and not self.capture.done:
                    self._finish_capture()
        self.push_state()

    # ----- audio thread ----------------------------------------------------------------------------
    def _on_block(self, block, injected=False):
        if self.injecting and not injected:
            return                      # the microphone is muted while a test WAV is fed in
        self.in_q.append(block)
        self.in_evt.set()

    def inject_wav(self, path, ptt=None, rid=None):
        """Tests only (--allow-inject): feed a WAV in real time as if it came from the microphone.

        The real microphone is muted meanwhile. `ptt` holds push-to-talk down around it (default:
        in push-to-talk mode, unless enrolling). Ends with {"type":"injected","seconds":...}."""
        if not self.allow_inject:
            self.error("inject_wav is refused: the service wasn't started with --allow-inject", id=rid)
            return
        if self.injecting:
            self.error("a WAV is already being injected", id=rid)
            return
        try:
            audio, sr = load_wav(path)
            audio = resample(np.asarray(audio, dtype=np.float32), sr, SR_IN)
        except Exception as e:
            self.error(f"inject_wav couldn't read {path}: {e}", id=rid)
            return
        if ptt is None:
            ptt = self.cfg["listen_mode"] == "push_to_talk" and not self.enrolling
        self.injecting = True

        def run():
            try:
                tail = np.zeros(int(SR_IN * 0.4), dtype=np.float32)
                x = np.concatenate([audio, tail])
                if ptt:
                    self.set_ptt(True)
                nxt = time.perf_counter()
                for i in range(0, x.size, BLOCK):
                    b = x[i:i + BLOCK]
                    if b.size < BLOCK:
                        b = np.pad(b, (0, BLOCK - b.size))
                    self._on_block(b, injected=True)
                    nxt += BLOCK_S
                    d = nxt - time.perf_counter()
                    if d > 0:
                        time.sleep(d)
                if ptt:
                    self.set_ptt(False)
                else:
                    # let the VAD hear the silence that ends the utterance
                    for _ in range(int(self.cfg["vad_silence_ms"] / 1000.0 / BLOCK_S) + 4):
                        self._on_block(np.zeros(BLOCK, dtype=np.float32), injected=True)
                        time.sleep(BLOCK_S)
            finally:
                self.injecting = False
            ev = {"type": "injected", "path": path, "seconds": round(audio.size / SR_IN, 2)}
            if rid is not None:
                ev["id"] = rid
            self.emit(ev)

        threading.Thread(target=run, name="inject", daemon=True).start()

    def _audio_loop(self):
        while not self.stopping.is_set():
            if not self.in_q:
                self.in_evt.wait(0.1)
                self.in_evt.clear()
                continue
            try:
                block = self.in_q.popleft()
            except IndexError:
                continue
            try:
                self._process_block(block)
            except Exception:
                log.exception("audio processing failed")

    def _process_block(self, block):
        if self.vad is None:
            self.pre_roll.append(block)
            self.pre_voiced.append(False)
            cap = self.capture
            if cap is not None and not cap.done and cap.kind == "ptt":
                cap.add(block, True)        # no VAD yet: treat push-to-talk audio as speech
            return
        p = self.vad.prob(block)
        voiced = p >= (VAD_OFF if self.run_voiced > 0 else VAD_ON)
        self.run_voiced = self.run_voiced + 1 if voiced else 0
        self.recent_voiced.append(voiced)
        self.recent_blocks.append(block)
        now = time.monotonic()
        mode = self.cfg["listen_mode"]
        wake_score = None
        if self.wake is not None and mode == "wake_word" and self.capture is None:
            wake_score = self.wake.score(block)
        changed = False
        with self.lock:
            cap = self.capture
            if cap is not None and not cap.done:
                cap.add(block, voiced)
                self._capture_tick(cap, now)
            else:
                playing = self.speaking or (now - self.audio.player.last_audio_t) < ECHO_HOLDOFF_S
                if self.enrolling:
                    if self.run_voiced >= START_BLOCKS and not playing:
                        self._start_capture("enroll")
                        changed = True
                elif playing and mode != "push_to_talk":
                    if self.speaking and self._barge_candidate(now):
                        changed = True
                elif mode == "always":
                    if self.run_voiced >= START_BLOCKS:
                        self._start_capture("vad")
                        changed = True
                elif mode == "wake_word":
                    if now < self.wake_armed_until and self.run_voiced >= START_BLOCKS:
                        self._start_capture("wake")
                        changed = True
                    elif self.wake_engine == "openwakeword" and wake_score is not None and wake_score >= 0.5 \
                            and now - self.last_wake_t > 1.5:
                        self.last_wake_t = now
                        self.emit({"type": "wake", "word": self.cfg["wake_word"], "score": round(wake_score, 3)})
                        self.wake.reset()
                        self.capture = Capture("wake")
                        changed = True
                    elif self.wake_engine == "whisper" and self.run_voiced >= START_BLOCKS:
                        self._start_capture("kws")
                        changed = True
            self.pre_roll.append(block)
            self.pre_voiced.append(voiced)
        if changed:
            self.push_state()

    def _start_capture(self, kind):
        self.capture = Capture(kind, list(self.pre_roll), list(self.pre_voiced))

    def _capture_tick(self, cap, now):
        silence_s = self.cfg["vad_silence_ms"] / 1000.0
        if cap.kind == "ptt" or (cap.kind == "enroll" and self.ptt_down):
            if cap.seconds() >= MAX_CAPTURE_S:
                self._finish_capture()
        elif cap.kind == "wake" and not cap.heard_speech:
            if now - cap.started > WAKE_WAIT_S:
                cap.done = True
                self.capture = None
                self.push_state()
                return
        elif cap.heard_speech and now - cap.last_voice >= silence_s:
            self._finish_capture()
            return
        elif cap.seconds() >= (30.0 if cap.kind in ("kws", "enroll") else MAX_CAPTURE_S):
            self._finish_capture()
            return
        if cap.kind in ("vad", "ptt", "wake", "kws") and not cap.partial_pending \
                and now - cap.last_partial_req >= PARTIAL_EVERY_S and cap.voiced_seconds() >= 0.25:
            cap.partial_pending = True
            cap.last_partial_req = now
            self._stt_submit(1, ("partial", cap))

    def _finish_capture(self):
        cap = self.capture
        if cap is None or cap.done:
            return
        cap.done = True
        cap.ended = time.monotonic()
        self.capture = None
        self._stt_submit(0, ("final", cap))
        self.push_state()

    def _barge_candidate(self, now):
        """While speaking: is this the user talking over us (not our own voice echoing back)?"""
        if sum(self.recent_voiced) < 8 or now - self.barge_checked_t < 0.25:
            return False
        cur = self.current
        if cur is not None and not cur.get("interruptible", True):
            return False
        self.barge_checked_t = now
        audio = np.concatenate(list(self.recent_blocks)[-16:])
        ok = True
        if self.speaker is not None:
            try:
                e = self.speaker.embed(audio)
                user = float(np.dot(e, self.profile)) if self.profile is not None else None
                echo = float(np.dot(e, self.assistant_emb)) if self.assistant_emb is not None else None
                if user is not None:
                    ok = user >= 0.8 * self.cfg["speaker_threshold"] and (echo is None or user > echo)
                elif echo is not None:
                    ok = echo < 0.5
                log.debug("barge check user=%s echo=%s ok=%s", user, echo, ok)
            except Exception:
                log.exception("barge-in check failed")
        if not ok:
            return False
        self._barge_in(reason="voice")
        self._start_capture("vad")
        return True

    def _barge_in(self, reason):
        cur = self.current
        if not self.speaking and cur is None and not self.speak_q:
            return
        if cur is not None and not cur.get("interruptible", True) and reason != "stop":
            return
        log.info("barge-in (%s)", reason)
        self.stop_speaking()
        self.emit({"type": "barge_in"})

    # ----- STT worker --------------------------------------------------------------------------------
    def _stt_submit(self, prio, job):
        with self.stt_cv:
            heapq.heappush(self.stt_jobs, (prio, next(self.stt_seq), job))
            self.stt_cv.notify()

    def _stt_loop(self):
        while not self.stopping.is_set():
            with self.stt_cv:
                while not self.stt_jobs and not self.stopping.is_set():
                    self.stt_cv.wait(0.5)
                if self.stopping.is_set():
                    return
                _, _, job = heapq.heappop(self.stt_jobs)
            try:
                if not self.ready_evt.wait(timeout=600) or not self.models_ready:
                    continue
                kind, cap = job
                if kind == "partial":
                    self._do_partial(cap)
                else:
                    self._do_final(cap)
            except Exception as e:
                log.exception("transcription failed")
                self.error(f"transcription failed: {e}")

    def _do_partial(self, cap):
        try:
            if cap.done:
                return
            audio = cap.audio()[-30 * SR_IN:]
            text, _ = self.stt.transcribe(audio, self.cfg.get("language") or None)
            if cap.done:
                return
            if cap.kind == "kws":
                rest = match_wake(text, self.cfg["wake_word"])
                if rest is None:
                    return
                if not cap.wake_found:
                    cap.wake_found = True
                    self.emit({"type": "wake", "word": self.cfg["wake_word"], "score": 1.0})
                text = rest
            if text and text != cap.last_partial_text:
                cap.last_partial_text = text
                self.emit({"type": "partial", "text": text})
        finally:
            cap.partial_pending = False

    def _do_final(self, cap):
        if cap.kind == "enroll":
            self._enroll_add(cap)
            return
        voiced_s = cap.voiced_seconds()
        if voiced_s < 0.15:
            log.info("capture %d dropped: no speech", cap.id)
            return
        audio = cap.audio(trim=True)
        text, lang = self.stt.transcribe(audio, self.cfg.get("language") or None)
        if text.strip().lower() in HALLUCINATIONS and voiced_s < 1.0:
            log.info("capture %d dropped: %r looks like noise", cap.id, text)
            return
        if cap.kind == "kws":
            rest = match_wake(text, self.cfg["wake_word"])
            if rest is None:
                log.info("not addressed to us: %r", text)
                return
            if not cap.wake_found:
                self.emit({"type": "wake", "word": self.cfg["wake_word"], "score": 1.0})
            if len(re.sub(r"\W", "", rest)) < 2:
                with self.lock:
                    self.wake_armed_until = time.monotonic() + WAKE_WAIT_S
                return
            text = rest
        elif cap.kind == "wake":
            rest = match_wake(text, self.cfg["wake_word"])
            if rest is not None:
                text = rest
        if not text.strip():
            return
        score = None
        try:
            score = self.speaker_score(cap.voiced_audio())
        except Exception:
            log.exception("speaker scoring failed")
        if score is None:
            ok, verified, score = self.profile is None, False, 0.0
        else:
            ok, verified = score >= self.cfg["speaker_threshold"], True
        ev = {"type": "utterance", "text": text, "speaker_ok": bool(ok), "speaker_score": round(float(score), 3),
              "duration_s": round(cap.seconds(), 2), "lang": lang, "verified": verified}
        if getattr(cap, "ended", None):
            ev["latency_ms"] = int((time.monotonic() - cap.ended) * 1000)
        self.emit(ev)

    # ----- enrolment -----------------------------------------------------------------------------------
    def enroll_start(self):
        with self.lock:
            self.enrolling = True
            self.enroll_segments = []
            if self.capture is not None:
                self.capture.done = True
                self.capture = None
        self.emit({"type": "enroll_progress", "seconds": 0.0, "needed": ENROLL_NEEDED_S})
        self.push_state()

    def _enroll_seconds(self):
        return sum(s for _, s in self.enroll_segments)

    def _enroll_add(self, cap):
        if not self.enrolling:
            return
        audio = cap.voiced_audio()
        secs = cap.voiced_seconds()
        if secs >= 0.8:
            self.enroll_segments.append((audio, secs))
            try:
                d = os.path.join(self.models_dir, "speaker", "enrolment")
                os.makedirs(d, exist_ok=True)
                import soundfile as sf
                sf.write(os.path.join(d, f"segment-{len(self.enroll_segments):02d}.wav"), audio, SR_IN)
            except Exception:
                pass
        total = self._enroll_seconds()
        self.emit({"type": "enroll_progress", "seconds": round(total, 1), "needed": ENROLL_NEEDED_S})
        if total >= ENROLL_NEEDED_S:
            self.enroll_finish()

    def enroll_stop(self):
        with self.lock:
            cap = self.capture
            if cap is not None and cap.kind == "enroll" and not cap.done:
                cap.done = True
                self.capture = None
            else:
                cap = None
        if cap is not None and self.enrolling:
            self._enroll_add(cap)
        if self.enrolling:
            self.enroll_finish()

    def enroll_finish(self):
        if not self.enrolling:
            return
        self.enrolling = False
        total = self._enroll_seconds()
        segs, self.enroll_segments = self.enroll_segments, []
        if total < ENROLL_MIN_S or self.speaker is None:
            self.emit({"type": "enrolled", "ok": False,
                       "message": f"only {total:.1f} s of speech recorded; need at least {ENROLL_MIN_S:.0f} s"
                       if self.speaker is not None else "speaker verification model unavailable"})
            self.push_state()
            return
        embs, weights = [], []
        for audio, secs in segs:
            embs.append(self.speaker.embed(audio))
            weights.append(secs)
        e = np.average(np.stack(embs), axis=0, weights=np.asarray(weights))
        e = e / (np.linalg.norm(e) or 1.0)
        os.makedirs(os.path.dirname(self.profile_path), exist_ok=True)
        tmp = self.profile_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump({"version": 1, "model": ECAPA if not self.stub else "stub", "seconds": round(total, 1),
                       "segments": len(segs), "created": time.time(), "embedding": [float(v) for v in e]}, f)
        os.replace(tmp, self.profile_path)
        self.profile = e.astype(np.float32)
        self.emit({"type": "enrolled", "ok": True, "seconds": round(total, 1)})
        self.push_state()

    # ----- speaking ------------------------------------------------------------------------------------
    def speak(self, sid, text, interruptible=True):
        job = {"id": sid, "text": text or "", "interruptible": bool(interruptible), "cancel": threading.Event(),
               "t0": time.monotonic()}
        with self.speak_cv:
            self.speak_q.append(job)
            self.speak_cv.notify()

    def stop_speaking(self):
        with self.speak_cv:
            dropped = list(self.speak_q)
            self.speak_q.clear()
            cur = self.current
            if cur is not None:
                cur["cancel"].set()
            self.audio.flush()
        for j in dropped:
            self.emit({"type": "spoken", "id": j["id"], "interrupted": True})

    def _speak_loop(self):
        while not self.stopping.is_set():
            with self.speak_cv:
                while not self.speak_q and not self.stopping.is_set():
                    self.speak_cv.wait(0.5)
                if self.stopping.is_set():
                    return
                job = self.speak_q.popleft()
                self.current = job
            interrupted = False
            try:
                self.ready_evt.wait()
                if not self.models_ready:
                    raise RuntimeError("the voice models are not loaded")
                if not job["cancel"].is_set():
                    self.speaking = True
                    self.push_state()
                    epoch = self.audio.player.epoch
                    first = True
                    for sentence in split_sentences(job["text"]):
                        for chunk in self.tts.stream(sentence, self.cfg.get("language") or "en", job["cancel"]):
                            if job["cancel"].is_set():
                                break
                            self.audio.play(chunk, self.tts.sr, epoch)
                            if first:
                                first = False
                                log.info("speak %s: first audio after %.0f ms", job["id"],
                                         (time.monotonic() - job["t0"]) * 1000)
                        if job["cancel"].is_set():
                            break
                    while self.audio.pending_seconds() > 0 and not job["cancel"].is_set() and not self.stopping.is_set():
                        time.sleep(0.02)
                interrupted = job["cancel"].is_set()
            except Exception as e:
                log.exception("speaking failed")
                self.error(f"speaking failed: {e}")
                interrupted = True
            finally:
                with self.speak_cv:
                    self.current = None
                    more = bool(self.speak_q)
                if not more:
                    self.speaking = False
                self.emit({"type": "spoken", "id": job["id"], "interrupted": bool(interrupted)})
                self.push_state()

    # ----- voices ----------------------------------------------------------------------------------------
    def design_voice(self, description, sample_text, rid=None):
        try:
            sample_text = (sample_text or DEFAULT_VOICE_TEXT).strip()
            slug = re.sub(r"[^a-z0-9]+", "-", (description or "voice").lower()).strip("-")[:32] or "voice"
            out = os.path.join(self.models_dir, "voices", f"{slug}-{time.strftime('%Y%m%d-%H%M%S')}.wav")
            lang = TTS_LANGS.get((self.cfg.get("language") or "en")[:2], "English")
            fn = stub_design if self.stub else design_voice_file
            fn(self.models_dir, self.device, description, sample_text, lang, out)
            ev = {"type": "voice_designed", "ref_audio": out, "ref_text": sample_text}
            if rid is not None:
                ev["id"] = rid
            self.emit(ev)
        except Exception as e:
            log.exception("voice design failed")
            self.error(f"voice design failed: {e}", id=rid)

    # ----- lifecycle ---------------------------------------------------------------------------------------
    def shutdown(self):
        self.stopping.set()
        try:
            self.stop_speaking()
        except Exception:
            pass
        try:
            self.audio.close()
        except Exception:
            pass
        with self.speak_cv:
            self.speak_cv.notify_all()
        with self.stt_cv:
            self.stt_cv.notify_all()


# ---------------------------------------------------------------------------------------------
# server
# ---------------------------------------------------------------------------------------------

class Server:
    def __init__(self, svc, host, port, on_shutdown=None):
        self.svc, self.host, self.port = svc, host, port
        self.client = None
        self.loop = None
        self.out_q = None
        self.runner = None
        self.done = None
        self.on_shutdown = on_shutdown

    def emit(self, ev):
        loop = self.loop
        if loop is not None and not loop.is_closed():
            loop.call_soon_threadsafe(self.out_q.put_nowait, ev)

    async def _sender(self):
        while True:
            ev = await self.out_q.get()
            ws = self.client
            if ws is None or ws.closed:
                continue
            try:
                await ws.send_str(json.dumps(ev, separators=(",", ":")))
            except Exception as e:
                log.debug("send failed: %s", e)

    async def health(self, request):
        from aiohttp import web
        return web.json_response({"ok": True, "version": VERSION, "state": self.svc.state(),
                                  "models": dict(self.svc.models)})

    async def ws(self, request):
        from aiohttp import web, WSMsgType
        ws = web.WebSocketResponse(heartbeat=20.0, max_msg_size=1 << 20)
        await ws.prepare(request)
        old, self.client = self.client, ws
        if old is not None and not old.closed:
            log.info("a new client replaces the old one")
            await old.close(code=4000, message=b"replaced by a new client")
        self.svc.push_state(force=True)
        try:
            async for msg in ws:
                if msg.type == WSMsgType.TEXT:
                    await self.handle(msg.data)
                elif msg.type == WSMsgType.ERROR:
                    break
        finally:
            if self.client is ws:
                self.client = None
        return ws

    def reply(self, ev, rid):
        if rid is not None:
            ev["id"] = rid
        self.svc.emit(ev)

    async def handle(self, data):
        svc = self.svc
        try:
            msg = json.loads(data)
            if not isinstance(msg, dict):
                raise ValueError("a command must be a JSON object")
        except Exception as e:
            svc.error(f"bad command: {e}")
            return
        cmd, rid = msg.get("cmd"), msg.get("id")
        loop = asyncio.get_running_loop()
        try:
            if cmd == "config":
                errs = await loop.run_in_executor(None, svc.configure, msg)
                for e in errs:
                    svc.error(e, id=rid)
                self.reply({"type": "config_ok"}, rid)
            elif cmd == "ptt":
                svc.set_ptt(bool(msg.get("down")))
            elif cmd == "speak":
                svc.speak(msg.get("id") or f"s{int(time.time() * 1000)}", str(msg.get("text") or ""),
                          msg.get("interruptible", True))
            elif cmd == "stop_speaking":
                svc.stop_speaking()
            elif cmd == "enroll_start":
                svc.enroll_start()
            elif cmd == "enroll_stop":
                await loop.run_in_executor(None, svc.enroll_stop)
            elif cmd == "design_voice":
                threading.Thread(target=svc.design_voice, args=(msg.get("description") or "", msg.get("sample_text"), rid),
                                 daemon=True).start()
            elif cmd == "devices":
                ins, outs = await loop.run_in_executor(None, svc.audio.list_devices)
                self.reply({"type": "devices", "inputs": ins, "outputs": outs}, rid)
            elif cmd == "status":
                self.reply(svc.status(), rid)
            elif cmd == "inject_wav":
                svc.inject_wav(str(msg.get("path") or ""), msg.get("ptt"), rid)
            elif cmd == "shutdown":
                log.info("shutdown requested")
                self.done.set()
            else:
                svc.error(f"unknown command {cmd!r}", id=rid)
        except Exception as e:
            log.exception("command %s failed", cmd)
            svc.error(f"{cmd} failed: {e}", id=rid)

    async def run(self, ready_cb=None):
        from aiohttp import web
        self.loop = asyncio.get_running_loop()
        self.out_q = asyncio.Queue()
        self.done = asyncio.Event()
        self.svc.emit_cb = self.emit
        app = web.Application()
        app.router.add_get("/health", self.health)
        app.router.add_get("/ws", self.ws)
        self.runner = web.AppRunner(app, access_log=None)
        await self.runner.setup()
        site = web.TCPSite(self.runner, self.host, self.port)
        await site.start()
        socks = getattr(site._server, "sockets", None) or []
        if socks:
            self.port = socks[0].getsockname()[1]
        sender = asyncio.create_task(self._sender())
        if ready_cb:
            ready_cb(self.port)
        await self.done.wait()
        await asyncio.sleep(0.05)
        sender.cancel()
        if self.client is not None and not self.client.closed:
            await self.client.close(code=1001, message=b"shutting down")
        await self.runner.cleanup()

    def request_shutdown(self):
        loop = self.loop
        if loop is not None and not loop.is_closed():
            loop.call_soon_threadsafe(self.done.set)


def watch_parent(pid, on_dead):
    try:
        import psutil
        ct = psutil.Process(pid).create_time()
    except Exception:
        ct = None
    if ct is None:
        log.error("parent process %s is not running", pid)
        on_dead()
        return
    while True:
        time.sleep(1.0)
        if not pid_alive(pid, ct):
            log.info("parent process %s is gone; shutting down", pid)
            on_dead()
            return


def main(argv=None):
    ap = argparse.ArgumentParser(description="Craft Anything voice service")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8191)
    ap.add_argument("--models-dir", required=True)
    ap.add_argument("--device", default=None, help="cuda:0 (default when available) or cpu")
    ap.add_argument("--parent-pid", type=int, default=None)
    ap.add_argument("--log-level", default="INFO")
    ap.add_argument("--check-models", action="store_true", help="print which models are present as JSON and exit")
    ap.add_argument("--stub-models", action="store_true", help=argparse.SUPPRESS)   # tests: no GPU models
    ap.add_argument("--fake-audio", action="store_true", help=argparse.SUPPRESS)    # tests: no microphone
    ap.add_argument("--allow-inject", action="store_true",
                    help="tests only: accept {\"cmd\":\"inject_wav\",\"path\":...} to feed a WAV as if spoken into the microphone")
    args = ap.parse_args(argv)

    # stdout carries exactly one line (READY); everything else, including library prints, goes to stderr
    ready_out = os.fdopen(os.dup(1), "w", encoding="utf-8")
    try:
        os.dup2(2, 1)
    except OSError:
        pass
    sys.stdout = sys.stderr
    logging.basicConfig(level=getattr(logging, args.log_level.upper(), logging.INFO), stream=sys.stderr,
                        format="%(asctime)s %(levelname)s %(threadName)s: %(message)s")
    for noisy in ("aiohttp.access", "httpx", "urllib3", "speechbrain", "numba"):
        logging.getLogger(noisy).setLevel(logging.WARNING)

    if args.check_models:
        ready_out.write(json.dumps(check_models(args.models_dir)) + "\n")
        ready_out.flush()
        return 0

    device = args.device
    if device is None:
        if args.stub_models:
            device = "cpu"
        else:
            try:
                import torch
                device = "cuda:0" if torch.cuda.is_available() else "cpu"
            except Exception:
                device = "cpu"
    if device.startswith("cuda") and not args.stub_models:
        import torch
        if not torch.cuda.is_available():
            log.warning("CUDA is not available; falling back to the CPU (slow)")
            device = "cpu"

    audio = FakeAudioIO() if args.fake_audio else None
    if audio is None:
        try:
            audio = SoundDeviceIO()
        except Exception as e:
            log.error("audio I/O unavailable (%s); using a silent virtual device", e)
            audio = FakeAudioIO(inputs=(), outputs=())
    svc = VoiceService(args.models_dir, device, audio, stub_models=args.stub_models, allow_inject=args.allow_inject)
    if args.allow_inject:
        log.warning("--allow-inject: test audio may be fed in place of the microphone")
    server = Server(svc, args.host, args.port)

    def on_ready(port):
        ready_out.write("READY " + json.dumps({"port": port, "version": VERSION}, separators=(",", ":")) + "\n")
        ready_out.flush()
        log.info("listening on ws://%s:%d/ws", args.host, port)
        svc.open_mic()
        threading.Thread(target=svc.load_models, name="loader", daemon=True).start()
        if args.parent_pid:
            threading.Thread(target=watch_parent, args=(args.parent_pid, server.request_shutdown),
                             name="parent-watchdog", daemon=True).start()

    try:
        asyncio.run(server.run(on_ready))
    except KeyboardInterrupt:
        pass
    except OSError as e:
        log.error("cannot listen on %s:%d: %s", args.host, args.port, e)
        svc.shutdown()
        os._exit(2)
    svc.shutdown()
    logging.shutdown()
    os._exit(0)


if __name__ == "__main__":
    main()
