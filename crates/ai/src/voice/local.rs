//! The local model's side of a voice conversation: it only talks. Each request is plain chat
//! messages (system, user, assistant) with no tools, no functions and no tool choice: the request
//! type has nowhere to put them, and [`request_body`] writes only the fields listed in
//! [`BODY_FIELDS`]. What the model answers is read as data, never run.

use crate::assets_ai::AssetModel;
use crate::config::{AiConfig, is_local_server};
use crate::{BackendKind, Role, http};
use craft_anything_evidence::{ErrorCode, ToolError};
use serde_json::{Value, json};
use std::time::Duration;

/// Who wrote a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    fn as_str(self) -> &'static str {
        match self {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

/// One chat message: text only.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        ChatMessage { role: ChatRole::System, content: text.into() }
    }
    pub fn user(text: impl Into<String>) -> Self {
        ChatMessage { role: ChatRole::User, content: text.into() }
    }
}

/// What the brain asks the local model for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Sort one utterance into a [`super::Intent`] (JSON answer).
    Classify,
    /// One spoken sentence about a running job's progress.
    Narrate,
    /// One or two spoken sentences about a finished job.
    Summarise,
    /// A few plain words for an action Opus asks permission for.
    Describe,
}

/// One request to the local model. There is deliberately no field for tools.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub purpose: Purpose,
    pub messages: Vec<ChatMessage>,
    /// Ask the server for a JSON object (`response_format`).
    pub json: bool,
    pub max_tokens: u32,
}

/// The local model the brain talks through. Implementations must send nothing but the request's
/// messages: no tools. The production one is [`LocalModel`]; tests use stand-ins.
pub trait Converser: Send {
    /// The model's answer text.
    fn chat(&self, req: &ChatRequest) -> Result<String, ToolError>;
    /// Its name, for messages.
    fn name(&self) -> String;
}

/// Every field [`request_body`] may write. Tools, functions and tool choice are not among them.
pub const BODY_FIELDS: &[&str] = &["model", "messages", "stream", "temperature", "max_tokens", "response_format"];

/// The OpenAI chat-completions body for `req`: messages as plain strings, no tools.
pub fn request_body(model: &str, req: &ChatRequest, with_format: bool) -> Value {
    let messages: Vec<Value> = req.messages.iter().map(|m| json!({"role": m.role.as_str(), "content": m.content})).collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "temperature": 0.2,
        "max_tokens": req.max_tokens,
    });
    if req.json
        && with_format
        && let Some(map) = body.as_object_mut()
    {
        map.insert("response_format".into(), json!({"type": "json_object"}));
    }
    body
}

/// How long one voice answer may take (a model loading for its first request included).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);

/// The Assets role's model server on this computer or its network, asked over the OpenAI chat
/// API with no tools.
#[derive(Debug, Clone)]
pub struct LocalModel {
    endpoint: AssetModel,
    timeout: Duration,
}

impl LocalModel {
    /// The model at `endpoint`.
    pub fn new(endpoint: AssetModel) -> Self {
        LocalModel { endpoint, timeout: ANSWER_TIMEOUT }
    }

    /// With another answer deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The Assets role's backend, which must be a model server on this computer or its network
    /// (listening and speaking stay local; only Opus is in the cloud).
    pub fn from_ai_config(ai: &AiConfig) -> Result<Self, ToolError> {
        let entry = ai.backend_for(Role::Assets).ok_or_else(|| ToolError::unavailable("No local model is set up for voice.").with_remediation(LOCAL_FIX))?;
        if entry.config.kind != BackendKind::OpenaiCompatible || !is_local_server(&entry.config) {
            return Err(ToolError::new(
                ErrorCode::BadSettings,
                format!("{} isn't a model server on this computer, so it can't be the voice's local model.", entry.config.name),
            )
            .with_remediation(LOCAL_FIX));
        }
        Ok(LocalModel::new(AssetModel::of(entry)?))
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        self.endpoint.key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect()
    }

    fn post(&self, body: &Value) -> Result<String, ToolError> {
        let agent = http::agent(Duration::from_secs(10), self.timeout, self.timeout);
        let url = format!("{}/chat/completions", self.endpoint.base);
        let mut r = http::post_json(&agent, &self.endpoint.name, &url, &self.headers(), body)?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut r, &mut text)
            .map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("{} stopped answering: {e}", self.endpoint.name)))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("{} sent JSON we couldn't read: {e}", self.endpoint.name)))?;
        let answer = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or_default();
        let answer = strip_thinking(answer);
        if answer.is_empty() {
            return Err(ToolError::new(ErrorCode::GarbledOutput, format!("{} gave an empty answer.", self.endpoint.name)));
        }
        Ok(answer.to_string())
    }
}

const LOCAL_FIX: &str =
    "Start a model server on this computer (llama.cpp, Ollama or LM Studio) and choose it for Assets in Settings → AI; the voice's local model only talks.";

impl Converser for LocalModel {
    /// One answer. A server that refuses `response_format` (some only take a JSON schema) is asked
    /// once more without it; the prompt asks for JSON either way.
    fn chat(&self, req: &ChatRequest) -> Result<String, ToolError> {
        match self.post(&request_body(&self.endpoint.model, req, true)) {
            Err(e) if req.json && e.code != ErrorCode::TimedOut && e.code != ErrorCode::Cancelled => {
                log::debug!("{} refused JSON mode ({}); asking without it", self.endpoint.name, e.message);
                self.post(&request_body(&self.endpoint.model, req, false))
            }
            other => other,
        }
    }

    fn name(&self) -> String {
        self.endpoint.name.clone()
    }
}

/// The answer after any reasoning a thinking model put first (`<think>…</think>`).
pub fn strip_thinking(answer: &str) -> &str {
    answer.rsplit("</think>").next().unwrap_or(answer).trim()
}
