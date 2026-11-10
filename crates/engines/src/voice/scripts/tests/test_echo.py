"""The assistant must not hear itself: own words, own voice, echo cancellation and turn-taking.
Fast tests with stand-in models (CPU, no microphone)."""
import time

import numpy as np
import pytest

from conftest import Client, InProcess, silence, tone, vs

SAID = ["Yes, I can hear you!", "How can I help you today?"]


# ----- own words ----------------------------------------------------------------------------------

@pytest.mark.parametrize("heard", [
    "Yes, I can hear you. How can I help you today?",       # the whole thing back
    "how can I help you today",                              # part of it
    "yes i can here you how can i help",                     # misheard words
    "I can hear you how can",                                # across two sentences
    "well yes I can hear you",                               # a word in front
])
def test_own_words_coming_back_are_echo(heard):
    assert vs.matches_own_speech(heard, SAID)


@pytest.mark.parametrize("heard", [
    "can you hear me",                                       # the user's own question
    "great, you can hear me",                                # a reply in other words
    "open the trailer folder",                               # an instruction
    "yes",                                                   # a short answer to a question it asked
    "no thanks",
    "delete hello dot txt",
])
def test_the_users_own_words_are_not_echo(heard):
    assert not vs.matches_own_speech(heard, SAID)


def test_a_shared_run_of_four_words_is_echo_and_short_words_never_are():
    said = ["The new trailer is in your working folder now."]
    assert vs.matches_own_speech("something something in your working folder", said)
    assert not vs.matches_own_speech("my other folder please", said)
    for answer in ("yes", "no", "yes go ahead", "no don't post it"):
        assert not vs.matches_own_speech(answer, ["Opus wants to post this. Yes or no?"]), f"an answer must get through: {answer}"
    assert not vs.matches_own_speech("anything at all", [])


# ----- own voice and turn-taking ------------------------------------------------------------------

def test_own_voice_rule():
    assert vs.own_voice(0.70, None), "not enrolled: like the assistant is enough"
    assert not vs.own_voice(0.50, None)
    assert not vs.own_voice(0.70, 0.80), "more like the user: the user"
    assert vs.own_voice(0.80, 0.60)
    assert not vs.own_voice(None, 0.2)


def test_open_mic_modes_wait_longer_after_playback():
    assert vs.holdoff_s("push_to_talk") == pytest.approx(0.35)
    assert vs.holdoff_s("always") == pytest.approx(1.0)
    assert vs.holdoff_s("wake_word") == pytest.approx(1.0)


# ----- echo cancellation ----------------------------------------------------------------------------

def speechlike(seconds, seed, sr=16000):
    """Noise shaped like speech: syllable-rate bursts of a few formant-ish bands."""
    rng = np.random.default_rng(seed)
    n = int(seconds * sr)
    t = np.arange(n) / sr
    x = sum(np.sin(2 * np.pi * f * t + rng.uniform(0, 6)) * rng.uniform(0.3, 1.0) for f in (180, 420, 900, 1700, 2600))
    x = x * (0.5 + 0.5 * np.sign(np.sin(2 * np.pi * 4.3 * t + rng.uniform(0, 6))))
    x += rng.standard_normal(n) * 0.3
    return (0.25 * x / np.max(np.abs(x))).astype(np.float32)


def bursts(seconds, seed, sr=16000):
    """Another talker: noise bursts at a different rate, nothing like the speakers' sound."""
    rng = np.random.default_rng(seed)
    n = int(seconds * sr)
    t = np.arange(n) / sr
    gate = (np.sin(2 * np.pi * 2.7 * t) > 0.2).astype(np.float32)
    return (rng.standard_normal(n) * 0.12 * gate).astype(np.float32)


def db(x):
    return 10 * np.log10(np.mean(np.square(x)) + 1e-12)


def run_loop(aec, far, near, delay_s=0.12, gain=0.35):
    """The speakers play `far`; the microphone hears its echo (delayed, quieter) plus `near`."""
    d = int(delay_s * 16000)
    echo = np.concatenate([np.zeros(d, np.float32), far[:-d] * gain])
    mic = echo + near
    out = []
    for i in range(0, far.size - vs.BLOCK + 1, vs.BLOCK):
        aec.render(far[i:i + vs.BLOCK], 16000)
        out.append(aec.capture(mic[i:i + vs.BLOCK]))
    o = np.concatenate(out)
    return mic[:o.size], o


def test_echo_canceller_removes_the_speakers_and_keeps_the_user():
    aec = vs.echo_canceller()
    if aec is None:
        pytest.skip("livekit (echo cancellation) isn't installed")
    far = speechlike(8.0, 1)
    mic, out = run_loop(aec, far, np.zeros_like(far))
    k = 3 * 16000                                     # after it has settled
    erle = db(mic[k:]) - db(out[k:])
    assert erle > 20, f"echo reduced by only {erle:.1f} dB"
    # The user talking over it is still there.
    aec2 = vs.echo_canceller()
    near = np.concatenate([np.zeros(4 * 16000, np.float32), bursts(4.0, 2)])
    mic, out = run_loop(aec2, far, near)
    user = slice(5 * 16000, 8 * 16000 - vs.BLOCK)
    kept = db(out[user]) - db(near[user])
    assert kept > -10, f"the user's speech lost {-kept:.1f} dB"


def test_the_echo_canceller_hears_what_is_played():
    s = InProcess_aec()
    try:
        assert s.svc.echo_cancellation_state() == "on"
        assert s.audio.player.on_read is not None, "the speakers' output is its reference"
        seen = []
        s.svc.aec.render = lambda x, sr: seen.append(x.size)
        s.audio.player.push(np.ones(2400, np.float32) * 0.1)
        deadline = time.time() + 3
        while sum(seen) < 2400 and time.time() < deadline:
            time.sleep(0.05)
        assert sum(seen) >= 2400
        s.svc.cfg["echo_cancellation"] = False
        assert s.svc.echo_cancellation_state() == "off"
    finally:
        s.close()


def InProcess_aec():
    import tempfile
    if vs.echo_canceller() is None:
        pytest.skip("livekit (echo cancellation) isn't installed")
    s = InProcess(tempfile.mkdtemp(), stub=True, aec=True)
    s.wait_ready(30)
    return s


# ----- the service drops its own echo, never the user's push-to-talk ----------------------------------

def test_own_words_heard_in_an_open_mic_mode_are_ignored_but_push_to_talk_is_not(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always", vad_silence_ms=500)
    c.wait("config_ok")
    c.send(cmd="speak", id="s1", text="Hello there, how are you doing today?")
    c.wait("spoken", lambda e: e["id"] == "s1", timeout=20)
    # Whatever is heard now "says" the assistant's sentence back (as an echo would).
    svc.svc.stt.transcribe = lambda audio, language=None: ("hello there how are you doing today", "en")
    t = time.perf_counter()
    time.sleep(1.1)                                         # past the open-mic holdoff
    svc.audio.feed(np.concatenate([tone(300, 1.2), silence(1.0)]))
    e = c.wait("echo_ignored", after=t, timeout=10)
    assert e["reason"] == "own_words" and "hello there" in e["text"]
    assert not c.of("utterance", t)
    # Held push-to-talk is the user talking: never filtered.
    c.send(cmd="config", listen_mode="push_to_talk")
    c.wait("config_ok")
    t = time.perf_counter()
    c.send(cmd="ptt", down=True)
    svc.audio.feed(tone(300, 1.0))
    time.sleep(1.2)
    c.send(cmd="ptt", down=False)
    u = c.wait("utterance", after=t, timeout=10)
    assert "hello there" in u["text"]
    c.close()


def test_the_assistants_own_voice_is_ignored(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always", vad_silence_ms=500)
    c.wait("config_ok")
    # The assistant's voice "sounds like" this tone.
    svc.svc.assistant_emb = svc.svc.speaker.embed(tone(300, 1.5))
    t = time.perf_counter()
    svc.audio.feed(np.concatenate([tone(300, 1.2), silence(1.0)]))
    e = c.wait("echo_ignored", after=t, timeout=10)
    assert e["reason"] == "own_voice"
    # Another voice is heard.
    t = time.perf_counter()
    time.sleep(1.1)
    svc.audio.feed(np.concatenate([tone(800, 1.2), silence(1.0)]))
    c.wait("utterance", after=t, timeout=10)
    c.close()


def test_no_capture_starts_by_itself_within_a_second_of_playback_in_open_mic_modes(svc):
    c = Client(svc.port)
    c.send(cmd="config", listen_mode="always", vad_silence_ms=500)
    c.wait("config_ok")
    c.send(cmd="speak", id="s2", text="Short line.")
    c.wait("spoken", lambda e: e["id"] == "s2", timeout=20)
    t = time.perf_counter()
    svc.audio.feed(np.concatenate([tone(800, 0.5), silence(1.0)]))     # right after playback
    time.sleep(2.0)
    assert not c.of("utterance", t), "nothing starts within the holdoff"
    time.sleep(0.5)
    t = time.perf_counter()
    svc.audio.feed(np.concatenate([tone(800, 1.0), silence(1.0)]))     # later: heard
    c.wait("utterance", after=t, timeout=10)
    c.close()


@pytest.fixture
def svc(tmp_models):
    s = InProcess(tmp_models, stub=True)
    s.wait_ready(30)
    yield s
    s.close()
