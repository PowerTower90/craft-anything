//! Voice lines with Qwen3-TTS in its own Python ([`VOICE_LINE_PY`]): each character gets a new
//! voice, designed once from a written description and kept as the character's reference clip
//! (`<voices>/<character>.wav`, with what it says in `<character>.json`); every line of that
//! character is then spoken in that voice, all in one run so the model loads once. The original
//! actor's voice is never used: an original recording is only transcribed (Whisper) for its words,
//! after decoding it to WAV when it's in a game's own format ([`super::audio::decode_sound`]).
//!
//! The helper takes one job (a JSON file) and prints one JSON line: `transcribe`, `design` or
//! `speak` (see the script's docstring).

use super::{Setup, file_stem};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::process::Process;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The voice helper (Python, Qwen3-TTS and Whisper).
pub const VOICE_LINE_PY: &str = include_str!("scripts/voice_line.py");

/// What a designed voice says in its reference clip (every common sound, a calm delivery).
pub const REFERENCE_TEXT: &str = "I have walked these roads for many years, and I still remember every stone, every river and every voice along the way.";

/// How long one run may take: loading the model (a minute or two the first time), then each line.
const RUN_BASE: Duration = Duration::from_secs(10 * 60);
const PER_LINE: Duration = Duration::from_secs(2 * 60);

/// One line to speak.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceLine {
    /// Names the line in the results.
    pub id: String,
    /// Whose line it is: one voice per character.
    pub character: String,
    /// The character's voice in words (used when the character has no voice yet).
    pub voice_description: String,
    /// The words; when empty, they're transcribed from `transcribe_from`.
    pub text: Option<String>,
    pub transcribe_from: Option<PathBuf>,
    /// The WAV to write.
    pub out: PathBuf,
}

/// What became of one line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineResult {
    pub id: String,
    pub ok: bool,
    pub out: PathBuf,
    /// The words spoken.
    pub text: Option<String>,
    pub error: Option<String>,
    /// How long the line is, in seconds.
    pub length: Option<f64>,
    /// The character's reference clip, and whether this run designed it.
    pub reference: PathBuf,
    pub designed_voice: bool,
}

/// A character's voice, as kept next to its reference clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceCard {
    pub character: String,
    pub description: String,
    /// What the reference clip says (the clone needs it).
    pub reference_text: String,
}

/// A character's reference clip and card.
pub fn voice_files(voices_dir: &Path, character: &str) -> (PathBuf, PathBuf) {
    let stem = file_stem(character);
    (voices_dir.join(format!("{stem}.wav")), voices_dir.join(format!("{stem}.json")))
}

/// The character's voice, when it has one.
pub fn voice_of(voices_dir: &Path, character: &str) -> Option<VoiceCard> {
    let (wav, card) = voice_files(voices_dir, character);
    if !wav.is_file() {
        return None;
    }
    serde_json::from_slice(&std::fs::read(card).ok()?).ok()
}

fn python(setup: &Setup) -> Result<&Path, ToolError> {
    setup.voice_python.as_deref().filter(|p| p.is_file()).ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "Voices aren't set up.")
            .with_remediation("Set the Python that has Qwen3-TTS in Settings → Local generation → Voices (local_gen.voice_python).")
    })
}

/// Run one job on the voice helper; its answer when it says ok.
fn run_job(setup: &Setup, job: &Value, timeout: Duration, cancel: &CancelToken) -> Result<Value, ToolError> {
    let python = python(setup)?;
    let script = setup.write_script("voice_line.py", VOICE_LINE_PY)?;
    let path = setup.scripts_dir.join(format!("voice_job_{}_{}.json", std::process::id(), super::now_nanos()));
    let body = serde_json::to_vec_pretty(job).map_err(|e| ToolError::internal(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| ToolError::io(path.display(), e))?;
    let c = cancel.clone();
    let run = Process::new(python)
        .arg("-I")
        .arg(&script)
        .arg(&path)
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .timeout(timeout)
        .cancel_when(move || c.is_cancelled())
        .label("the voice helper")
        .run_checked();
    let _ = std::fs::remove_file(&path);
    let out = run?;
    let text = out.stdout_text();
    let answer = text.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(Value::is_object)).ok_or_else(|| {
        ToolError::new(ErrorCode::GarbledOutput, "The voice helper didn't answer.")
            .with_details(json!({"output": craft_anything_platform::process::output_excerpt(&format!("{text}\n{}", out.stderr_text()))}))
    })?;
    if answer["ok"] != true {
        let why = answer["error"].as_str().unwrap_or("it gave no reason");
        return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The voice helper failed: {why}")).with_details(json!({"trace": answer["trace"]})));
    }
    Ok(answer)
}

/// The words in a recording (Whisper). A recording in a game's own format is decoded to WAV first
/// when the sound decoder is set up.
pub fn transcribe(setup: &Setup, audio: &Path, cancel: &CancelToken) -> Result<String, ToolError> {
    let ext = audio.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let decoded = if matches!(ext.as_str(), "wav" | "flac" | "mp3" | "ogg") {
        None
    } else {
        let out = std::env::temp_dir().join(format!("craft-anything-transcribe-{}-{}.wav", std::process::id(), super::now_nanos()));
        super::audio::decode_sound(setup, audio, &out, cancel)?;
        Some(out)
    };
    let job = json!({"op": "transcribe", "audio": decoded.as_deref().unwrap_or(audio), "language": "en"});
    let r = run_job(setup, &job, RUN_BASE, cancel);
    if let Some(d) = decoded {
        let _ = std::fs::remove_file(d);
    }
    Ok(r?["text"].as_str().unwrap_or_default().trim().to_string())
}

/// Design a new voice for `character` from `description` and keep it in `voices_dir`.
pub fn design_voice(setup: &Setup, voices_dir: &Path, character: &str, description: &str, cancel: &CancelToken) -> Result<VoiceCard, ToolError> {
    let (wav, card_path) = voice_files(voices_dir, character);
    std::fs::create_dir_all(voices_dir).map_err(|e| ToolError::io(voices_dir.display(), e))?;
    let job = json!({"op": "design", "description": description.trim(), "sample_text": REFERENCE_TEXT, "language": "English", "out": wav});
    let r = run_job(setup, &job, RUN_BASE, cancel)?;
    if !wav.is_file() {
        return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The voice for {character} wasn't made.")));
    }
    let card = VoiceCard {
        character: character.to_string(),
        description: description.trim().to_string(),
        reference_text: r["sample_text"].as_str().unwrap_or(REFERENCE_TEXT).to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&card).map_err(|e| ToolError::internal(e.to_string()))?;
    std::fs::write(&card_path, bytes).map_err(|e| ToolError::io(card_path.display(), e))?;
    Ok(card)
}

/// Speak `lines`: their words found first (given, or transcribed), each character's voice kept or
/// designed now (`voices_dir` keeps them), then each character's lines in one run. One line
/// failing doesn't stop the rest; fails as a whole only when voices aren't set up.
pub fn voice_lines(setup: &Setup, voices_dir: &Path, lines: &[VoiceLine], cancel: &CancelToken, progress: &dyn Fn(&str)) -> Result<Vec<LineResult>, ToolError> {
    python(setup)?;
    let mut results: Vec<LineResult> = lines
        .iter()
        .map(|l| LineResult {
            id: l.id.clone(),
            ok: false,
            out: l.out.clone(),
            text: l.text.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string),
            error: None,
            length: None,
            reference: voice_files(voices_dir, &l.character).0,
            designed_voice: false,
        })
        .collect();
    // The words of the lines that have none.
    for (r, line) in results.iter_mut().zip(lines) {
        if r.text.is_some() {
            continue;
        }
        cancel.check()?;
        match &line.transcribe_from {
            Some(audio) => {
                progress(&format!("{}: transcribing the original", line.id));
                match transcribe(setup, audio, cancel) {
                    Ok(t) if !t.is_empty() => r.text = Some(t),
                    Ok(_) => r.error = Some("No words were heard in the original.".into()),
                    Err(e) => r.error = Some(e.for_people()),
                }
            }
            None => r.error = Some("The line has no words: give its text, or the original to transcribe.".into()),
        }
    }
    // Each character, in the order they come.
    let mut order: Vec<String> = Vec::new();
    let mut by_character: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        let key = file_stem(&line.character);
        if !by_character.contains_key(&key) {
            order.push(key.clone());
        }
        by_character.entry(key).or_default().push(i);
    }
    for key in order {
        let idx: Vec<usize> =
            by_character.remove(&key).unwrap_or_default().into_iter().filter(|i| results.get(*i).is_some_and(|r| r.error.is_none())).collect();
        let Some(first) = idx.first().and_then(|i| lines.get(*i)) else { continue };
        cancel.check()?;
        let card = match voice_of(voices_dir, &first.character) {
            Some(card) => Ok(card),
            None => {
                progress(&format!("designing a voice for {}", first.character));
                let description = idx.iter().filter_map(|i| lines.get(*i)).map(|l| l.voice_description.trim()).find(|d| !d.is_empty()).unwrap_or_default();
                if description.is_empty() {
                    Err(ToolError::invalid(format!("{} has no voice yet: describe it in voice_description.", first.character)))
                } else {
                    design_voice(setup, voices_dir, &first.character, description, cancel).inspect(|_| {
                        for i in &idx {
                            if let Some(r) = results.get_mut(*i) {
                                r.designed_voice = true;
                            }
                        }
                    })
                }
            }
        };
        let card = match card {
            Ok(c) => c,
            Err(e) => {
                for i in &idx {
                    if let Some(r) = results.get_mut(*i) {
                        r.error = Some(e.for_people());
                    }
                }
                continue;
            }
        };
        progress(&format!("speaking {} line(s) as {}", idx.len(), first.character));
        let mut speak: Vec<Value> = Vec::new();
        for i in &idx {
            let (Some(line), Some(r)) = (lines.get(*i), results.get(*i)) else { continue };
            if let Some(dir) = line.out.parent() {
                std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir.display(), e))?;
            }
            speak.push(json!({"text": r.text, "out": line.out}));
        }
        let (wav, _) = voice_files(voices_dir, &first.character);
        let job = json!({"op": "speak", "ref_audio": wav, "ref_text": card.reference_text, "lines": speak, "language": "English"});
        let timeout = RUN_BASE + PER_LINE * u32::try_from(idx.len()).unwrap_or(u32::MAX).min(2000);
        match run_job(setup, &job, timeout, cancel) {
            Ok(answer) => {
                let same = |v: &Value, p: &Path| v["out"].as_str().is_some_and(|o| Path::new(o) == p);
                for i in &idx {
                    let (Some(line), Some(r)) = (lines.get(*i), results.get_mut(*i)) else { continue };
                    let made = answer["made"].as_array().and_then(|m| m.iter().find(|v| same(v, &line.out)));
                    let failed = answer["failed"].as_array().and_then(|m| m.iter().find(|v| same(v, &line.out)));
                    if made.is_some() && line.out.is_file() {
                        r.ok = true;
                        r.length = made.and_then(|m| m["seconds"].as_f64());
                    } else {
                        r.error = Some(failed.and_then(|f| f["error"].as_str()).unwrap_or("The voice helper didn't make this line.").to_string());
                    }
                }
            }
            Err(e) => {
                for i in &idx {
                    if let Some(r) = results.get_mut(*i) {
                        r.error = Some(e.for_people());
                    }
                }
            }
        }
    }
    Ok(results)
}
