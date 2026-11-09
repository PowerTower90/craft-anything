"""Local voices for game rebuilds: transcribe an original line, design a new character voice, speak lines in it.

Reads one JSON job from a file (argv[1]) and prints one JSON result line: {"ok": true, ...} or {"ok": false, "error": "..."}.
Models download to the Hugging Face cache on first use (Qwen3-TTS 1.7B VoiceDesign + Base, Whisper large-v3-turbo).

Jobs:
  {"op": "transcribe", "audio": "line.ogg", "language": "en"}                          -> {"text": "..."}
  {"op": "design", "description": "gruff old miner, deep...", "sample_text": "...",
   "language": "English", "out": "voices/miner.wav"}                                   -> {"out", "seconds", "sample_text"}
  {"op": "speak", "ref_audio": "voices/miner.wav", "ref_text": "<sample_text>",
   "lines": [{"text": "...", "out": "lines/a.wav"}, ...], "language": "English"}        -> {"made": [...], "failed": [...]}
The voice is designed once from words (never from an original recording); every line is then spoken in that designed voice.
"""
import json, os, sys, time, traceback

DESIGN = "Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign"
BASE = "Qwen/Qwen3-TTS-12Hz-1.7B-Base"
WHISPER = "openai/whisper-large-v3-turbo"


def device():
    import torch
    return ("cuda:0", torch.bfloat16) if torch.cuda.is_available() else ("cpu", torch.float32)


def tts(name):
    from qwen_tts import Qwen3TTSModel
    dev, dt = device()
    return Qwen3TTSModel.from_pretrained(name, device_map=dev, dtype=dt)


def write(path, wav, sr):
    import soundfile as sf
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    sf.write(path, wav, sr)
    return round(len(wav) / sr, 2)


def transcribe(job):
    import torch
    from transformers import pipeline
    dev, dt = device()
    asr = pipeline("automatic-speech-recognition", model=WHISPER, torch_dtype=dt if dev != "cpu" else torch.float32, device=dev)
    out = asr(job["audio"], generate_kwargs={"language": job.get("language", "en"), "task": "transcribe"}, return_timestamps=False)
    return {"text": out["text"].strip()}


def design(job):
    sample = job.get("sample_text") or "Alright, team, listen up. We dig in, we hold the line, and we all go home rich."
    model = tts(DESIGN)
    wavs, sr = model.generate_voice_design(text=sample, instruct=job["description"], language=job.get("language", "English"))
    return {"out": job["out"], "seconds": write(job["out"], wavs[0], sr), "sample_text": sample}


def speak(job):
    model = tts(BASE)
    prompt = model.create_voice_clone_prompt(ref_audio=job["ref_audio"], ref_text=job["ref_text"])
    made, failed = [], []
    for line in job["lines"]:
        try:
            wavs, sr = model.generate_voice_clone(text=line["text"], language=job.get("language", "English"), voice_clone_prompt=prompt)
            made.append({"out": line["out"], "seconds": write(line["out"], wavs[0], sr)})
        except Exception as e:  # one bad line doesn't stop the rest
            failed.append({"out": line.get("out"), "error": str(e)[:300]})
    return {"made": made, "failed": failed}


def main():
    t0 = time.time()
    try:
        job = json.load(open(sys.argv[1], encoding="utf-8"))
        res = {"transcribe": transcribe, "design": design, "speak": speak}[job["op"]](job)
        res.update(ok=True, seconds_taken=round(time.time() - t0, 1))
    except Exception as e:
        res = {"ok": False, "error": f"{type(e).__name__}: {e}", "trace": traceback.format_exc()[-1500:]}
    print(json.dumps(res), flush=True)


if __name__ == "__main__":
    main()
