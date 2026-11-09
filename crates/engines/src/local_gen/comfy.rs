//! A small ComfyUI client: the graphs we run (a picture with Z-Image Turbo, a 3D shape with
//! Hunyuan3D 2.1), queuing them (`/prompt`), uploading an input picture (`/upload/image`),
//! waiting for the result (`/history`, `/queue`) and fetching it (`/view`).
//!
//! Every request has a timeout, and a wait never hangs silently: when neither the job's history
//! nor the server's queue changes for [`Limits::stall`], the job fails with a plain message
//! ([`Stall`]).

use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use serde_json::{Value, json};
use std::io::Read;
use std::time::{Duration, Instant};

/// The picture model's files (all three must be on the picture server).
pub const IMAGE_UNET: &str = "z_image_turbo_bf16.safetensors";
pub const IMAGE_TEXT_ENCODER: &str = "qwen_3_4b.safetensors";
pub const IMAGE_VAE: &str = "ae.safetensors";
/// The 3D model's checkpoint (on the 3D server).
pub const SHAPE_CHECKPOINT: &str = "hunyuan_3d_v2.1.safetensors";
/// Largest file fetched from a server.
const MAX_BYTES: u64 = 512 << 20;

/// How long to wait.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The longest one job may take.
    pub job: Duration,
    /// Fail when neither the job nor the queue changed for this long.
    pub stall: Duration,
    /// How often to look.
    pub poll: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { job: Duration::from_secs(30 * 60), stall: Duration::from_secs(3 * 60), poll: Duration::from_millis(700) }
    }
}

/// A Z-Image Turbo picture: `prompt`, `width`×`height`, saved under `craft/<prefix>`.
pub fn image_graph(prompt: &str, width: u32, height: u32, seed: u64, prefix: &str) -> Value {
    json!({
        "1": {"class_type": "UNETLoader", "inputs": {"unet_name": IMAGE_UNET, "weight_dtype": "default"}},
        "2": {"class_type": "CLIPLoader", "inputs": {"clip_name": IMAGE_TEXT_ENCODER, "type": "lumina2", "device": "default"}},
        "3": {"class_type": "VAELoader", "inputs": {"vae_name": IMAGE_VAE}},
        "4": {"class_type": "ModelSamplingAuraFlow", "inputs": {"model": ["1", 0], "shift": 3}},
        "5": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": prompt}},
        "6": {"class_type": "ConditioningZeroOut", "inputs": {"conditioning": ["5", 0]}},
        "7": {"class_type": "EmptySD3LatentImage", "inputs": {"width": width, "height": height, "batch_size": 1}},
        "8": {"class_type": "KSampler", "inputs": {
            "model": ["4", 0], "positive": ["5", 0], "negative": ["6", 0], "latent_image": ["7", 0],
            "seed": seed, "steps": 8, "cfg": 1, "sampler_name": "res_multistep", "scheduler": "simple", "denoise": 1
        }},
        "9": {"class_type": "VAEDecode", "inputs": {"samples": ["8", 0], "vae": ["3", 0]}},
        "10": {"class_type": "SaveImage", "inputs": {"images": ["9", 0], "filename_prefix": format!("craft/{prefix}")}},
    })
}

/// The node whose output is the picture in [`image_graph`].
pub const IMAGE_OUTPUT_NODE: &str = "10";

/// A Hunyuan3D 2.1 shape from an uploaded picture (`image`: its name on the server), saved as a
/// GLB under `craft/<prefix>`.
pub fn shape_graph(image: &str, seed: u64, steps: u32, octree: u32, prefix: &str) -> Value {
    json!({
        "1": {"class_type": "ImageOnlyCheckpointLoader", "inputs": {"ckpt_name": SHAPE_CHECKPOINT}},
        "2": {"class_type": "ModelSamplingAuraFlow", "inputs": {"model": ["1", 0], "shift": 1}},
        "3": {"class_type": "LoadImage", "inputs": {"image": image}},
        "4": {"class_type": "CLIPVisionEncode", "inputs": {"clip_vision": ["1", 1], "image": ["3", 0], "crop": "center"}},
        "5": {"class_type": "Hunyuan3Dv2Conditioning", "inputs": {"clip_vision_output": ["4", 0]}},
        "6": {"class_type": "EmptyLatentHunyuan3Dv2", "inputs": {"resolution": 4096, "batch_size": 1}},
        "7": {"class_type": "KSampler", "inputs": {
            "model": ["2", 0], "positive": ["5", 0], "negative": ["5", 1], "latent_image": ["6", 0],
            "seed": seed, "steps": steps, "cfg": 5, "sampler_name": "euler", "scheduler": "normal", "denoise": 1
        }},
        "8": {"class_type": "VAEDecodeHunyuan3D", "inputs": {"samples": ["7", 0], "vae": ["1", 2], "num_chunks": 8000, "octree_resolution": octree}},
        "9": {"class_type": "VoxelToMesh", "inputs": {"voxel": ["8", 0], "algorithm": "surface net", "threshold": 0.6}},
        "10": {"class_type": "SaveGLB", "inputs": {"mesh": ["9", 0], "filename_prefix": format!("craft/{prefix}")}},
    })
}

/// The node whose output is the shape in [`shape_graph`].
pub const SHAPE_OUTPUT_NODE: &str = "10";

/// Notices a wait that has stopped moving: what the job and the queue look like is fed in each
/// time it's checked; when it hasn't changed for the stall limit, the wait fails.
#[derive(Debug)]
pub struct Stall {
    limit: Duration,
    seen: String,
    since: Instant,
}

impl Stall {
    pub fn new(limit: Duration, now: Instant) -> Stall {
        Stall { limit, seen: String::new(), since: now }
    }

    /// Feed what the job and queue look like now; `Err` once nothing has changed for the limit.
    pub fn observe(&mut self, state: &str, now: Instant) -> Result<(), Duration> {
        if state != self.seen {
            self.seen = state.to_string();
            self.since = now;
            return Ok(());
        }
        let quiet = now.saturating_duration_since(self.since);
        if quiet >= self.limit { Err(quiet) } else { Ok(()) }
    }
}

/// A file a job made, as `/history` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputFile {
    pub filename: String,
    pub subfolder: String,
    pub kind: String,
}

/// One ComfyUI server.
#[derive(Debug, Clone)]
pub struct Comfy {
    base: String,
    what: &'static str,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("CraftAnything-dev")
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(Duration::from_secs(5 * 60)))
        .build()
        .new_agent()
}

impl Comfy {
    /// The server at `base` (`http://127.0.0.1:8188`); `what` names it in messages ("picture
    /// server", "3D server").
    pub fn new(base: &str, what: &'static str) -> Comfy {
        Comfy { base: base.trim().trim_end_matches('/').to_string(), what }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn unreachable(&self, e: impl std::fmt::Display) -> ToolError {
        ToolError::new(ErrorCode::EngineMissing, format!("The {} ({}) isn't answering: {e}", self.what, self.base))
            .with_remediation("Start it (Settings → Local generation → Start, or local_generation_start), then try again.")
    }

    fn read(&self, resp: ureq::http::Response<ureq::Body>, path: &str) -> Result<String, ToolError> {
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().map_err(|e| self.unreachable(e))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(ToolError::new(
                ErrorCode::ProgramFailed,
                format!("The {} refused {path} ({status}): {}", self.what, text.chars().take(600).collect::<String>()),
            ))
        }
    }

    pub fn get_json(&self, path: &str, timeout: Duration) -> Result<Value, ToolError> {
        let resp = agent(timeout).get(format!("{}{path}", self.base)).call().map_err(|e| self.unreachable(e))?;
        let text = self.read(resp, path)?;
        serde_json::from_str(&text).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("The {} sent an unreadable answer to {path}: {e}", self.what)))
    }

    /// The server answers at all.
    pub fn reachable(&self) -> bool {
        self.get_json("/system_stats", Duration::from_secs(3)).is_ok()
    }

    /// The model files in one of the server's folders (`checkpoints`, `vae`…).
    pub fn models(&self, folder: &str) -> Result<Vec<String>, ToolError> {
        let v = self.get_json(&format!("/models/{folder}"), Duration::from_secs(5))?;
        Ok(v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default())
    }

    /// Queue a graph; its prompt id.
    pub fn queue(&self, graph: &Value) -> Result<String, ToolError> {
        let body = json!({"prompt": graph, "client_id": "craft-anything"});
        let resp = agent(Duration::from_secs(30))
            .post(format!("{}/prompt", self.base))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| self.unreachable(e))?;
        let text = self.read(resp, "/prompt")?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        v["prompt_id"].as_str().map(str::to_string).ok_or_else(|| {
            ToolError::new(ErrorCode::ProgramFailed, format!("The {} didn't take the job: {}", self.what, text.chars().take(600).collect::<String>()))
        })
    }

    /// Upload a picture as `name`; the name the server keeps it under.
    pub fn upload_image(&self, bytes: &[u8], name: &str) -> Result<String, ToolError> {
        let boundary = format!("craft{:016x}", bytes.len() as u64 ^ 0x5eed_c0de);
        let mut body = Vec::with_capacity(bytes.len() + 512);
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{name}\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"overwrite\"\r\n\r\ntrue\r\n--{boundary}--\r\n").as_bytes());
        let resp = agent(Duration::from_secs(60))
            .post(format!("{}/upload/image", self.base))
            .header("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .send(&body[..])
            .map_err(|e| self.unreachable(e))?;
        let text = self.read(resp, "/upload/image")?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let base = v["name"].as_str().unwrap_or(name).to_string();
        Ok(match v["subfolder"].as_str().filter(|s| !s.is_empty()) {
            Some(sub) => format!("{sub}/{base}"),
            None => base,
        })
    }

    /// Wait for job `id` to finish: its outputs (node id → output). Fails on an error the server
    /// reports, on cancel, past the job limit, or when nothing moves for the stall limit.
    /// `progress` hears how long it has been waiting and where the job is in the queue.
    pub fn wait(&self, id: &str, limits: Limits, cancel: &CancelToken, progress: &dyn Fn(&str)) -> Result<Value, ToolError> {
        let start = Instant::now();
        let mut stall = Stall::new(limits.stall, start);
        loop {
            cancel.check()?;
            let hist = self.get_json(&format!("/history/{id}"), Duration::from_secs(20))?;
            if let Some(entry) = hist.get(id) {
                let status = &entry["status"];
                if status["status_str"].as_str() == Some("error") {
                    return Err(job_failed(self.what, status));
                }
                if status["completed"].as_bool() == Some(true) || status["status_str"].as_str() == Some("success") {
                    return Ok(entry["outputs"].clone());
                }
            }
            let queue = self.get_json("/queue", Duration::from_secs(20)).unwrap_or(Value::Null);
            let place = queue_place(&queue, id);
            let state = format!("{}|{}", hist.get(id).map(|e| e.to_string()).unwrap_or_default(), queue_signature(&queue));
            let now = Instant::now();
            if let Err(quiet) = stall.observe(&state, now) {
                return Err(ToolError::new(
                    ErrorCode::TimedOut,
                    format!("The {} stopped making progress: nothing changed for {} s.", self.what, quiet.as_secs()),
                )
                .with_remediation("Check the server's window or log; restart it if it hangs, then try again."));
            }
            if now.duration_since(start) >= limits.job {
                return Err(ToolError::new(ErrorCode::TimedOut, format!("The {} took longer than {} minutes.", self.what, limits.job.as_secs() / 60)));
            }
            progress(&match place {
                Some(0) => format!("working ({} s)", now.duration_since(start).as_secs()),
                Some(n) => format!("waiting in the server's queue ({n} ahead)"),
                None => format!("waiting ({} s)", now.duration_since(start).as_secs()),
            });
            std::thread::sleep(limits.poll);
        }
    }

    /// Fetch a file a job made.
    pub fn view(&self, f: &OutputFile) -> Result<Vec<u8>, ToolError> {
        let q = format!("/view?filename={}&subfolder={}&type={}", encode(&f.filename), encode(&f.subfolder), encode(&f.kind));
        let resp = agent(Duration::from_secs(120)).get(format!("{}{q}", self.base)).call().map_err(|e| self.unreachable(e))?;
        if !resp.status().is_success() {
            return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The {} couldn't send {} ({}).", self.what, f.filename, resp.status().as_u16())));
        }
        let mut bytes = Vec::new();
        resp.into_body().into_reader().take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| self.unreachable(e))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(ToolError::new(ErrorCode::OverLimit, format!("{} is too large.", f.filename)));
        }
        Ok(bytes)
    }
}

/// The first file of a node's output (`images`, or `3d` for a shape).
pub fn first_output(outputs: &Value, node: &str) -> Option<OutputFile> {
    let out = outputs.get(node)?;
    ["images", "3d", "meshes", "gifs", "audio"].iter().filter_map(|k| out.get(*k).and_then(Value::as_array)).flatten().find_map(|f| {
        Some(OutputFile {
            filename: f.get("filename")?.as_str()?.to_string(),
            subfolder: f.get("subfolder").and_then(Value::as_str).unwrap_or_default().to_string(),
            kind: f.get("type").and_then(Value::as_str).unwrap_or("output").to_string(),
        })
    })
}

fn job_failed(what: &str, status: &Value) -> ToolError {
    let messages: Vec<String> = status["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m[0].as_str() == Some("execution_error"))
        .map(|m| format!("{} ({})", m[1]["exception_message"].as_str().unwrap_or("an error").trim(), m[1]["node_type"].as_str().unwrap_or("?")))
        .collect();
    let why = if messages.is_empty() { "it reported an error".to_string() } else { messages.join("; ") };
    ToolError::new(ErrorCode::ProgramFailed, format!("The {what} couldn't make it: {why}"))
        .with_remediation("A missing model file or too little graphics memory are the usual causes; local_generation_status lists what the server has.")
}

/// Where `id` is in the queue: Some(0) running, Some(n) n jobs ahead of it, None not listed.
fn queue_place(queue: &Value, id: &str) -> Option<usize> {
    let has = |item: &Value| item.get(1).and_then(Value::as_str) == Some(id);
    if queue["queue_running"].as_array().is_some_and(|a| a.iter().any(has)) {
        return Some(0);
    }
    queue["queue_pending"].as_array().and_then(|a| a.iter().position(has)).map(|i| i + 1)
}

/// What the queue looks like, compactly (which jobs run and how many wait).
fn queue_signature(queue: &Value) -> String {
    let ids =
        |k: &str| queue[k].as_array().map(|a| a.iter().filter_map(|i| i.get(1).and_then(Value::as_str)).collect::<Vec<_>>().join(",")).unwrap_or_default();
    format!("{}/{}", ids("queue_running"), ids("queue_pending"))
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graphs_have_the_nodes_the_models_need() {
        let g = image_graph("Game HUD icon: a pickaxe", 512, 768, 42, "job/pick");
        assert_eq!(g["1"]["inputs"]["unet_name"], IMAGE_UNET);
        assert_eq!(g["2"]["inputs"]["type"], "lumina2");
        assert_eq!(g["5"]["inputs"]["text"], "Game HUD icon: a pickaxe");
        assert_eq!((g["7"]["inputs"]["width"].as_u64(), g["7"]["inputs"]["height"].as_u64()), (Some(512), Some(768)));
        assert_eq!(g["8"]["inputs"]["steps"], 8);
        assert_eq!(g["8"]["inputs"]["positive"], json!(["5", 0]));
        assert_eq!(g[IMAGE_OUTPUT_NODE]["class_type"], "SaveImage");
        assert_eq!(g[IMAGE_OUTPUT_NODE]["inputs"]["filename_prefix"], "craft/job/pick");
        let s = shape_graph("ref.png", 7, 30, 256, "job/crate");
        assert_eq!(s["1"]["inputs"]["ckpt_name"], SHAPE_CHECKPOINT);
        assert_eq!(s["3"]["inputs"]["image"], "ref.png");
        assert_eq!(s["8"]["inputs"]["octree_resolution"], 256);
        assert_eq!(s["9"]["inputs"]["algorithm"], "surface net");
        assert_eq!(s[SHAPE_OUTPUT_NODE]["class_type"], "SaveGLB");
        // Every link points at a node that's there.
        for graph in [&g, &s] {
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
    fn a_wait_that_stops_moving_is_a_stall() {
        let t0 = Instant::now();
        let mut s = Stall::new(Duration::from_secs(180), t0);
        assert!(s.observe("a", t0).is_ok());
        assert!(s.observe("a", t0 + Duration::from_secs(179)).is_ok());
        assert!(s.observe("b", t0 + Duration::from_secs(200)).is_ok(), "a change starts the clock again");
        assert!(s.observe("b", t0 + Duration::from_secs(379)).is_ok());
        assert_eq!(s.observe("b", t0 + Duration::from_secs(381)), Err(Duration::from_secs(181)));
    }

    #[test]
    fn outputs_queue_places_and_errors_are_read() {
        let out = json!({"10": {"3d": [{"filename": "a.glb", "subfolder": "craft", "type": "output"}]}});
        assert_eq!(first_output(&out, "10").map(|f| f.filename), Some("a.glb".into()));
        assert!(first_output(&out, "9").is_none());
        let q = json!({"queue_running": [[0, "x", {}]], "queue_pending": [[1, "y", {}], [2, "z", {}]]});
        assert_eq!((queue_place(&q, "x"), queue_place(&q, "z"), queue_place(&q, "w")), (Some(0), Some(2), None));
        let e = job_failed("picture server", &json!({"messages": [["execution_error", {"exception_message": "Out of memory", "node_type": "KSampler"}]]}));
        assert!(e.message.contains("Out of memory (KSampler)"));
        assert_eq!(encode("craft/a b.png"), "craft%2Fa%20b.png");
    }
}
