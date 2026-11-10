"""Protocol and pipeline tests with stand-in models (CPU only, no microphone, no GPU). Fast."""
import json
import os
import subprocess
import sys
import time
import urllib.request

import numpy as np
import pytest

from conftest import SERVICE_PY, Client, InProcess, silence, tone, vs, write_wav


# ----- pure helpers ------------------------------------------------------------------------------

def test_split_sentences():
    s = vs.split_sentences("Hello there. How are you?\nFine! " + "word, " * 80)
    assert s[:3] == ["Hello there.", "How are you?", "Fine!"]
    assert all(len(x) <= 221 for x in s)
    assert vs.split_sentences("  ...  ") == []


def test_match_wake():
    assert vs.match_wake("Hey, Kraft, open the trailer folder.", "hey craft") == "open the trailer folder."
    assert vs.match_wake("hey craft", "hey craft") == ""
    assert vs.match_wake("Okay, hey craft. What time is it?", "hey craft") == "What time is it?"
    assert vs.match_wake("open the folder", "hey craft") is None
    assert vs.match_wake("Hey Jarvis, stop.", "hey jarvis") == "stop."


def test_player_clear_drops_stale_producer():
    p = vs.Player(24000)
    e = p.epoch
    p.push(np.ones(100, np.float32), e)
    assert p.pending() == 100
    p.clear()
    p.push(np.ones(100, np.float32), e)          # stale epoch: ignored
    assert p.pending() == 0
    assert not np.any(p.read(50))


# ----- subprocess: the real entry point --------------------------------------------------------

def start_proc(models, *extra):
    cmd = [sys.executable, "-I", SERVICE_PY, "--host", "127.0.0.1", "--port", "0", "--models-dir", models,
           "--stub-models", "--fake-audio", *extra]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    line = p.stdout.readline()
    assert line.startswith("READY "), (line, p.stderr.read() if p.poll() is not None else "")
    info = json.loads(line[len("READY "):])
    return p, info


def test_ready_line_health_and_shutdown(tmp_models):
    p, info = start_proc(tmp_models)
    try:
        assert info["version"] == 1 and info["port"] > 0
        with urllib.request.urlopen(f"http://127.0.0.1:{info['port']}/health", timeout=5) as r:
            h = json.load(r)
        assert h["ok"] is True and h["version"] == 1
        for k in ("listening", "capturing", "speaking", "mode", "enrolled"):
            assert k in h["state"]
        c = Client(info["port"])
        c.wait("state")
        c.send(cmd="devices", id="d1")
        d = c.wait("devices")
        assert d["id"] == "d1" and d["inputs"] == ["Fake Microphone"] and d["outputs"] == ["Fake Speakers"]
        c.send(cmd="status", id="st")
        st = c.wait("status", lambda e: e.get("id") == "st")
        assert st["version"] == 1 and "models" in st
        c.send(cmd="bogus", id="x")
        assert c.wait("error", lambda e: e.get("id") == "x")["fatal"] is False
        c.send(cmd="shutdown")
        assert p.wait(15) == 0
        rest = p.stdout.read()
        assert rest.strip() == "", f"stdout must carry only the READY line, got {rest!r}"
    finally:
        if p.poll() is None:
            p.kill()


def test_parent_pid_watchdog(tmp_models):
    parent = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
    p, info = start_proc(tmp_models, "--parent-pid", str(parent.pid))
    try:
        time.sleep(1.0)
        assert p.poll() is None
        parent.kill()
        parent.wait(5)
        assert p.wait(15) == 0
    finally:
        for q in (p, parent):
            if q.poll() is None:
                q.kill()


def test_check_models(tmp_models):
    out = subprocess.run([sys.executable, "-I", SERVICE_PY, "--models-dir", tmp_models, "--check-models"],
                         capture_output=True, text=True, timeout=120)
    res = json.loads(out.stdout.strip().splitlines()[-1])
    for k in ("stt", "tts", "voice_design", "speaker", "vad", "wake_word", "default_voice", "speaker_profile"):
        assert k in res
    assert res["speaker_profile"] is False


def test_bad_port_fails_cleanly(tmp_models):
    p, info = start_proc(tmp_models)
    try:
        q = subprocess.run([sys.executable, "-I", SERVICE_PY, "--port", str(info["port"]), "--models-dir", tmp_models,
                            "--stub-models", "--fake-audio"], capture_output=True, text=True, timeout=60)
        assert q.returncode == 2 and "READY" not in q.stdout
    finally:
        p.kill()


# ----- in-process: pipeline behaviour with stub models ------------------------------------------

@pytest.fixture
def svc(tmp_models):
    s = InProcess(tmp_models, stub=True)
    s.wait_ready(30)
    yield s
    s.close()


def test_second_client_replaces_first(svc):
    a = Client(svc.port)
    a.wait("state")
    b = Client(svc.port)
    b.wait("state")
    assert a.closed.wait(5)
    b.send(cmd="status", id="s")
    assert b.wait("status")["id"] == "s"
    b.close()


def test_config_and_bad_values(svc):
    c = Client(svc.port)
    c.send(cmd="config", id="c1", listen_mode="always", speaker_threshold=0.6, vad_silence_ms=500,
           input_device="Fake Microphone", output_device=None, language="en", tts_voice=None, wake_word="hey craft")
    assert c.wait("config_ok")["id"] == "c1"
    c.wait("state", lambda e: e["mode"] == "always")
    t = time.perf_counter()
    c.send(cmd="config", id="c2", listen_mode="shouting", input_device="No Such Mic",
           tts_voice={"ref_audio": "missing.wav", "ref_text": "x"})
    c.wait("config_ok", lambda e: e["id"] == "c2")
    msgs = " | ".join(e["message"] for e in c.of("error", t))
    assert "listen_mode" in msgs and "microphone" in msgs and "tts_voice" in msgs
    c.close()


def test_push_to_talk_utterance(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="push_to_talk")
    c.wait("config_ok")
    c.send(cmd="config", input_device="Fake Microphone")
    c.wait("state", lambda e: e["listening"])
    c.send(cmd="ptt", down=True)
    c.wait("state", lambda e: e["capturing"])
    svc.audio.feed(tone(200, 1.5))
    time.sleep(1.7)
    t_up = c.send(cmd="ptt", down=False)
    u = c.wait("utterance", timeout=10)
    assert u["text"].startswith("stub speech") and u["speaker_ok"] is True and u["verified"] is False
    assert u["duration_s"] >= 1.5 and u["lang"] == "en"
    assert u["_t"] - t_up < 1.5
    assert c.of("partial"), "partials while capturing"
    c.close()


def test_always_mode_segments_on_silence(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always", vad_silence_ms=600)
    c.wait("config_ok")
    svc.audio.feed(np.concatenate([silence(0.3), tone(220, 1.2), silence(1.5), tone(260, 0.8), silence(1.2)]))
    u1 = c.wait("utterance", timeout=10)
    u2 = c.wait("utterance", lambda e: e is not u1 and e["_t"] > u1["_t"], timeout=10)
    secs = float(u1["text"].split()[2])
    assert 0.9 <= secs <= 1.4, u1
    assert u2["duration_s"] < u1["duration_s"]
    c.close()


def test_speak_queue_stop_and_spoken(svc):
    c = Client(svc.port)
    t0 = c.send(cmd="speak", id="s1", text="One sentence here. And another one.")
    c.send(cmd="speak", id="s2", text="Queued after the first.")
    s1 = c.wait("spoken", lambda e: e["id"] == "s1", timeout=20)
    s2 = c.wait("spoken", lambda e: e["id"] == "s2", timeout=20)
    assert not s1["interrupted"] and not s2["interrupted"] and s2["_t"] >= s1["_t"]
    assert svc.audio.first_audio_t is not None and svc.audio.first_audio_t - t0 < 1.5
    c.wait("state", lambda e: not e["speaking"])
    c.send(cmd="speak", id="s3", text="A long text " * 40)
    c.send(cmd="speak", id="s4", text="never heard")
    c.wait("state", lambda e: e["speaking"])
    time.sleep(0.3)
    c.send(cmd="stop_speaking")
    assert c.wait("spoken", lambda e: e["id"] == "s3")["interrupted"] is True
    assert c.wait("spoken", lambda e: e["id"] == "s4")["interrupted"] is True
    c.close()


def test_barge_in_by_voice_and_by_ptt(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always")
    c.wait("config_ok")
    c.send(cmd="speak", id="long", text="Talking for quite a while now. " * 30)
    c.wait("state", lambda e: e["speaking"])
    time.sleep(0.5)
    svc.audio.feed(np.concatenate([tone(180, 1.5), silence(1.2)]))
    c.wait("barge_in", timeout=5)
    assert c.wait("spoken", lambda e: e["id"] == "long")["interrupted"] is True
    c.wait("utterance", timeout=10)
    # push-to-talk barge-in
    c.send(cmd="config", listen_mode="push_to_talk")
    c.wait("config_ok")
    t = time.perf_counter()
    c.send(cmd="speak", id="long2", text="Talking again for a while. " * 30)
    c.wait("state", lambda e: e["speaking"], after=t)
    c.send(cmd="ptt", down=True)
    c.wait("barge_in", after=t)
    assert c.wait("spoken", lambda e: e["id"] == "long2")["interrupted"] is True
    c.send(cmd="ptt", down=False)
    c.close()


def test_non_interruptible_ignores_voice(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always")
    c.wait("config_ok")
    t = time.perf_counter()
    c.send(cmd="speak", id="must", text="Please confirm. " * 6, interruptible=False)
    c.wait("state", lambda e: e["speaking"], after=t)
    svc.audio.feed(tone(180, 1.0))
    s = c.wait("spoken", lambda e: e["id"] == "must", timeout=30)
    assert s["interrupted"] is False and not c.of("barge_in", t)
    c.close()


def test_enrol_and_verify(svc, tmp_models):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always", vad_silence_ms=500)
    c.wait("config_ok")
    c.send(cmd="enroll_start")
    c.wait("enroll_progress")
    c.wait("state", lambda e: e["enrolling"])
    me = np.concatenate([np.concatenate([tone(150 + 5 * i, 2.5, vibrato=0.02), silence(0.8)]) for i in range(4)])
    svc.audio.feed(me)
    while svc.audio.feed_remaining_s() > 0:
        time.sleep(0.1)
    p = c.wait("enroll_progress", lambda e: e["seconds"] >= 9, timeout=20)
    assert p["needed"] == 20
    c.send(cmd="enroll_stop")
    e = c.wait("enrolled", timeout=10)
    assert e["ok"] is True
    c.wait("state", lambda e: e["enrolled"] and not e["enrolling"])
    assert os.path.exists(os.path.join(tmp_models, "speaker", "profile.json"))
    t = time.perf_counter()
    svc.audio.feed(np.concatenate([tone(152, 1.5, vibrato=0.02), silence(1.0)]))
    mine = c.wait("utterance", after=t, timeout=10)
    t = time.perf_counter()
    svc.audio.feed(np.concatenate([tone(700, 1.5), silence(1.0)]))
    other = c.wait("utterance", after=t, timeout=10)
    assert mine["verified"] and mine["speaker_ok"], mine
    assert other["verified"] and not other["speaker_ok"], other
    assert mine["speaker_score"] > other["speaker_score"]
    c.close()


def test_enrol_too_short(svc):
    c = Client(svc.port)
    c.send(cmd="enroll_start")
    c.wait("enroll_progress")
    c.send(cmd="enroll_stop")
    assert c.wait("enrolled")["ok"] is False
    c.close()


def test_design_voice(svc, tmp_models):
    c = Client(svc.port)
    c.send(cmd="design_voice", id="v1", description="A deep calm voice", sample_text="Hello there, friend.")
    v = c.wait("voice_designed", timeout=20)
    assert v["id"] == "v1" and os.path.isfile(v["ref_audio"]) and v["ref_text"] == "Hello there, friend."
    c.send(cmd="config", id="cv", tts_voice={"ref_audio": v["ref_audio"], "ref_text": v["ref_text"]})
    c.wait("config_ok", lambda e: e["id"] == "cv")
    assert not [e for e in c.of("error") if "tts_voice" in e["message"]]
    c.close()


# ----- test-only audio injection (--allow-inject) -------------------------------------------------

def test_inject_is_refused_without_the_flag(svc, tmp_path):
    c = Client(svc.port)
    c.wait("state")
    wav = write_wav(tmp_path / "hi.wav", tone(200, 1.0))
    c.send(cmd="inject_wav", id="i1", path=wav)
    e = c.wait("error", lambda e: e.get("id") == "i1", timeout=5)
    assert "--allow-inject" in e["message"]
    time.sleep(1.5)
    assert not c.of("utterance") and not c.of("injected")
    c.close()


def test_inject_feeds_a_wav_as_a_push_to_talk_utterance(tmp_models, tmp_path):
    s = InProcess(tmp_models, stub=True, allow_inject=True)
    try:
        s.wait_ready(30)
        c = Client(s.port)
        c.send(cmd="config", listen_mode="push_to_talk", input_device="Fake Microphone")
        c.wait("config_ok")
        wav = write_wav(tmp_path / "hi.wav", tone(200, 1.5))
        t = c.send(cmd="inject_wav", id="i2", path=wav)
        u = c.wait("utterance", after=t, timeout=15)
        assert u["text"].startswith("stub speech") and u["duration_s"] >= 1.4, u
        done = c.wait("injected", lambda e: e.get("id") == "i2", timeout=10)
        assert done["seconds"] == 1.5
        assert c.wait("state", lambda e: not e["capturing"], after=t, timeout=5)
        c.close()
    finally:
        s.close()


def test_command_line_accepts_allow_inject(tmp_models):
    p, info = start_proc(tmp_models, "--allow-inject")
    try:
        assert info["port"] > 0
    finally:
        p.kill()


# ----- levels and chimes ---------------------------------------------------------------------------

def test_level_events_follow_the_microphone_and_the_speakers(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="push_to_talk", input_device="Fake Microphone", sounds=False)
    c.wait("config_ok")
    time.sleep(1.2)
    quiet = [e["in"] for e in c.of("level")]
    assert quiet, "levels while listening, even to silence"
    assert max(quiet) < 0.15, quiet
    assert len(quiet) <= 5, f"only ~2 Hz while just listening: {len(quiet)} in 1.2 s"
    t = time.perf_counter()
    c.send(cmd="ptt", down=True)
    svc.audio.feed(tone(200, 1.5))
    time.sleep(1.2)
    loud = [e["in"] for e in c.of("level", t)]
    assert max(loud) > 0.5, loud
    assert len(loud) >= 10, f"~20 Hz while capturing: {len(loud)} in 1.2 s"
    c.send(cmd="ptt", down=False)
    c.wait("utterance", timeout=10)
    t = time.perf_counter()
    c.send(cmd="speak", id="s1", text="Hello there, this is a test of the speakers.")
    c.wait("spoken", lambda e: e["id"] == "s1", timeout=20)
    outs = [e["out"] for e in c.of("level", t)]
    assert max(outs) > 0.3, outs
    c.close()


def test_state_reports_model_loading_steps(svc):
    c = Client(svc.port)
    st = c.wait("state")
    assert st["load_steps"] == 5 and st["load_step"] == 5 and st["models_ready"] is True, st
    c.close()


def test_wake_word_checks_are_silent_until_the_wake_word(svc):
    """Sounds that aren't the wake word mustn't show as capturing or chime (the chime used to set
    off the next wake-word check: the orb flickered on and off by itself)."""
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="wake_word", wake_word="hey craft", input_device="Fake Microphone", sounds=True)
    c.wait("config_ok")
    time.sleep(0.3)
    svc.audio.reset_played()
    seen = []
    svc.audio.feed(np.concatenate([silence(0.3), tone(220, 1.0), silence(1.0), tone(260, 0.8), silence(1.0)]))
    t_end = time.time() + 4.0
    while time.time() < t_end:
        try:
            e = c.wait("state", timeout=0.5)
            seen.append(e["capturing"])
        except Exception:
            pass
    assert not any(seen), f"a wake-word check showed as capturing: {seen}"
    assert svc.audio.played_audio().size == 0, "no chime for sounds that aren't the wake word"
    c.close()


def test_chimes_play_quietly_and_can_be_turned_off(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="push_to_talk", input_device="Fake Microphone", sounds=True)
    c.wait("config_ok")
    svc.audio.reset_played()
    c.send(cmd="chime", kind="done")
    time.sleep(0.6)
    played = svc.audio.played_audio()
    assert played.size > 0 and np.max(np.abs(played)) <= 0.12, "a quiet tone was played"
    # Capture start and end chime by themselves.
    svc.audio.reset_played()
    c.send(cmd="ptt", down=True)
    c.wait("state", lambda e: e["capturing"])
    time.sleep(0.4)
    assert svc.audio.played_audio().size > 0, "capture start chimed"
    c.send(cmd="ptt", down=False)
    # Off: nothing.
    c.send(cmd="config", sounds=False)
    c.wait("config_ok")
    time.sleep(0.5)
    svc.audio.reset_played()
    c.send(cmd="chime", kind="ask")
    time.sleep(0.5)
    assert svc.audio.played_audio().size == 0
    c.send(cmd="chime", id="x", kind="nope")
    c.close()
