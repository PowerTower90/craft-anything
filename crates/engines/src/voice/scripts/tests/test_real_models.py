"""End-to-end tests with the real models on the GPU (Whisper, Silero, ECAPA, Qwen3-TTS, openWakeWord).

No microphone: the "user" and an "other person" are two voices designed with Qwen3-TTS VoiceDesign;
their sentences are synthesised once (cached) and fed to the service's virtual microphone in real time.
Latencies, scores and VRAM are written to target/voice-test-report.json.

Skipped without CUDA or with VOICE_REAL=0. Models dir: VOICE_TEST_MODELS_DIR (default target/voice-test-models).
"""
import difflib
import json
import os
import time

import numpy as np
import pytest

from conftest import REPO, Client, InProcess, silence, vs

torch = pytest.importorskip("torch")
pytestmark = pytest.mark.skipif(os.environ.get("VOICE_REAL", "1") == "0" or not torch.cuda.is_available(),
                                reason="real-model tests need CUDA (set VOICE_REAL=1)")

MODELS = os.environ.get("VOICE_TEST_MODELS_DIR") or os.path.join(REPO, "target", "voice-test-models")
REPORT = os.path.join(REPO, "target", "voice-test-report.json")
VOICES = {
    "user": ("A man in his thirties with a clear, mid-pitched voice and a relaxed, casual way of speaking.",
             "Okay, so here's the thing. I want to get this done today, if we can."),
    "other": ("An elderly woman with a soft, breathy, high-pitched voice, speaking slowly and gently.",
              "Oh my dear, would you mind passing me that cup of tea over there?"),
}
LINES = {
    "u_open": ("user", "Open the trailer folder on my desktop."),
    "u_build": ("user", "Please build the new trailer and tell me when it's done."),
    "u_status": ("user", "How is the render going so far?"),
    "u_long": ("user", "Find the newest video in my downloads folder and make a short clip from it."),
    "u_wake": ("user", "Hey Craft, what time is it?"),
    "u_wake_only": ("user", "Hey Craft."),
    "u_jarvis": ("user", "Hey Jarvis."),
    "u_cmd": ("user", "Open the downloads folder."),
    "o_open": ("other", "Open the trailer folder on my desktop."),
    "o_talk": ("other", "Could you please stop talking for a moment and listen to me?"),
}
ENROL = [
    "The quick brown fox jumps over the lazy dog near the river bank.",
    "I usually start my morning with a cup of coffee and a short walk outside.",
    "Please remember to save the project before you close the editor.",
    "We should test the new build on every computer in the office this week.",
    "My favourite games have big open worlds and a lot of secrets to find.",
    "Tomorrow I will record a new trailer and post it on the website.",
]

metrics = {}


def sim(a, b):
    na, nb = " ".join(vs.norm_words(a)), " ".join(vs.norm_words(b))
    return difflib.SequenceMatcher(None, na, nb).ratio()


@pytest.fixture(scope="module")
def real():
    prof = os.path.join(MODELS, "speaker", "profile.json")
    if os.path.exists(prof):
        os.remove(prof)                      # every run starts unenrolled
    s = InProcess(MODELS, stub=False, device="cuda:0")
    t0 = time.perf_counter()
    s.wait_ready(1800)
    metrics["load_s"] = round(time.perf_counter() - t0, 1)
    yield s
    metrics["status"] = s.svc.status()
    try:
        metrics["peak_vram_mb_torch"] = round(torch.cuda.max_memory_allocated() / 2 ** 20)
    except Exception:
        pass
    os.makedirs(os.path.dirname(REPORT), exist_ok=True)
    with open(REPORT, "w", encoding="utf-8") as f:
        json.dump(metrics, f, indent=2, default=str)
    s.close()


@pytest.fixture(scope="module")
def clips(real):
    """16 kHz arrays for every test line, synthesised once with the two designed voices (cached on disk)."""
    d = os.path.join(MODELS, "test-fixtures")
    os.makedirs(d, exist_ok=True)
    refs = {}
    for name, (desc, text) in VOICES.items():
        p = os.path.join(d, f"voice_{name}.wav")
        if not os.path.exists(p):
            vs.design_voice_file(MODELS, "cuda:0", desc, text, "English", p)
        refs[name] = real.svc.tts.voice_of(p, text)
    out = {}
    items = dict(LINES)
    items.update({f"enrol_{i}": ("user", t) for i, t in enumerate(ENROL)})
    for key, (who, text) in items.items():
        p = os.path.join(d, f"{key}.wav")
        if not os.path.exists(p):
            import soundfile as sf
            a = real.svc.tts.synthesize(text, "en", voice=refs[who])
            sf.write(p, vs.resample(a, real.svc.tts.sr, 16000), 16000)
        out[key] = (trim(vs.load_wav(p, 16000)[0]), text)
    return out


def trim(a, keep=0.05):
    """Cut leading/trailing near-silence so 'speech start/end' in the timings is the real voice onset/offset."""
    on = np.flatnonzero(np.abs(a) > 0.02)
    if on.size == 0:
        return a
    k = int(keep * 16000)
    return a[max(0, on[0] - k): min(a.size, on[-1] + k)]


def config(c, **kw):
    t = c.send(cmd="config", id="cfg", **kw)
    c.wait("config_ok", after=t)
    time.sleep(0.2)


def feed_and_time(real, audio, lead=0.5, tail=2.0):
    """Feed lead silence + audio + tail silence; return (speech start, speech end) perf_counter times."""
    while real.audio.feed_remaining_s() > 0:
        time.sleep(0.05)
    t = time.perf_counter()
    real.audio.feed(np.concatenate([silence(lead), audio, silence(tail)]))
    return t + lead, t + lead + audio.size / 16000


def test_roundtrip_always_mode(real, clips):
    c = Client(real.port)
    config(c, listen_mode="always", vad_silence_ms=700, language="en")
    rows = []
    for key in ("u_open", "u_build", "u_long"):
        audio, text = clips[key]
        start, end = feed_and_time(real, audio)
        u = c.wait("utterance", after=start, timeout=20)
        parts = c.of("partial", after=start)
        first_partial = round(parts[0]["_t"] - start, 3) if parts else None
        rows.append({"line": key, "said": text, "heard": u["text"], "similarity": round(sim(text, u["text"]), 3),
                     "first_partial_s": first_partial, "partials": len(parts),
                     "final_after_speech_end_s": round(u["_t"] - end, 3), "service_latency_ms": u.get("latency_ms")})
        assert sim(text, u["text"]) >= 0.8, rows[-1]
        assert first_partial is not None and first_partial < 1.0, rows[-1]
        assert u["_t"] - end < 1.5, rows[-1]
        time.sleep(0.5)
    metrics["always_mode"] = rows
    c.close()


def test_push_to_talk_latency(real, clips):
    c = Client(real.port)
    config(c, listen_mode="push_to_talk")
    rows = []
    for key in ("u_status", "u_cmd"):
        audio, text = clips[key]
        c.send(cmd="ptt", down=True)
        start, end = feed_and_time(real, audio, lead=0.2, tail=0.3)
        while real.audio.feed_remaining_s() > 0:
            time.sleep(0.02)
        t_up = c.send(cmd="ptt", down=False)
        u = c.wait("utterance", after=start, timeout=20)
        rows.append({"line": key, "heard": u["text"], "similarity": round(sim(text, u["text"]), 3),
                     "final_after_ptt_up_s": round(u["_t"] - t_up, 3)})
        assert sim(text, u["text"]) >= 0.8, rows[-1]
        assert u["_t"] - t_up < 1.5, rows[-1]
    metrics["push_to_talk"] = rows
    c.close()


def test_speak_latency_and_intelligibility(real):
    c = Client(real.port)
    config(c, listen_mode="push_to_talk")
    rows = []
    for i, text in enumerate(["Okay, I'm opening the trailer folder now.",
                              "The build finished without errors. The new trailer is in the output folder, ready for you to review."]):
        while real.svc.speaking:
            time.sleep(0.05)
        time.sleep(0.3)
        real.audio.reset_played()
        t = c.send(cmd="speak", id=f"lat{i}", text=text)
        s = c.wait("spoken", lambda e, i=i: e["id"] == f"lat{i}", timeout=60)
        first = real.audio.first_audio_t - t
        played = vs.resample(real.audio.played_audio(), real.audio.player.sr, 16000)
        heard, _ = real.svc.stt.transcribe(played, "en")
        rows.append({"text": text, "first_audio_s": round(first, 3), "audio_s": round(played.size / 16000, 2),
                     "spoken_after_s": round(s["_t"] - t, 2), "transcribed": heard, "similarity": round(sim(text, heard), 3)})
        assert not s["interrupted"]
        assert first < 1.5, rows[-1]
        assert sim(text, heard) >= 0.8, rows[-1]
    metrics["speak"] = rows
    c.close()


def test_barge_in_without_profile(real, clips):
    if real.svc.profile is not None:
        pytest.skip("a speaker profile exists already")
    c = Client(real.port)
    config(c, listen_mode="always", vad_silence_ms=700)
    t = c.send(cmd="speak", id="long", text="Let me tell you about the render queue. " * 4 + "It has many steps and takes a while.")
    c.wait("state", lambda e: e["speaking"], after=t)
    time.sleep(1.5)
    audio, text = clips["u_status"]
    start, end = feed_and_time(real, audio, lead=0.0)
    b = c.wait("barge_in", after=t, timeout=10)
    s = c.wait("spoken", lambda e: e["id"] == "long", after=t, timeout=10)
    u = c.wait("utterance", after=start, timeout=20)
    metrics["barge_in_no_profile"] = {"barge_after_speech_start_s": round(b["_t"] - start, 3), "heard": u["text"],
                                      "similarity": round(sim(text, u["text"]), 3)}
    assert s["interrupted"] is True
    assert b["_t"] - start < 1.0
    assert sim(text, u["text"]) >= 0.7, metrics["barge_in_no_profile"]
    c.close()


def test_enrol_and_verify(real, clips):
    c = Client(real.port)
    config(c, listen_mode="always", vad_silence_ms=600, speaker_threshold=0.55)
    t = c.send(cmd="enroll_start")
    c.wait("enroll_progress", after=t)
    for i in range(len(ENROL)):
        feed_and_time(real, clips[f"enrol_{i}"][0], lead=0.2, tail=1.0)
    e = c.wait("enrolled", after=t, timeout=60)
    assert e["ok"] is True, e
    prog = c.of("enroll_progress", after=t)
    time.sleep(0.5)
    rows = []
    for key in ("u_open", "u_status", "o_open", "o_talk"):
        audio, text = clips[key]
        start, end = feed_and_time(real, audio)
        u = c.wait("utterance", after=start, timeout=20)
        rows.append({"line": key, "speaker_ok": u["speaker_ok"], "score": u["speaker_score"], "heard": u["text"]})
        time.sleep(0.3)
    metrics["speaker"] = {"enrolled_seconds": prog[-1]["seconds"] if prog else None, "rows": rows}
    by = {r["line"]: r for r in rows}
    assert by["u_open"]["speaker_ok"] and by["u_status"]["speaker_ok"], rows
    assert not by["o_open"]["speaker_ok"] and not by["o_talk"]["speaker_ok"], rows
    c.close()


def test_other_voice_cannot_barge_in_once_enrolled(real, clips):
    if real.svc.profile is None:
        pytest.skip("needs the enrolment test first")
    c = Client(real.port)
    config(c, listen_mode="always", vad_silence_ms=700)
    t = c.send(cmd="speak", id="guarded", text="I am reading out a long status report right now. " * 3)
    c.wait("state", lambda e: e["speaking"], after=t)
    time.sleep(1.0)
    feed_and_time(real, clips["o_talk"][0], lead=0.0, tail=0.5)
    s = c.wait("spoken", lambda e: e["id"] == "guarded", after=t, timeout=60)
    other_barged = bool(c.of("barge_in", after=t))
    # now the enrolled user interrupts
    t2 = c.send(cmd="speak", id="guarded2", text="Here is another long status report for you to hear. " * 3)
    c.wait("state", lambda e: e["speaking"], after=t2)
    time.sleep(1.0)
    start, _ = feed_and_time(real, clips["u_build"][0], lead=0.0)
    b = c.wait("barge_in", after=t2, timeout=10)
    s2 = c.wait("spoken", lambda e: e["id"] == "guarded2", after=t2, timeout=10)
    metrics["barge_in_enrolled"] = {"other_voice_barged": other_barged, "other_spoken_interrupted": s["interrupted"],
                                    "user_barge_after_speech_start_s": round(b["_t"] - start, 3)}
    assert not other_barged and not s["interrupted"]
    assert s2["interrupted"] is True
    c.close()


def test_wake_word_whisper_spotting(real, clips):
    c = Client(real.port)
    config(c, listen_mode="wake_word", wake_word="hey craft", vad_silence_ms=700)
    assert real.svc.wake_engine == "whisper"
    audio, _ = clips["u_wake"]
    start, end = feed_and_time(real, audio)
    w = c.wait("wake", after=start, timeout=15)
    u = c.wait("utterance", after=start, timeout=20)
    one = {"wake_after_start_s": round(w["_t"] - start, 3), "heard": u["text"], "final_after_end_s": round(u["_t"] - end, 3)}
    assert sim("what time is it", u["text"]) >= 0.8, one
    # wake phrase alone, then the command
    t = time.perf_counter()
    feed_and_time(real, clips["u_wake_only"][0], tail=0.9)
    w2 = c.wait("wake", after=t, timeout=15)
    start, end = feed_and_time(real, clips["u_cmd"][0], lead=0.1)
    u2 = c.wait("utterance", after=start, timeout=20)
    two = {"heard": u2["text"], "final_after_end_s": round(u2["_t"] - end, 3)}
    metrics["wake_whisper"] = {"one_breath": one, "two_part": two}
    assert w2 and sim("open the downloads folder", u2["text"]) >= 0.8, two
    # not addressed to us: ignored
    t = time.perf_counter()
    feed_and_time(real, clips["u_open"][0])
    time.sleep(2.5)
    assert not c.of("utterance", after=t)
    c.close()


def test_wake_word_openwakeword(real, clips):
    c = Client(real.port)
    config(c, listen_mode="wake_word", wake_word="hey jarvis", vad_silence_ms=700)
    deadline = time.time() + 120
    while real.svc.wake_engine is None and time.time() < deadline:
        time.sleep(0.2)
    metrics["wake_oww_engine"] = real.svc.wake_engine
    assert real.svc.wake_engine == "openwakeword"
    start, end = feed_and_time(real, clips["u_jarvis"][0], tail=0.3)
    try:
        w = c.wait("wake", after=start, timeout=6)
        metrics["wake_oww"] = {"score": w["score"], "after_start_s": round(w["_t"] - start, 3)}
    except TimeoutError:
        metrics["wake_oww"] = {"detected": False}
        pytest.xfail("openWakeWord did not fire on the synthetic voice (it is trained on real speech)")
    start, end = feed_and_time(real, clips["u_cmd"][0], lead=0.2)
    u = c.wait("utterance", after=start, timeout=20)
    assert sim("open the downloads folder", u["text"]) >= 0.8
    c.close()


def test_vram_report(real):
    st = real.svc.status()
    metrics["vram_mb"] = st.get("vram_mb")
    assert st["models"]["stt"] == "ready" and st["models"]["tts"] == "ready"
