//! Better 3D models: ComfyUI's own TRELLIS.2 chain with Pixal3D conditioning, on the picture
//! server. From one reference picture it takes the background off (BiRefNet), estimates the camera
//! (MoGe-2), makes the shape and its colours (TRELLIS.2 structure, shape, upsample and texture
//! stages), rebuilds the surface, decimates it to the model's face budget, unwraps it and bakes
//! base colour, metallic, roughness, a normal map from the full-detail surface and ambient
//! occlusion. The result is a GLB with those maps plus each map as a PNG; Blender then finishes it
//! ([`super::finish_pbr`]).

use super::comfy::{self, Comfy, Limits};
use super::{PartStatus, Setup, part};
use serde_json::{Value, json};
use std::time::Duration;

/// The model files the chain needs on the server: (folder, file).
pub const MODELS: &[(&str, &str)] = &[
    ("diffusion_models", "pixal3d_int8_convrot.safetensors"),
    ("diffusion_models", "trellis_2_int8_convrot.safetensors"),
    ("vae", "trellis_2_shape_vae_bf16.safetensors"),
    ("vae", "trellis_2_texture_vae_bf16.safetensors"),
    ("clip_vision", "dino_v3_L_naf_fp32.safetensors"),
    ("geometry_estimation", "moge_2_vitl_normal_fp16.safetensors"),
    ("background_removal", "birefnet.safetensors"),
];

/// Nodes that only a ComfyUI with the TRELLIS.2 chain has.
pub const NODES: &[&str] = &[
    "Pixal3DConditioning",
    "Trellis2ShapeStage",
    "Trellis2TextureStage",
    "VaeDecodeTextureTrellis",
    "MoGeInference",
    "RemoveBackground",
    "UnwrapMesh",
    "BakeTextureFromVoxel",
    "BakeNormalMapFromMesh",
    "BakeAmbientOcclusion",
];

/// The node that saves the model (a GLB).
pub const MODEL_OUTPUT_NODE: &str = "40";
/// The nodes that save each map: (node, map name).
pub const MAP_NODES: &[(&str, &str)] = &[("41", "basecolor"), ("42", "metallic"), ("43", "roughness"), ("44", "normal"), ("45", "ao")];

/// A job this long can sit on one node (unwrapping, baking) for minutes without the queue moving:
/// four times the stall limit (12 minutes by default) and a third more time in all (40 minutes).
pub fn limits(base: Limits) -> Limits {
    Limits { job: base.job * 4 / 3, stall: base.stall * 4, poll: base.poll }
}

/// How detailed: the face budget the surface is decimated to, and the texture size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detail {
    pub faces: u32,
    pub texture: u32,
}

/// The graph for one model from an uploaded picture (`image`: its name on the server); files saved
/// under `craft/<prefix>`.
pub fn graph(image: &str, seed: u64, detail: Detail, prefix: &str) -> Value {
    let faces = detail.faces.clamp(500, 1_000_000);
    let tex = detail.texture.clamp(512, 4096);
    let save = |node: &str, map: &str| json!({"class_type": "SaveImage", "inputs": {"images": [node, 0], "filename_prefix": format!("craft/{prefix}_{map}")}});
    let sampler = |model: &str, cond: &str, latent: Value, steps: u32, cfg: f32, scheduler: &str| {
        json!({"class_type": "KSampler", "inputs": {
            "model": [model, 0], "positive": [cond, 0], "negative": [cond, 1], "latent_image": latent,
            "seed": seed, "steps": steps, "cfg": cfg, "sampler_name": "euler", "scheduler": scheduler, "denoise": 1
        }})
    };
    // In parts: one json! this size is past the macro's recursion limit.
    let input = json!({
        // The subject, cropped square on black, and the camera it was seen from.
        "1": {"class_type": "LoadImage", "inputs": {"image": image}},
        "2": {"class_type": "LoadBackgroundRemovalModel", "inputs": {"bg_removal_name": "birefnet.safetensors"}},
        "3": {"class_type": "RemoveBackground", "inputs": {"bg_removal_model": ["2", 0], "image": ["1", 0]}},
        "4": {"class_type": "ImageCropToMask", "inputs": {
            "images": ["1", 0], "masks": ["3", 0], "width": 1024, "height": 1024, "pad_factor": 1.1, "grow_mask": 0, "background": "#000000"
        }},
        "5": {"class_type": "LoadMoGeModel", "inputs": {"model_name": "moge_2_vitl_normal_fp16.safetensors"}},
        "6": {"class_type": "MoGeInference", "inputs": {
            "moge_model": ["5", 0], "image": ["4", 0], "resolution_level": 9, "fov_x_degrees": 0, "batch_size": 4,
            "force_projection": true, "apply_mask": true, "refine_steps": 3
        }},
        "7": {"class_type": "MoGeGeometryToFOV", "inputs": {"moge_geometry": ["6", 0], "axis": "horizontal", "unit": "degrees"}},
        "8": {"class_type": "CLIPVisionLoader", "inputs": {"clip_name": "dino_v3_L_naf_fp32.safetensors"}},
        "9": {"class_type": "Pixal3DConditioning", "inputs": {"clip_vision_model": ["8", 0], "image": ["4", 0], "camera_angle_x": ["7", 0]}},
        "10": {"class_type": "UNETLoader", "inputs": {"unet_name": "pixal3d_int8_convrot.safetensors", "weight_dtype": "default"}},
        "11": {"class_type": "VAELoader", "inputs": {"vae_name": "trellis_2_shape_vae_bf16.safetensors"}},
        "12": {"class_type": "VAELoader", "inputs": {"vae_name": "trellis_2_texture_vae_bf16.safetensors"}},
    });
    let stages = json!({
        // Structure.
        "13": {"class_type": "CFGOverride", "inputs": {"model": ["10", 0], "cfg": 1, "start_percent": 0.667, "end_percent": 1}},
        "14": {"class_type": "RescaleCFG", "inputs": {"model": ["13", 0], "multiplier": 0.7}},
        "15": {"class_type": "ModelSamplingSD3", "inputs": {"model": ["14", 0], "shift": 5}},
        "16": {"class_type": "EmptyTrellis2LatentStructure", "inputs": {"batch_size": 1}},
        "18": {"class_type": "VaeDecodeStructureTrellis2", "inputs": {"samples": ["17", 0], "vae": ["11", 0], "resolution": "32"}},
        // Shape, then upsampled.
        "19": {"class_type": "Trellis2ShapeStage", "inputs": {"positive": ["9", 0], "negative": ["9", 1], "voxel": ["18", 0]}},
        "20": {"class_type": "CFGOverride", "inputs": {"model": ["10", 0], "cfg": 1, "start_percent": 0.769, "end_percent": 1}},
        "21": {"class_type": "RescaleCFG", "inputs": {"model": ["20", 0], "multiplier": 0.5}},
        "23": {"class_type": "Trellis2UpsampleStage", "inputs": {
            "positive": ["19", 0], "negative": ["19", 1], "shape_latent": ["22", 0], "vae": ["11", 0], "target_resolution": 1536
        }},
        "25": {"class_type": "VaeDecodeShapeTrellis", "inputs": {"samples": ["24", 0], "vae": ["11", 0]}},
        // Colours.
        "26": {"class_type": "Trellis2TextureStage", "inputs": {"positive": ["23", 0], "negative": ["23", 1], "shape_latent": ["24", 0]}},
        "28": {"class_type": "VaeDecodeTextureTrellis", "inputs": {"samples": ["27", 0], "vae": ["12", 0], "shape_subdivides": ["25", 1]}},
    });
    let mesh = json!({
        // A clean surface at the budget, unwrapped, with its maps baked.
        "29": {"class_type": "GetMeshInfo", "inputs": {"mesh": ["25", 0]}},
        "30": {"class_type": "RemeshMesh", "inputs": {
            "mesh": ["29", 0], "resolution": 768, "sign_mode": "solid", "sign_mode.fill": 20, "sign_mode.qef": false, "band": 1,
            "project_back": 0, "fix_poles": false, "smooth_iters": 20, "drop_small_components": 0.01, "precluster_max_verts": 20_000_000
        }},
        "31": {"class_type": "DecimateMesh", "inputs": {"mesh": ["30", 0], "target_face_count": faces, "placement_mode": "midpoint"}},
        "32": {"class_type": "MeshSmoothNormals", "inputs": {"mesh": ["31", 0], "crease_angle": 180}},
    });
    let bake = json!({
        "33": {"class_type": "UnwrapMesh", "inputs": {"mesh": ["32", 0], "segmenter": "pec", "resolution": tex, "padding": 1, "weld_distance": 0.0002}},
        "34": {"class_type": "BakeTextureFromVoxel", "inputs": {"mesh": ["33", 0], "voxel_colors": ["28", 0], "reference_mesh": ["25", 0], "texture_size": tex}},
        "35": {"class_type": "BakeNormalMapFromMesh", "inputs": {
            "low_poly": ["33", 0], "high_poly": ["30", 0], "resolution": tex, "cage_distance": 0.05, "ignore_backfaces": true
        }},
        "36": {"class_type": "BakeAmbientOcclusion", "inputs": {
            "low_poly": ["33", 0], "high_poly": ["30", 0], "resolution": tex.min(1024), "samples": 64, "max_distance": 0.71, "strength": 1, "bias": 0.01
        }},
        "37": {"class_type": "ApplyTextureToMesh", "inputs": {
            "mesh": ["33", 0], "base_color": ["34", 0], "metallic": ["34", 1], "roughness": ["34", 2], "occlusion": ["36", 0], "normal_map": ["35", 0]
        }},
        "38": {"class_type": "MeshSmoothNormals", "inputs": {"mesh": ["37", 0], "crease_angle": 180}},
        "40": {"class_type": "SaveGLB", "inputs": {"mesh": ["38", 0], "filename_prefix": format!("craft/{prefix}")}},
    });
    let mut g = Value::Object(Default::default());
    for part in [input, stages, mesh, bake] {
        if let (Value::Object(all), Value::Object(nodes)) = (&mut g, part) {
            all.extend(nodes);
        }
    }
    g["17"] = sampler("15", "9", json!(["16", 0]), 12, 7.5, "normal");
    g["22"] = sampler("21", "19", json!(["19", 2]), 20, 7.5, "normal");
    g["24"] = sampler("21", "23", json!(["23", 2]), 12, 7.5, "simple");
    g["27"] = sampler("10", "26", json!(["26", 2]), 12, 1.0, "normal");
    for ((node, map), (from, slot)) in MAP_NODES.iter().zip([("34", 0), ("34", 1), ("34", 2), ("35", 0), ("36", 0)]) {
        let mut s = save(from, map);
        s["inputs"]["images"] = json!([from, slot]);
        g[*node] = s;
    }
    g
}

/// Whether the picture server can run the chain: answering, with the nodes and the model files.
pub fn status(setup: &Setup) -> PartStatus {
    let server = setup.picture();
    let models = super::server_status(&server, setup.picture_start.as_deref(), MODELS);
    if !models.ready && models.state != "missing_model" {
        return models;
    }
    let missing: Vec<&str> = NODES.iter().copied().filter(|n| !has_node(&server, n)).collect();
    if !missing.is_empty() {
        let also = if models.ready { String::new() } else { format!(" {}.", models.detail) };
        return part(
            false,
            "missing_model",
            format!("Running at {}, but its ComfyUI has no TRELLIS.2 nodes ({}): update ComfyUI.{also}", server.base(), missing.join(", ")),
            models.startable,
        );
    }
    models
}

fn has_node(server: &Comfy, class: &str) -> bool {
    server.get_json(&format!("/object_info/{class}"), Duration::from_secs(10)).is_ok_and(|v| v.get(class).is_some())
}

/// The maps a finished job made: (map name, file).
pub fn maps(outputs: &Value) -> Vec<(&'static str, comfy::OutputFile)> {
    MAP_NODES.iter().filter_map(|(node, map)| comfy::first_output(outputs, node).map(|f| (*map, f))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graph_is_whole_and_uses_the_budget() {
        let g = graph("ref.png", 7, Detail { faces: 12_000, texture: 2048 }, "job/dwarf");
        assert_eq!(g["1"]["inputs"]["image"], "ref.png");
        assert_eq!(g["31"]["inputs"]["target_face_count"], 12_000);
        assert_eq!(g["33"]["inputs"]["resolution"], 2048);
        assert_eq!(g["36"]["inputs"]["resolution"], 1024, "ambient occlusion at most 1024");
        assert_eq!(g["30"]["inputs"]["sign_mode.fill"], 20, "a dynamic combo's inputs are keyed <input>.<sub>");
        assert_eq!(g[MODEL_OUTPUT_NODE]["class_type"], "SaveGLB");
        assert_eq!(g[MODEL_OUTPUT_NODE]["inputs"]["filename_prefix"], "craft/job/dwarf");
        assert_eq!(g["42"]["inputs"]["images"], json!(["34", 1]), "metallic is the bake's second output");
        assert_eq!(g["44"]["inputs"]["filename_prefix"], "craft/job/dwarf_normal");
        for k in ["17", "22", "24", "27"] {
            assert_eq!(g[k]["inputs"]["seed"], 7);
        }
        // Every model file the status checks is the one the graph loads.
        let text = g.to_string();
        for (_, file) in MODELS.iter().filter(|(_, f)| !f.starts_with("trellis_2_int8")) {
            assert!(text.contains(file), "{file}");
        }
        // Every link points at a node that's there, and every node is used or saves something.
        let obj = g.as_object().unwrap();
        let mut used = std::collections::BTreeSet::new();
        for node in obj.values() {
            for v in node["inputs"].as_object().unwrap().values() {
                if let Some([from, _]) = v.as_array().map(Vec::as_slice) {
                    assert!(obj.contains_key(from.as_str().unwrap()), "{v}");
                    used.insert(from.as_str().unwrap().to_string());
                }
            }
        }
        for (id, node) in obj {
            let saves = matches!(node["class_type"].as_str(), Some("SaveGLB" | "SaveImage"));
            assert!(saves || used.contains(id), "node {id} leads nowhere");
        }
        let clamped = graph("r", 1, Detail { faces: 10, texture: 99_999 }, "p");
        assert_eq!((clamped["31"]["inputs"]["target_face_count"].as_u64(), clamped["33"]["inputs"]["resolution"].as_u64()), (Some(500), Some(4096)));
    }

    #[test]
    fn long_steps_get_longer_limits() {
        let l = limits(Limits::default());
        assert_eq!((l.stall, l.job), (Duration::from_secs(12 * 60), Duration::from_secs(40 * 60)));
        let quick = Limits { job: Duration::from_secs(3), stall: Duration::from_millis(600), poll: Duration::from_millis(5) };
        assert_eq!((limits(quick).stall, limits(quick).poll), (Duration::from_millis(2400), Duration::from_millis(5)));
    }

    #[test]
    fn maps_are_read_from_the_outputs() {
        let out = json!({
            "40": {"3d": [{"filename": "a.glb", "subfolder": "craft", "type": "output"}]},
            "41": {"images": [{"filename": "a_basecolor.png", "subfolder": "craft", "type": "output"}]},
            "44": {"images": [{"filename": "a_normal.png", "subfolder": "craft", "type": "output"}]},
        });
        let m = maps(&out);
        assert_eq!(m.iter().map(|(k, f)| (*k, f.filename.as_str())).collect::<Vec<_>>(), [("basecolor", "a_basecolor.png"), ("normal", "a_normal.png")]);
    }
}
