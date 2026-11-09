//! Sounds and music on a ComfyUI server: sound effects, ambience and UI sounds with Stable Audio 3
//! Medium, music and ambience loops with ACE-Step 1.5 (XL turbo). The server saves FLAC; games
//! want WAV, so it's turned into WAV here ([`flac_to_wav`]).

use super::comfy::{self, Comfy};
use super::{Setup, write};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::process::Process;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long decoding one sound may take.
const DECODE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Stable Audio 3 Medium's files (on the audio server).
pub const SOUND_CHECKPOINT: &str = "stable_audio_3_medium.safetensors";
pub const SOUND_TEXT_ENCODER: &str = "t5gemma_b_b_ul2.safetensors";
/// ACE-Step 1.5's files.
pub const MUSIC_UNET: &str = "acestep_v1.5_xl_turbo_bf16.safetensors";
pub const MUSIC_TEXT_ENCODER_SMALL: &str = "qwen_0.6b_ace15.safetensors";
pub const MUSIC_TEXT_ENCODER_LARGE: &str = "qwen_4b_ace15.safetensors";
pub const MUSIC_VAE: &str = "ace_1.5_vae.safetensors";
/// The node that saves the audio, in both graphs.
pub const AUDIO_OUTPUT_NODE: &str = "20";

/// How long a sound may be (Stable Audio 3 makes up to 47 s), and a piece of music.
pub const SOUND_SECONDS: (f32, f32) = (0.5, 47.0);
pub const MUSIC_SECONDS: (f32, f32) = (10.0, 240.0);

/// What a sound is for, in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SoundKind {
    /// A sound effect (a hit, a shot, a creature).
    Sfx,
    /// A background ambience.
    Ambience,
    /// A menu or HUD sound.
    Ui,
    /// Music.
    Music,
}

pub fn clamp_seconds(kind: SoundKind, seconds: Option<f32>) -> f32 {
    let (lo, hi) = if kind == SoundKind::Music { MUSIC_SECONDS } else { SOUND_SECONDS };
    let default = match kind {
        SoundKind::Music => 60.0,
        SoundKind::Ambience => 20.0,
        _ => 2.0,
    };
    let s = seconds.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(default);
    s.clamp(lo, hi)
}

/// The prompt for a sound: what it is, how it should sound.
pub fn sound_prompt(kind: SoundKind, description: &str, style: &str) -> String {
    let what = match kind {
        SoundKind::Sfx => "Game sound effect",
        SoundKind::Ambience => "Game ambience, seamless background loop",
        SoundKind::Ui => "Game user interface sound, short and clean",
        SoundKind::Music => "Instrumental game music",
    };
    let style = style.trim().trim_end_matches('.');
    let style = if style.is_empty() { String::new() } else { format!(" {style}.") };
    format!("{what}: {}.{style} High quality, no speech.", description.trim().trim_end_matches('.'))
}

/// A Stable Audio 3 sound `seconds` long (the latent is at least a second; the sound is trimmed).
pub fn sound_graph(prompt: &str, seconds: f32, seed: u64, prefix: &str) -> Value {
    json!({
        "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": SOUND_CHECKPOINT}},
        "2": {"class_type": "CLIPLoader", "inputs": {"clip_name": SOUND_TEXT_ENCODER, "type": "stable_audio", "device": "default"}},
        "3": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": prompt}},
        "4": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": ""}},
        "5": {"class_type": "EmptyLatentAudio", "inputs": {"seconds": seconds.max(1.0), "batch_size": 1}},
        "6": {"class_type": "KSampler", "inputs": {
            "model": ["1", 0], "positive": ["3", 0], "negative": ["4", 0], "latent_image": ["5", 0],
            "seed": seed, "steps": 8, "cfg": 1, "sampler_name": "lcm", "scheduler": "simple", "denoise": 1
        }},
        "7": {"class_type": "VAEDecodeAudio", "inputs": {"samples": ["6", 0], "vae": ["1", 2]}},
        "8": {"class_type": "TrimAudioDuration", "inputs": {"audio": ["7", 0], "start_index": 0.0, "duration": seconds}},
        AUDIO_OUTPUT_NODE: {"class_type": "SaveAudio", "inputs": {"audio": ["8", 0], "filename_prefix": format!("craft/{prefix}")}},
    })
}

/// An ACE-Step 1.5 piece `seconds` long: `tags` say the genre, instruments and mood; empty
/// `lyrics` make it instrumental.
pub fn music_graph(tags: &str, lyrics: &str, seconds: f32, seed: u64, prefix: &str) -> Value {
    json!({
        "1": {"class_type": "UNETLoader", "inputs": {"unet_name": MUSIC_UNET, "weight_dtype": "default"}},
        "2": {"class_type": "VAELoader", "inputs": {"vae_name": MUSIC_VAE}},
        "3": {"class_type": "DualCLIPLoader", "inputs": {"clip_name1": MUSIC_TEXT_ENCODER_SMALL, "clip_name2": MUSIC_TEXT_ENCODER_LARGE, "type": "ace", "device": "default"}},
        "4": {"class_type": "TextEncodeAceStepAudio1.5", "inputs": {
            "clip": ["3", 0], "tags": tags, "lyrics": lyrics, "seed": seed, "bpm": 110, "duration": seconds,
            "timesignature": "4", "language": "en", "keyscale": "C major", "generate_audio_codes": true,
            "cfg_scale": 2.0, "temperature": 0.85, "top_p": 0.9, "top_k": 0, "min_p": 0.0
        }},
        "5": {"class_type": "ConditioningZeroOut", "inputs": {"conditioning": ["4", 0]}},
        "6": {"class_type": "EmptyAceStep1.5LatentAudio", "inputs": {"seconds": seconds, "batch_size": 1}},
        "7": {"class_type": "ModelSamplingAuraFlow", "inputs": {"model": ["1", 0], "shift": 3}},
        "8": {"class_type": "KSampler", "inputs": {
            "model": ["7", 0], "positive": ["4", 0], "negative": ["5", 0], "latent_image": ["6", 0],
            "seed": seed, "steps": 8, "cfg": 1, "sampler_name": "euler", "scheduler": "simple", "denoise": 1
        }},
        "9": {"class_type": "VAEDecodeAudio", "inputs": {"samples": ["8", 0], "vae": ["2", 0]}},
        AUDIO_OUTPUT_NODE: {"class_type": "SaveAudio", "inputs": {"audio": ["9", 0], "filename_prefix": format!("craft/{prefix}")}},
    })
}

/// A FLAC file as a WAV file (the same sample rate, channels and bit depth).
pub fn flac_to_wav(flac: &[u8]) -> Result<Vec<u8>, ToolError> {
    let bad = |e: claxon::Error| ToolError::new(ErrorCode::GarbledOutput, format!("The generated sound couldn't be read: {e}"));
    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(flac)).map_err(bad)?;
    let info = reader.streaminfo();
    let (channels, rate, bits) = (info.channels as u16, info.sample_rate, info.bits_per_sample as u16);
    if !matches!(bits, 8 | 16 | 24 | 32) || channels == 0 {
        return Err(ToolError::new(ErrorCode::GarbledOutput, format!("The generated sound has {bits}-bit samples, which aren't supported.")));
    }
    let bytes_per = usize::from(bits / 8);
    let mut data: Vec<u8> = Vec::with_capacity(info.samples.unwrap_or(0) as usize * usize::from(channels) * bytes_per);
    for s in reader.samples() {
        let s = s.map_err(bad)?;
        // WAV's 8-bit samples are unsigned; the rest little-endian signed.
        if bits == 8 {
            data.push((s + 128).clamp(0, 255) as u8);
        } else {
            data.extend_from_slice(&s.to_le_bytes()[..bytes_per]);
        }
    }
    let block = channels * (bits / 8);
    let mut out = Vec::with_capacity(data.len() + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(36 + data.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&data);
    Ok(out)
}

/// What a WAV file holds.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// Its length in seconds.
    pub seconds: f64,
}

/// Read a WAV file's format and length from its header (PCM or float); None when it isn't one.
pub fn wav_info(bytes: &[u8]) -> Option<WavInfo> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let u16_at = |i: usize| bytes.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (mut at, mut fmt, mut data) = (12usize, None, None);
    while at + 8 <= bytes.len() {
        let id = bytes.get(at..at + 4)?;
        let size = u32_at(at + 4)? as usize;
        if id == b"fmt " {
            fmt = Some((u16_at(at + 10)?, u32_at(at + 12)?, u32_at(at + 16)?, u16_at(at + 22)?));
        } else if id == b"data" {
            // A streamed WAV may say 0 or more than there is: count what's there.
            data = Some(size.min(bytes.len().saturating_sub(at + 8)));
        }
        at = at.checked_add(8 + size + (size & 1))?;
    }
    let (channels, sample_rate, byte_rate, bits) = fmt?;
    let data = data?;
    let seconds = if byte_rate > 0 { data as f64 / f64::from(byte_rate) } else { 0.0 };
    Some(WavInfo { sample_rate, channels, bits, seconds })
}

/// vgmstream's decoder: the configured one when it's there, else on the search path.
pub fn find_decoder(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    craft_anything_platform::which::which("vgmstream-cli")
}

/// Decode a sound in a game's own format (an FMOD bank, a Wwise stream, …) to `out` as WAV, with
/// vgmstream. Only to listen to it: its length, what it is, the words said. Never give the result
/// to a generator.
pub fn decode_sound(setup: &Setup, src: &Path, out: &Path, cancel: &CancelToken) -> Result<WavInfo, ToolError> {
    let decoder = find_decoder(setup.sound_decoder.as_deref()).ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "The sound decoder (vgmstream) isn't set up.")
            .with_remediation("Install vgmstream and set vgmstream-cli in Settings → Local generation → Sound decoder (local_gen.sound_decoder).")
    })?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir.display(), e))?;
    }
    let c = cancel.clone();
    Process::new(decoder).arg("-o").arg(out).arg(src).timeout(DECODE_TIMEOUT).cancel_when(move || c.is_cancelled()).label("the sound decoder").run_checked()?;
    let bytes = std::fs::read(out).map_err(|e| ToolError::io(out.display(), e))?;
    wav_info(&bytes).ok_or_else(|| ToolError::new(ErrorCode::GarbledOutput, format!("{} couldn't be decoded to a sound.", src.display())))
}

/// Run an audio graph on the audio server and save its sound at `out` as WAV.
fn run(server: &Comfy, setup: &Setup, graph: &Value, out: &Path, cancel: &CancelToken, progress: &dyn Fn(&str)) -> Result<u64, ToolError> {
    let id = server.queue(graph)?;
    let outputs = server.wait(&id, setup.limits, cancel, progress)?;
    let file = comfy::first_output(&outputs, AUDIO_OUTPUT_NODE)
        .ok_or_else(|| ToolError::new(ErrorCode::GarbledOutput, "The audio server finished without a sound."))?;
    let flac = server.view(&file)?;
    let wav = flac_to_wav(&flac)?;
    write(out, &wav)?;
    Ok(wav.len() as u64)
}

/// Make a sound (or music) on the audio server, saved at `out` as WAV.
#[allow(clippy::too_many_arguments)]
pub fn sound(
    setup: &Setup,
    kind: SoundKind,
    description: &str,
    style: &str,
    seconds: Option<f32>,
    seed: u64,
    out: &Path,
    cancel: &CancelToken,
    progress: &dyn Fn(&str),
) -> Result<u64, ToolError> {
    let server = setup.audio();
    let seconds = clamp_seconds(kind, seconds);
    let prefix = out.file_stem().and_then(|s| s.to_str()).map(super::file_stem).unwrap_or_else(|| "sound".into());
    let graph = if kind == SoundKind::Music {
        let style = style.trim().trim_end_matches('.');
        let tags = if style.is_empty() {
            format!("{}, instrumental, game soundtrack", description.trim())
        } else {
            format!("{}, {style}, instrumental, game soundtrack", description.trim())
        };
        music_graph(&tags, "", seconds, seed, &prefix)
    } else {
        sound_graph(&sound_prompt(kind, description, style), seconds, seed, &prefix)
    };
    run(&server, setup, &graph, out, cancel, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_graphs_have_what_the_models_need() {
        let g = sound_graph("Game sound effect: a door creaks.", 0.6, 3, "job/door");
        assert_eq!(g["1"]["inputs"]["ckpt_name"], SOUND_CHECKPOINT);
        assert_eq!(g["2"]["inputs"]["type"], "stable_audio");
        assert_eq!(g["5"]["inputs"]["seconds"], 1.0, "the latent is at least a second");
        assert!((g["8"]["inputs"]["duration"].as_f64().unwrap() - 0.6).abs() < 1e-6, "trimmed to the length asked for");
        assert_eq!(g["6"]["inputs"]["sampler_name"], "lcm");
        assert_eq!(g[AUDIO_OUTPUT_NODE]["class_type"], "SaveAudio");
        let m = music_graph("dark synth, cave", "", 90.0, 1, "job/theme");
        assert_eq!(m["3"]["inputs"]["type"], "ace");
        assert_eq!(m["4"]["inputs"]["lyrics"], "");
        assert_eq!(m["6"]["inputs"]["seconds"], 90.0);
        assert_eq!(m["7"]["inputs"]["shift"], 3);
        for graph in [&g, &m] {
            for node in graph.as_object().unwrap().values() {
                for v in node["inputs"].as_object().unwrap().values() {
                    if let Some([from, _]) = v.as_array().map(Vec::as_slice) {
                        assert!(graph.get(from.as_str().unwrap()).is_some(), "{v}");
                    }
                }
            }
        }
    }

    #[test]
    fn lengths_are_kept_in_range_and_prompts_say_what_it_is() {
        assert_eq!(clamp_seconds(SoundKind::Sfx, Some(0.1)), 0.5);
        assert_eq!(clamp_seconds(SoundKind::Sfx, Some(120.0)), 47.0);
        assert_eq!(clamp_seconds(SoundKind::Music, Some(3.0)), 10.0);
        assert_eq!(clamp_seconds(SoundKind::Music, Some(500.0)), 240.0);
        assert_eq!(clamp_seconds(SoundKind::Ui, None), 2.0);
        assert_eq!(clamp_seconds(SoundKind::Sfx, Some(f32::NAN)), 2.0);
        assert!(sound_prompt(SoundKind::Ui, "a soft click.", "Sci-fi").starts_with("Game user interface sound, short and clean: a soft click. Sci-fi."));
    }

    #[test]
    fn flac_becomes_wav_and_bad_input_is_refused() {
        // A tiny real FLAC: 16-bit mono, 8 samples (made with the reference encoder).
        let flac = include_bytes!("testdata/tiny.flac");
        let wav = flac_to_wav(flac).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "16-bit");
        assert_eq!(wav.len(), 44 + 8 * 2);
        assert_eq!(&wav[44..48], &[0, 0, 0xe8, 0x03], "the samples, little-endian (0, 1000)");
        let info = wav_info(&wav).unwrap();
        assert_eq!((info.sample_rate, info.channels, info.bits), (22050, 1, 16));
        assert!((info.seconds - 8.0 / 22050.0).abs() < 1e-9);
        assert!(flac_to_wav(b"not a flac").is_err());
        assert!(wav_info(b"RIFF....WAVE").is_none(), "no format, no data");
        assert!(wav_info(&wav[..30]).is_none(), "cut short");
    }
}
