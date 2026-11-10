//! The local model and the asset work it does, from the app: checking and starting a model
//! server (Settings → AI), its start command, rendering model previews and describing assets
//! (the gallery and the rebuild plan), and sorting with the Assets AI. Each is the engine's or
//! the AI layer's tool (`ai_backend_status`, `ai_backend_start`, `project_inventory_render_models`,
//! `asset_describe`, `project_inventory_sort_with_agent`), run on a worker.

use crate::CraftApp;
use crate::i18n::{tr, trf};
use craft_anything_engine::evidence::ToolError;
use serde_json::{Value, json};

impl CraftApp {
    /// Look at the model servers on this computer again (ai_backend_status).
    pub fn check_local_models(&mut self) {
        self.local_models_asked = true;
        self.local_models_busy = true;
        self.act("ai_backend_status", json!({}), tr("Check the local model"));
    }

    /// Start a model server with its start command, and wait until it answers (ai_backend_start).
    pub fn start_local_model(&mut self, name: &str) {
        self.local_models_busy = true;
        self.act("ai_backend_start", json!({"name": name}), tr("Start the local model"));
    }

    /// Set what starts a model server (`None`: started by hand).
    pub fn set_backend_start(&mut self, name: &str, start: Option<String>) {
        let (hub, name) = (self.hub.clone(), name.to_string());
        self.jobs.spawn(tr("Set what starts the local model"), false, move |_| {
            hub.set_start(&name, start.as_deref()).map_err(|e| e.message)?;
            Ok(crate::backends(&hub))
        });
        self.local_models = None;
        self.local_models_asked = false;
    }

    /// Render previews of the open project's models that have none (project_inventory_render_models).
    pub fn render_model_previews(&mut self) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        self.assets.rendering = true;
        self.act("project_inventory_render_models", json!({"project": root}), tr("Render model previews"));
    }

    /// Let the Assets AI check how the shown category's files are grouped into things
    /// (project_inventory_check_grouping).
    pub fn check_grouping(&mut self) {
        let (Some(root), Some(category)) = (self.workspace.as_ref().map(|w| w.root.clone()), self.assets.category) else { return };
        self.act("project_inventory_check_grouping", json!({"project": root, "category": category}), tr("Check the grouping"));
    }

    /// Whether the asset work is done by a model server on this computer that's running (so the
    /// rebuild plan offers to describe first).
    pub fn local_assets_ready(&self) -> bool {
        let Some(name) = self.assets_backend.as_ref() else { return false };
        let local = self.backends.iter().any(|b| &b.name == name && craft_anything_ai::config::is_local_server(b));
        let running = self
            .local_models
            .as_ref()
            .is_some_and(|v| v["servers"].as_array().into_iter().flatten().any(|s| s["name"].as_str() == Some(name.as_str()) && s["state"] == "running"));
        local && running
    }

    /// A local-model tool finished.
    pub(crate) fn local_model_done(&mut self, tool: &str, result: Result<Value, ToolError>) {
        match (tool, result) {
            ("ai_backend_status", Ok(v)) => {
                self.local_models_busy = false;
                self.local_models = Some(v);
            }
            ("ai_backend_start", Ok(v)) => {
                self.local_models_busy = false;
                let started = v["started"].as_bool() == Some(true);
                self.toast(if started { tr("The local model is running.") } else { tr("The local model was already running.") }, false);
                self.check_local_models();
            }
            ("project_inventory_render_models", Ok(v)) => {
                self.assets.rendering = false;
                let (made, failed) = (v["rendered"].as_u64().unwrap_or(0), v["failed"].as_u64().unwrap_or(0));
                let text = if made == 0 && failed == 0 {
                    v["note"].as_str().map(tr).unwrap_or_else(|| tr("Every model has a preview."))
                } else {
                    trf("{made} model previews made, {failed} couldn't be.", &[("made", &made.to_string()), ("failed", &failed.to_string())])
                };
                self.toast(text, failed > 0 && made == 0);
                self.load_assets();
            }
            ("project_inventory_sort_with_agent", Ok(v)) => {
                let added: Vec<String> = v["added"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| Some(format!("{} → {}", r["term"].as_str()?, r["category"].as_str()?)))
                    .collect();
                if added.is_empty() {
                    self.toast(tr("The AI found nothing it was sure about."), false);
                } else {
                    self.toast(trf("Sorted with {n} new terms: {terms}", &[("n", &added.len().to_string()), ("terms", &added.join(", "))]), false);
                }
                self.assets.summary = None;
                self.load_assets();
            }
            ("project_inventory_check_grouping", Ok(v)) => {
                let (moved, merged) = (v["moved"].as_u64().unwrap_or(0), v["merged"].as_u64().unwrap_or(0));
                self.toast(
                    trf("The AI moved {moved} files and merged {merged} things.", &[("moved", &moved.to_string()), ("merged", &merged.to_string())]),
                    false,
                );
                self.assets.summary = None;
                self.load_assets();
            }
            ("asset_describe", Ok(v)) => {
                let n = v["described"].as_u64().unwrap_or(0);
                self.toast(trf("{n} assets described.", &[("n", &n.to_string())]), false);
            }
            (tool, Err(e)) => {
                if tool.starts_with("ai_backend") {
                    self.local_models_busy = false;
                }
                if tool == "project_inventory_render_models" {
                    self.assets.rendering = false;
                }
                self.toast(e.for_people(), true);
            }
            _ => {}
        }
    }
}
