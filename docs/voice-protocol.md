# Voice service protocol (version 1)

The voice service is the local Python program that listens to the microphone, turns speech into text,
checks that it is the enrolled user's voice, and speaks the assistant's replies. Craft Anything starts
it, talks to it over one WebSocket, and stops it. Nothing in it uses a cloud service.

Source: `crates/engines/src/voice/scripts/voice_service.py` (one self-contained file, because it runs
with `python -I`). Extra packages: `voice_requirements.txt` next to it.

## Starting and stopping

```
<voice python> -I voice_service.py --host 127.0.0.1 --port 8191 --models-dir <dir>
               [--device cuda:0|cpu] [--parent-pid <pid>] [--log-level INFO]
```

- `--models-dir` (required): where models are downloaded when they are not already in the Hugging Face
  cache, and where the service keeps its own files (see "Files" below).
- `--device`: `cuda:0` by default when CUDA is available, otherwise `cpu` (works, much slower).
- `--parent-pid`: the service polls this process every second and shuts down when it is gone.
- `--port 0` picks a free port; the READY line says which.

stdout carries exactly one line, printed as soon as the socket listens:

```
READY {"port":8191,"version":1}
```

Everything else (logs, library warnings) goes to stderr. If the port is taken the service logs the
reason and exits with code 2 without printing READY.

**Models load after READY**, in the background (about 20-30 s when cached; the first run also
downloads the speaker model and, for built-in wake words, the openWakeWord files, and designs the
default assistant voice). `state.models_ready` turns true and a `status` event is sent when they are
usable. Commands sent earlier are accepted: `speak` waits for the models, push-to-talk audio is kept and
transcribed once they are ready.

It stops on `{"cmd":"shutdown"}`, when the parent process dies, or on Ctrl+C; exit code 0.

`--check-models` prints one JSON line saying which models/files are present and exits (nothing is
downloaded): `{"stt":true,"tts":true,"voice_design":true,"speaker":false,"wake_word":false,"vad":true,"default_voice":false,"speaker_profile":false}`.
Use it to decide whether to ask the person before a first start that downloads models.

## HTTP

`GET /health` → `{"ok":true,"version":1,"state":{...the state fields below...},"models":{...}}`

`models` maps `vad`, `stt`, `speaker`, `tts`, `wake_word` to `"pending"`, `"ready"`, `"error: ..."`,
or for `wake_word` `"off"`, `"openwakeword"` or `"whisper"`.

## WebSocket `ws://host:port/ws`

JSON text frames. One client at a time: a new connection closes the previous one (close code 4000)
and immediately receives a `state` event. Any command may carry an `"id"`; replies to it
(`config_ok`, `devices`, `status`, `voice_designed`, `error`) echo it. `speak` uses its `id` as the
utterance id of the `spoken` event.

### Commands (client → service)

| Command | Effect |
|---|---|
| `{"cmd":"config", ...fields}` | Apply settings (any subset). Replies `config_ok`; each rejected field also produces an `error` (non-fatal) and keeps the old value. |
| `{"cmd":"ptt","down":true\|false}` | Push-to-talk key. Down: stops interruptible speech (`barge_in`) and starts capturing (with ~200 ms of audio from before the key). Up: ends the utterance. Works in every listen mode. |
| `{"cmd":"speak","id":"s1","text":"...","interruptible":true}` | Speak text sentence by sentence; queued behind anything already speaking. |
| `{"cmd":"stop_speaking"}` | Stop now and clear the queue (every dropped or cut item gets `spoken` with `interrupted:true`). |
| `{"cmd":"enroll_start"}` / `{"cmd":"enroll_stop"}` | Record the user's voice for verification (see Enrolment). |
| `{"cmd":"design_voice","description":"...","sample_text":"..."}` | Design a new assistant voice with Qwen3-TTS VoiceDesign; replies `voice_designed`. It is not applied until a `config` sets `tts_voice`. |
| `{"cmd":"devices"}` | Replies `devices`. |
| `{"cmd":"status"}` | Replies `status`. |
| `{"cmd":"shutdown"}` | Exit. |

`config` fields:

| Field | Default | Meaning |
|---|---|---|
| `listen_mode` | `"push_to_talk"` | `push_to_talk`, `wake_word` or `always` |
| `wake_word` | `"hey craft"` | Phrase (or a path to a custom openWakeWord `.onnx`) |
| `input_device` / `output_device` | `null` | Device name from `devices` (exact, else first containing it); `null` = system default |
| `speaker_threshold` | `0.55` | Cosine score an utterance needs to count as the user |
| `language` | `"en"` | Whisper language code; `null` or `""` = detect per utterance. Also picks the TTS language. |
| `tts_voice` | `null` | `{"ref_audio":path,"ref_text":str}` = the assistant voice (a `voice_designed` result); `null` = the default voice |
| `vad_silence_ms` | `700` | Silence that ends an utterance (200-5000) |

### Events (service → client)

| Event | When |
|---|---|
| `{"type":"state","listening":bool,"capturing":bool,"speaking":bool,"mode":"...","enrolled":bool,"enrolling":bool,"models_ready":bool,"loading":"..."}` | On every change, and to a newly connected client |
| `{"type":"wake","word":"hey craft","score":0.8}` | The wake word was heard |
| `{"type":"partial","text":"open the tra"}` | While capturing, at most every 500 ms, only when the text changed |
| `{"type":"utterance","text":"...","speaker_ok":true,"speaker_score":0.71,"duration_s":2.4,"lang":"en","verified":true,"latency_ms":180}` | A finished utterance |
| `{"type":"barge_in"}` | The user started talking (or pressed push-to-talk) while the assistant spoke: playback stopped, queue cleared |
| `{"type":"spoken","id":"s1","interrupted":false}` | A `speak` item finished, was stopped, or was dropped from the queue |
| `{"type":"enroll_progress","seconds":12.5,"needed":20}` | After each enrolment sentence |
| `{"type":"enrolled","ok":true,"seconds":21.3}` / `{"type":"enrolled","ok":false,"message":"..."}` | Enrolment finished |
| `{"type":"voice_designed","ref_audio":"...","ref_text":"..."}` | A designed voice was saved |
| `{"type":"devices","inputs":[...],"outputs":[...]}` | Reply to `devices` (names of the default host API's devices) |
| `{"type":"config_ok"}` | Reply to `config` |
| `{"type":"status","version":1,"state":{...},"models":{...},"device":"cuda:0","wake_engine":"whisper","wake_word":"...","config":{...},"voice":{...},"speaker_threshold":0.55,"queue":0,"vram_mb":{"allocated":..,"peak":..,"reserved":..}}` | Reply to `status`; also sent once when the models are ready |
| `{"type":"error","message":"...","fatal":false}` | Something failed; `fatal:true` only when the models could not load |

Fields beyond the contract (`enrolling`, `models_ready`, `loading`, `verified`, `latency_ms`,
`seconds`, `message`) are additive; clients may ignore them.

## Listening

The microphone is read at 16 kHz in 32 ms blocks; Silero VAD scores every block.

- **push_to_talk**: only the key captures. Utterances end on key up (max 60 s).
- **always**: ~100 ms of speech starts a capture (with 320 ms of audio from before it); `vad_silence_ms`
  of silence ends it (max 60 s).
- **wake_word**: two engines.
  - Built-in openWakeWord phrases (`hey jarvis`, `alexa`, `hey mycroft`, `hey rhasspy`) or a custom
    openWakeWord `.onnx` path: the model scores the audio continuously; a score of 0.5 sends `wake` and
    the next speech (within 6 s) is the command.
  - **Any other phrase, including the default `hey craft`**: spotted with Whisper. Each speech
    segment is transcribed; if it starts with the phrase (fuzzy match, so "Hey, Kraft" counts), `wake`
    is sent as soon as a partial shows it, and the rest of the sentence is the command
    ("Hey Craft, open the downloads folder" → utterance "open the downloads folder"). The phrase alone
    arms a 6 s window for the command. Speech that doesn't start with the phrase is ignored. A
    trained "hey craft" openWakeWord model is a later step; it would plug in as a custom `.onnx`.

Partials re-transcribe the capture so far (Whisper large-v3-turbo) about twice a second. Finals are
transcribed with priority. Captures with no speech and Whisper's typical noise phrases on very short
audio ("Thank you.", "you") are dropped without an event.

**Speaker verification.** Every utterance is scored against the enrolled profile with SpeechBrain
ECAPA-TDNN (cosine similarity of the voiced part of the audio). `speaker_ok` = score ≥
`speaker_threshold`, `verified:true`. **Before enrolment** there is nothing to compare with:
`speaker_ok:true`, `speaker_score:0`, `verified:false` (so the app works on first use; the UI should
ask the person to enrol).

## Speaking

Text is split into sentences (long ones at commas). Each sentence is synthesised with Qwen3-TTS 1.7B
Base in the assistant voice and **streamed frame by frame**: the first ~320 ms of audio plays as soon
as it exists, the rest follows in ~640 ms chunks while the next frames are generated (about 3x faster
than real time on an RTX 5070 Ti). The service runs the model's own modules with static caches and one
CUDA graph per audio frame; the stock generate loop was slower than real time on this GPU.

**Barge-in**:
- push-to-talk down while speaking always interrupts (if the item is `interruptible`);
- in `always`/`wake_word` mode, ~250 ms of speech during playback is checked first so the assistant's
  own voice coming back from the speakers doesn't interrupt it: with a profile, the speech must
  score ≥ 0.8 × threshold against the user and closer to the user than to the assistant voice;
  without a profile, it must not sound like the assistant voice (score < 0.5 against it). Then
  playback stops, the queue is cleared (`barge_in`, `spoken` with `interrupted:true`), and the speech
  becomes the next utterance. For 350 ms after playback ends, speech doesn't start a capture (room echo).
- `interruptible:false` items are only stopped by `stop_speaking`.

There is no acoustic echo cancellation; with loud speakers and no profile, headphones are the safe
choice.

## Enrolment

`enroll_start` → every speech segment (VAD, or push-to-talk) is kept as enrolment audio instead of
becoming an utterance; `enroll_progress` follows each one with the voiced seconds so far (`needed:20`).
At 20 s the profile is saved automatically (`enrolled ok:true`); `enroll_stop` finishes early (ok when
there are at least 6 s, else `ok:false` with a message, and the old profile stays). The profile is the
length-weighted mean of the segments' ECAPA embeddings.

## Assistant voices

`design_voice` loads Qwen3-TTS VoiceDesign for that call only (about 4 GB of VRAM for ~10-20 s), speaks
`sample_text` in the described voice, saves it and frees the model. The first start designs a
default voice the same way when no `tts_voice` is configured.

## Files under `--models-dir`

| Path | What |
|---|---|
| `hf/` | Whisper / Qwen3-TTS downloads when they aren't in the Hugging Face cache |
| `speechbrain/spkrec-ecapa-voxceleb/` | Speaker model |
| `openwakeword/` | openWakeWord feature models and built-in wake words |
| `voices/default.wav` (+ `.json`) | The default assistant voice |
| `voices/<slug>-<time>.wav` (+ `.json`) | Designed voices (`ref_text` in the `.json`) |
| `speaker/profile.json` | `{"version":1,"model":...,"seconds":...,"embedding":[...192 floats]}` |
| `speaker/enrolment/segment-NN.wav` | The last enrolment's audio (local only) |

## Resources (RTX 5070 Ti, measured)

See the numbers in `target/voice-test-report.json` after running the real-model tests; typical:
Whisper large-v3-turbo fp16 ~1.6 GB, Qwen3-TTS Base bf16 + caches ~4.5 GB; peak ~6 GB while
idle-resident (+4 GB transiently during `design_voice`). Speaker model and VAD run on the CPU.

## Test audio injection (`--allow-inject`)

For end-to-end tests only. Started with `--allow-inject`, the service also takes
`{"cmd":"inject_wav","path":"C:\…\hello.wav","ptt":true}`: the WAV (any rate, mixed to mono) is fed in
real time into the same pipeline as the microphone, which is muted meanwhile. `ptt` holds push-to-talk
down around it; left out, it does so in push-to-talk mode unless enrolling (enrolment and the other modes
rely on the voice activity detector, so ~1 s of silence follows the clip). When the clip has been fed it
sends `{"type":"injected","path":…,"seconds":…}` (with the command's `id`). Without the flag the
command is refused with an `error` and nothing is fed. Craft Anything never passes the flag from its
settings; only test code sets `VoiceSetup::allow_inject`.

## Tests

From the repository root, with the voice Python:

```
<voice python> -I -m pytest crates/engines/src/voice/scripts/tests -q
```

- `test_protocol.py`: the real entry point and pipeline with stand-in models (`--stub-models`,
  `--fake-audio`, test-only flags): READY line, `/health`, every command and event, one-client rule,
  parent watchdog, push-to-talk, VAD segmentation, partials, barge-in, enrolment and verification,
  voice design. No GPU, no microphone; ~40 s.
- `test_real_models.py`: the real models on `cuda:0` with a virtual microphone fed from voices designed
  with Qwen3-TTS (a "user" and an "other person"): STT round trip and latency, push-to-talk latency,
  TTS first-audio latency and intelligibility (the played audio is transcribed back), barge-in,
  enrolment and verification, the other voice unable to interrupt, both wake-word engines, VRAM.
  Skipped without CUDA or with `VOICE_REAL=0`; models in `VOICE_TEST_MODELS_DIR` (default
  `target/voice-test-models`); writes `target/voice-test-report.json`.
