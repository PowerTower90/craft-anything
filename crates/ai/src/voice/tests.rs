//! The voice brain's guarantees, each proved here with stand-ins: a stand-in local model (a
//! closure, or a stand-in OpenAI-compatible server for what goes over the wire), a stand-in
//! executor, and a stand-in "Claude Code" (this test program run again as `voice_opus_child`,
//! speaking Claude Code's stream-json and asking for approval through the real approval policy).
//! Nothing here runs the real Claude Code or reaches the network.

use super::approvals::{ALWAYS_ASKED, prepare_workspace};
use super::*;
use crate::assets_ai::AssetModel;
use crate::cli_agents::adapters::ClaudeCode;
use crate::cli_agents::{AgentAdapter, AgentAdapters, ArgsCx, McpCommand, StreamParser};
use crate::{AgentEvent, BackendConfig, BackendKind, BackendOptions};
use craft_anything_engine::inbox::{Inbox, RequestKind};
use craft_anything_engine::tools::interaction_tools::approve_with;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

// ---- Stand-ins ----

type Asked = Arc<Mutex<Vec<ChatRequest>>>;

/// A local model answering with `answer`, keeping every request.
struct ScriptedModel {
    asked: Asked,
    answer: fn(&ChatRequest) -> Result<String, ToolError>,
}

impl Converser for ScriptedModel {
    fn chat(&self, req: &ChatRequest) -> Result<String, ToolError> {
        self.asked.lock().unwrap().push(req.clone());
        (self.answer)(req)
    }
    fn name(&self) -> String {
        "stand-in".into()
    }
}

/// The words the brain asked about (the classification request's last line).
fn said_in(req: &ChatRequest) -> String {
    let user = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
    user.rsplit("The user just said: ").next().unwrap_or_default().trim_matches('"').to_lowercase()
}

/// Sorts the way a sensible model would, by the words.
fn sensible(req: &ChatRequest) -> Result<String, ToolError> {
    Ok(match req.purpose {
        Purpose::Classify => {
            let said = said_in(req);
            if said.contains("hello") {
                json!({"kind":"chat","reply":"Hello! How can I help?"}).to_string()
            } else if said.contains("what is opus doing") {
                json!({"kind":"question","reply":"Opus is still working on it."}).to_string()
            } else if said.trim_end_matches('.') == "cancel" {
                json!({"kind":"cancel"}).to_string()
            } else if said.contains("make it blue") {
                // A model that wrongly reads this as a yes.
                json!({"kind":"confirm_yes"}).to_string()
            } else {
                json!({"kind":"instruction","reply":"","instruction":format!("Please {said}.")}).to_string()
            }
        }
        Purpose::Narrate => "Opus is looking through the folder.".into(),
        Purpose::Summarise => "Opus opened the folder for you. Nothing else is needed.".into(),
        Purpose::Describe => "push the code to GitHub".into(),
    })
}

fn unreachable_model(_: &ChatRequest) -> Result<String, ToolError> {
    Err(ToolError::unavailable("the stand-in model is off"))
}

/// What the stand-in executor saw and will report.
#[derive(Clone, Default)]
struct ExecLog {
    submits: Arc<Mutex<Vec<JobRequest>>>,
    events: Arc<Mutex<VecDeque<ExecEvent>>>,
    cancelled: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    current: Arc<AtomicU64>,
}

impl ExecLog {
    fn push(&self, e: ExecEvent) {
        self.events.lock().unwrap().push_back(e);
    }
}

struct FakeExec {
    log: ExecLog,
    inbox: Inbox,
}

impl Executor for FakeExec {
    fn submit(&mut self, job: &JobRequest) -> Result<u64, ToolError> {
        if self.log.busy.swap(true, Ordering::SeqCst) {
            return Err(ToolError::invalid("busy"));
        }
        self.log.submits.lock().unwrap().push(job.clone());
        Ok(self.log.current.fetch_add(1, Ordering::SeqCst) + 1)
    }
    fn poll(&mut self) -> Vec<ExecEvent> {
        let events: Vec<ExecEvent> = self.log.events.lock().unwrap().drain(..).collect();
        if events.iter().any(|e| matches!(e, ExecEvent::Finished { .. })) {
            self.log.busy.store(false, Ordering::SeqCst);
        }
        events
    }
    fn control(&self) -> JobControl {
        let log = self.log.clone();
        JobControl::new(move || {
            log.cancelled.store(true, Ordering::SeqCst);
            let job = log.current.load(Ordering::SeqCst);
            log.push(ExecEvent::Finished { job, text: String::new(), error: None, cancelled: true });
        })
    }
    fn is_busy(&self) -> bool {
        self.log.busy.load(Ordering::SeqCst)
    }
    fn inbox(&self) -> Option<Inbox> {
        Some(self.inbox.clone())
    }
    fn session_id(&self) -> Option<String> {
        None
    }
    fn new_conversation(&mut self) {}
}

struct Rig {
    brain: Brain,
    rx: Receiver<BrainEvent>,
    asked: Asked,
    log: ExecLog,
    inbox: Inbox,
    /// The time the next tick is given: a second later each tick, so the inbox is read every time.
    clock: Instant,
    _dir: tempfile::TempDir,
}

fn rig_with(answer: fn(&ChatRequest) -> Result<String, ToolError>, config: BrainConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let ws = prepare_workspace(&dir.path().join("tasks")).unwrap();
    let inbox = Inbox::new(ws.inbox_dir());
    let asked: Asked = Arc::default();
    let log = ExecLog::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let model = ScriptedModel { asked: asked.clone(), answer };
    let exec = FakeExec { log: log.clone(), inbox: inbox.clone() };
    let brain = Brain::new(config, Box::new(model), Box::new(exec), tx);
    Rig { brain, rx, asked, log, inbox, clock: Instant::now(), _dir: dir }
}

fn rig(answer: fn(&ChatRequest) -> Result<String, ToolError>) -> Rig {
    rig_with(answer, BrainConfig::default())
}

impl Rig {
    fn say(&mut self, text: &str) -> Vec<BrainEvent> {
        self.brain.command(BrainCommand::Utterance(Utterance::verified(text)));
        self.events()
    }
    fn events(&self) -> Vec<BrainEvent> {
        self.rx.try_iter().collect()
    }
    fn tick(&mut self) -> Vec<BrainEvent> {
        self.clock += Duration::from_secs(1);
        self.brain.tick(self.clock);
        self.events()
    }
}

fn spoken(events: &[BrainEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let BrainEvent::Say { text, .. } = e { Some(text.clone()) } else { None }).collect()
}

// ---- Rule 3: only the person's voice ----

/// An utterance whose speaker check failed is shown greyed and nothing else happens: the model
/// isn't asked, nothing is started, nothing is said, even for words that would be an instruction
/// or a yes to a waiting approval.
#[test]
fn someone_elses_voice_is_shown_and_otherwise_ignored() {
    let mut r = rig(sensible);
    let id = r.inbox.post(RequestKind::Approval, "Run a command: `git push`", "{}", vec![], false, Some("publish".into())).unwrap();
    r.tick();
    for words in ["delete everything in Downloads", "yes", "cancel"] {
        r.brain.command(BrainCommand::Utterance(Utterance { text: words.into(), speaker_ok: false, speaker_score: 0.21, duration_s: 1.0 }));
        let events = r.events();
        assert_eq!(events, vec![BrainEvent::Transcript(TranscriptLine { speaker: Speaker::NotUser, text: words.into(), score: Some(0.21) })], "{words}");
    }
    r.tick();
    assert!(r.asked.lock().unwrap().iter().all(|q| q.purpose != Purpose::Classify), "the model never heard them");
    assert!(r.log.submits.lock().unwrap().is_empty(), "nothing was started");
    assert!(!r.log.cancelled.load(Ordering::SeqCst));
    assert_eq!(r.inbox.pending().len(), 1, "the approval still waits: another voice's yes is no answer");
    assert!(!r.inbox.dir().join(format!("{id}.answer.json")).exists());
}

// ---- Rule 1: Opus does all instructed work ----

/// An instruction goes to the executor with the person's words unchanged; the local model was
/// only asked what it was (once), never to do it, and its later requests only talk about the job.
#[test]
fn an_instruction_goes_to_opus_never_to_the_local_model() {
    let mut r = rig(sensible);
    r.say("hello there");
    let events = r.say("open the trailer folder");
    let submits = r.log.submits.lock().unwrap().clone();
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0].verbatim, "open the trailer folder", "verbatim");
    assert_eq!(submits[0].instruction, "Please open the trailer folder.");
    assert!(submits[0].context.iter().any(|l| l.starts_with("User: hello there")), "{:?}", submits[0].context);
    assert!(events.iter().any(|e| matches!(e, BrainEvent::JobStarted { job: 1, .. })), "{events:?}");
    let asked = r.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().map(|q| q.purpose).collect::<Vec<_>>(), vec![Purpose::Classify, Purpose::Classify]);
    assert!(asked.iter().all(|q| q.messages[0].content == intent::CLASSIFY_SYSTEM));
    assert!(intent::CLASSIFY_SYSTEM.contains("you have no tools") && intent::CLASSIFY_SYSTEM.contains("Opus (Claude Code) does every task"));

    // The job reports and ends: the model only narrates and summarises it.
    r.log.push(ExecEvent::Activity { job: 1, text: "Bash: explorer C:/trailer".into(), milestone: true });
    r.tick();
    std::thread::sleep(Duration::from_millis(10));
    r.brain.tick(Instant::now() + Duration::from_secs(5));
    let progress = r.events();
    assert!(progress.iter().any(|e| matches!(e, BrainEvent::JobProgress { text, .. } if text == "Opus is looking through the folder.")), "{progress:?}");
    r.log.push(ExecEvent::Text { job: 1, text: "I opened C:/trailer in Explorer.".into() });
    r.log.push(ExecEvent::Finished { job: 1, text: "I opened C:/trailer in Explorer.".into(), error: None, cancelled: false });
    let done = r.tick();
    assert!(
        done.iter()
            .any(|e| matches!(e, BrainEvent::JobDone { summary, text, .. } if summary.starts_with("Opus opened the folder") && text.contains("Explorer"))),
        "{done:?}"
    );
    assert!(done.iter().any(|e| matches!(e, BrainEvent::Transcript(TranscriptLine { speaker: Speaker::Opus, .. }))), "the full answer is kept");
    let purposes: Vec<Purpose> = r.asked.lock().unwrap().iter().map(|q| q.purpose).collect();
    assert_eq!(purposes, vec![Purpose::Classify, Purpose::Classify, Purpose::Narrate, Purpose::Summarise]);
}

/// With the local model unreachable, an action request still goes to Opus (never handled
/// locally), and chat gets a plain "didn't catch that".
#[test]
fn without_the_local_model_instructions_still_go_to_opus() {
    let mut r = rig(unreachable_model);
    r.say("please render the trailer again");
    assert_eq!(r.log.submits.lock().unwrap()[0].verbatim, "please render the trailer again");
    let said = spoken(&r.say("what a lovely day"));
    assert!(said.contains(&intent::DIDNT_CATCH.to_string()), "{said:?}");
    assert_eq!(r.log.submits.lock().unwrap().len(), 1);
}

/// A question about the job is answered from its status, read-only: the model is given the
/// status and nothing is started or stopped.
#[test]
fn a_status_question_is_answered_from_the_jobs_status() {
    let mut r = rig(sensible);
    r.say("build the trailer cut");
    r.log.push(ExecEvent::Activity { job: 1, text: "Bash: ffmpeg -i a.mp4".into(), milestone: false });
    r.tick();
    let said = spoken(&r.say("what is opus doing right now?"));
    assert_eq!(said, vec!["Opus is still working on it."]);
    let asked = r.asked.lock().unwrap().clone();
    let last = asked.last().unwrap().messages.last().unwrap().content.clone();
    assert!(last.contains("Opus has been working for") && last.contains("Please build the trailer cut.") && last.contains("ffmpeg"), "{last}");
    assert_eq!(r.log.submits.lock().unwrap().len(), 1, "nothing new was started");
    assert!(!r.log.cancelled.load(Ordering::SeqCst));
    let status = r.brain.status();
    assert_eq!(status.running.as_ref().map(|j| j.recent.clone()), Some(vec!["Bash: ffmpeg -i a.mp4".to_string()]));
}

/// A second instruction while a job runs waits for it, then starts.
#[test]
fn an_instruction_during_a_job_waits_its_turn() {
    let mut r = rig(sensible);
    r.say("open the trailer folder");
    let said = spoken(&r.say("then play the newest clip"));
    assert!(said[0].contains("I'll pass that on"), "{said:?}");
    assert_eq!(r.brain.status().queued, vec!["Please then play the newest clip."]);
    r.log.push(ExecEvent::Finished { job: 1, text: "Done.".into(), error: None, cancelled: false });
    let events = r.tick();
    assert!(events.iter().any(|e| matches!(e, BrainEvent::JobStarted { job: 2, .. })), "{events:?}");
    assert_eq!(r.log.submits.lock().unwrap()[1].verbatim, "then play the newest clip");
}

// ---- Rule 2: the local model only converses, with no tools ----

/// A stand-in OpenAI-compatible server: answers with `answer`, keeps every request body; with
/// `refuse_format`, refuses `response_format` the way some servers do.
fn stand_in_server(answer: fn(&Value) -> String, refuse_format: bool) -> (String, Arc<Mutex<Vec<Value>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut stream) = stream else { continue };
            let s = s.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    return;
                }
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                s.lock().unwrap().push(req.clone());
                let (status, out) = if refuse_format && req.get("response_format").is_some() {
                    ("400 Bad Request", json!({"error": {"message": "response_format.type must be json_schema"}}))
                } else {
                    ("200 OK", json!({"choices": [{"message": {"role": "assistant", "content": answer(&req)}, "finish_reason": "stop"}]}))
                };
                let out = out.to_string();
                let _ =
                    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
            });
        }
    });
    (base, seen)
}

fn wire_answer(req: &Value) -> String {
    let system = req["messages"][0]["content"].as_str().unwrap_or_default();
    let user = req["messages"][1]["content"].as_str().unwrap_or_default().to_lowercase();
    if system == intent::CLASSIFY_SYSTEM {
        let said = user.rsplit("the user just said: ").next().unwrap_or_default();
        if said.contains("hello") {
            return json!({"kind":"chat","reply":"Hi!"}).to_string();
        }
        if said.contains("what is") {
            return format!("<think>status</think>```json\n{}\n```", json!({"kind":"question","reply":"It's busy."}));
        }
        return json!({"kind":"instruction","instruction":"Push the code."}).to_string();
    }
    if system == intent::DESCRIBE_SYSTEM {
        return "push the code to GitHub".into();
    }
    "Opus is working.".into()
}

fn local_model(base: &str) -> LocalModel {
    LocalModel::new(AssetModel { name: "Local".into(), base: base.to_string(), key: None, model: "local-chat".into() })
}

/// Over the wire, every request to the local model is plain chat messages with only the fields
/// in `BODY_FIELDS`: never tools, functions or a tool choice, whatever the brain is doing
/// (sorting words, describing an approval, narrating, summarising).
#[test]
fn the_local_model_is_never_given_tools() {
    for refuse_format in [false, true] {
        let (base, seen) = stand_in_server(wire_answer, refuse_format);
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(prepare_workspace(&dir.path().join("tasks")).unwrap().inbox_dir());
        let log = ExecLog::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let config = BrainConfig { narrate_every: Duration::ZERO, ..BrainConfig::default() };
        let mut brain = Brain::new(config, Box::new(local_model(&base)), Box::new(FakeExec { log: log.clone(), inbox: inbox.clone() }), tx);
        brain.command(BrainCommand::Utterance(Utterance::verified("hello")));
        brain.command(BrainCommand::Utterance(Utterance::verified("push my code to the server")));
        inbox.post(RequestKind::Approval, "Run a command: `git push`", r#"{"command":"git push"}"#, vec![], false, Some("publish".into())).unwrap();
        brain.tick(Instant::now());
        brain.command(BrainCommand::ApproveNo(inbox.pending()[0].id.clone()));
        brain.command(BrainCommand::Utterance(Utterance::verified("what is it doing")));
        log.push(ExecEvent::Activity { job: 1, text: "Bash: git status".into(), milestone: true });
        brain.tick(Instant::now() + Duration::from_secs(10));
        log.push(ExecEvent::Finished { job: 1, text: "Pushed.".into(), error: None, cancelled: false });
        brain.tick(Instant::now() + Duration::from_secs(20));
        let events: Vec<BrainEvent> = rx.try_iter().collect();
        let said = spoken(&events);
        assert!(said.contains(&"Hi!".to_string()) && said.contains(&"It's busy.".to_string()), "{refuse_format}: {said:?}");
        assert!(said.iter().any(|s| s.starts_with("Opus wants to push the code to GitHub.")), "{said:?}");
        assert_eq!(log.submits.lock().unwrap()[0].verbatim, "push my code to the server");

        let seen = seen.lock().unwrap().clone();
        let systems: Vec<&str> = seen.iter().map(|b| b["messages"][0]["content"].as_str().unwrap_or_default()).collect();
        for s in [intent::CLASSIFY_SYSTEM, intent::DESCRIBE_SYSTEM, intent::NARRATE_SYSTEM, intent::SUMMARISE_SYSTEM] {
            assert!(systems.contains(&s), "{refuse_format}: every kind of request was made");
        }
        for body in &seen {
            let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
            assert!(keys.iter().all(|k| local::BODY_FIELDS.contains(&k.as_str())), "{keys:?}");
            for banned in ["tools", "tool_choice", "functions", "function_call", "parallel_tool_calls"] {
                assert!(body.get(banned).is_none(), "{banned} in {body}");
            }
            assert!(body["messages"].as_array().unwrap().iter().all(|m| m["content"].is_string()), "plain text messages only");
        }
        if refuse_format {
            let classify: Vec<&Value> = seen.iter().filter(|b| b["messages"][0]["content"] == intent::CLASSIFY_SYSTEM).collect();
            assert!(
                classify.iter().any(|b| b.get("response_format").is_some()) && classify.iter().any(|b| b.get("response_format").is_none()),
                "asked again without JSON mode"
            );
        }
    }
}

/// The voice's local model must be a model server on this computer or its network.
#[test]
fn the_voice_model_must_be_local() {
    let entry = |kind, base: &str| crate::BackendEntry {
        config: BackendConfig { name: "M".into(), kind, model: "m".into(), base_url: Some(base.into()), ssh_host: None, enabled: true },
        options: BackendOptions::default(),
    };
    let ai = |e| crate::AiConfig { backends: vec![e], roles: [("assets".to_string(), "M".to_string())].into(), ..Default::default() };
    assert!(LocalModel::from_ai_config(&ai(entry(BackendKind::OpenaiCompatible, "http://127.0.0.1:11434/v1"))).is_ok());
    let cloud = LocalModel::from_ai_config(&ai(entry(BackendKind::Openai, "https://api.openai.com/v1"))).unwrap_err();
    assert_eq!(cloud.code, craft_anything_evidence::ErrorCode::BadSettings);
    assert!(LocalModel::from_ai_config(&crate::AiConfig::default()).is_err());
}

// ---- Rule 4: risky actions confirmed by voice ----

fn needed(events: &[BrainEvent]) -> Vec<(String, bool, String)> {
    events
        .iter()
        .filter_map(|e| if let BrainEvent::ApprovalNeeded { id, outward, text, .. } = e { Some((id.clone(), *outward, text.clone())) } else { None })
        .collect()
}

fn answer_of(inbox: &Inbox, id: &str) -> Option<craft_anything_engine::inbox::Answer> {
    serde_json::from_slice(&std::fs::read(inbox.dir().join(format!("{id}.answer.json"))).ok()?).ok()
}

/// An approval nobody answers is a no after the timeout (two minutes by default).
#[test]
fn an_unanswered_approval_is_a_no() {
    assert_eq!(BrainConfig::default().approval_timeout, Duration::from_secs(120));
    let mut r = rig(sensible);
    let id = r.inbox.post(RequestKind::Approval, "Run a command: `git push`", r#"{"command":"git push"}"#, vec![], false, Some("publish".into())).unwrap();
    let start = Instant::now();
    r.brain.tick(start);
    let asked = needed(&r.events());
    assert_eq!(asked.len(), 1);
    assert!(asked[0].1, "outward");
    r.brain.tick(start + Duration::from_secs(119));
    assert!(answer_of(&r.inbox, &id).is_none(), "not yet");
    r.brain.tick(start + Duration::from_secs(120));
    let events = r.events();
    assert!(events.contains(&BrainEvent::ApprovalResolved { id: id.clone(), approved: false, how: Resolution::TimedOut }), "{events:?}");
    assert!(spoken(&events).iter().any(|s| s.contains("told Opus no")));
    assert_eq!(answer_of(&r.inbox, &id).and_then(|a| a.allow), Some(false));
}

/// Only the person's own clear yes approves: a model that reads other words as a yes doesn't, a
/// spoken "yes" does, and the on-screen buttons answer the one they're on.
#[test]
fn only_a_clear_spoken_yes_or_the_button_approves() {
    let mut r = rig(sensible);
    let first = r.inbox.post(RequestKind::Approval, "Run a command: `git push`", "{}", vec![], false, Some("publish".into())).unwrap();
    r.tick();
    let said = spoken(&r.say("make it blue"));
    assert_eq!(said, vec!["Please say yes or no."]);
    assert!(answer_of(&r.inbox, &first).is_none());
    let events = r.say("Yes.");
    assert!(events.contains(&BrainEvent::ApprovalResolved { id: first.clone(), approved: true, how: Resolution::Voice }), "{events:?}");
    assert_eq!(answer_of(&r.inbox, &first).and_then(|a| a.allow), Some(true));
    assert!(r.asked.lock().unwrap().iter().all(|q| !said_in(q).contains("yes")), "a plain yes needs no model");

    let second = r.inbox.post(RequestKind::Approval, "Change a file outside the project: C:/x.txt", "{}", vec![], false, None).unwrap();
    r.tick();
    r.brain.command(BrainCommand::ApproveNo(second.clone()));
    assert!(r.events().contains(&BrainEvent::ApprovalResolved { id: second.clone(), approved: false, how: Resolution::Screen }));
    assert_eq!(answer_of(&r.inbox, &second).and_then(|a| a.allow), Some(false));
}

/// A question Opus asks is read out, and the next words answer it.
#[test]
fn opus_questions_are_answered_by_voice() {
    let mut r = rig(sensible);
    let id = r.inbox.post(RequestKind::Question, "Which clip should go first?", "", vec!["Intro".into(), "Battle".into()], true, None).unwrap();
    let events = r.tick();
    assert!(spoken(&events).iter().any(|s| s == "Opus asks: Which clip should go first? The choices are: Intro or Battle."), "{events:?}");
    r.say("battle");
    let a = answer_of(&r.inbox, &id).unwrap();
    assert_eq!(a.choice.as_deref(), Some("Battle"));
    assert!(r.log.submits.lock().unwrap().is_empty(), "an answer isn't an instruction");
}

/// The voice project makes the real approval policy ask about outward actions even in Auto mode
/// (which would otherwise refuse them unasked), and the brain reads them out.
#[test]
fn outward_actions_are_asked_even_in_auto_mode() {
    let dir = tempfile::tempdir().unwrap();
    let outside = if cfg!(windows) { "C:/Users/someone/Documents/old-cuts" } else { "/home/someone/Documents/old-cuts" };
    // A plain project in Auto mode refuses them without asking anyone.
    let plain = craft_anything_engine::workspace::Workspace::create(dir.path(), "plain", "r", craft_anything_engine::workspace::TargetRef::None).unwrap();
    let push = json!({"command": "git push origin main"});
    let verdict = approve_with(Some(Inbox::new(plain.inbox_dir())), true, false, "Bash", &push, &|| false, &mut || {}).unwrap();
    assert!(verdict.is_err(), "Auto mode alone refuses unasked");
    assert!(ALWAYS_ASKED.len() >= 3);

    let mut r = rig_with(sensible, BrainConfig { approval_timeout: Duration::from_secs(60), ..BrainConfig::default() });
    for (input, yes) in [(push, true), (json!({"command": format!("rm -rf {outside}")}), false)] {
        let inbox = r.inbox.clone();
        let asker = std::thread::spawn(move || approve_with(Some(inbox), true, false, "Bash", &input, &|| false, &mut || {}).unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        let (id, outward) = loop {
            assert!(Instant::now() < deadline, "the policy never asked");
            if let Some((id, outward, _)) = needed(&r.tick()).pop() {
                break (id, outward);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(outward);
        r.say(if yes { "yes" } else { "no" });
        let verdict = asker.join().unwrap();
        assert_eq!(verdict.is_ok(), yes, "{id}: {verdict:?}");
    }
}

// ---- Cancel ----

/// "Cancel" stops the job at once through its control, answers no to what waited and drops the
/// queue.
#[test]
fn cancel_stops_the_job_and_answers_no_to_what_waited() {
    let mut r = rig(sensible);
    r.say("open the trailer folder");
    r.say("and then post it");
    let id = r.inbox.post(RequestKind::Approval, "Run a command: `rm -r C:/x`", "{}", vec![], false, Some("delete_outside".into())).unwrap();
    r.tick();
    let events = r.say("cancel");
    assert!(r.log.cancelled.load(Ordering::SeqCst), "the job's control was used");
    assert!(events.contains(&BrainEvent::ApprovalResolved { id: id.clone(), approved: false, how: Resolution::Cancelled }), "{events:?}");
    assert_eq!(answer_of(&r.inbox, &id).and_then(|a| a.allow), Some(false));
    let events = r.tick();
    assert!(events.iter().any(|e| matches!(e, BrainEvent::JobFailed { cancelled: true, .. })), "{events:?}");
    assert!(r.brain.status().queued.is_empty() && r.brain.status().running.is_none());
    assert_eq!(r.log.submits.lock().unwrap().len(), 1, "the queued one never started");
    assert_eq!(spoken(&r.say("cancel")), vec!["Nothing is running."], "nothing left to stop");
}

// ---- The Opus executor, with a stand-in Claude Code ----

const CHILD_ENV: &str = "CRAFT_ANYTHING_TEST_VOICE_OPUS_CHILD";

/// The stand-in Claude Code: this test program run again with only this test, as
/// `CHILD_ENV` says. It speaks Claude Code's stream-json: `done` works and answers; `approve`
/// asks to `git push` through the real approval policy and the voice project's inbox, as our
/// approval tool does, and says what it was told; `hang` holds a lock file and waits. Its first
/// lines echo what it was started with. Run as an ordinary test, it does nothing.
#[test]
fn voice_opus_child() {
    let Some(mode) = std::env::var_os(CHILD_ENV) else { return };
    let arg = |k: &str| std::env::args().find_map(|a| a.strip_prefix(&format!("{k}=")).map(str::to_string));
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let emit = |v: Value| {
        // On a line of its own: libtest has already written "test … ... " without a line break.
        println!("\n{v}");
        std::io::stdout().flush().unwrap();
    };
    let text = |t: &str| json!({"type":"assistant","session_id":"ses_voice","message":{"content":[{"type":"text","text":format!("{t}\n")}]}});
    let tool = |id: &str, command: &str| json!({"type":"assistant","session_id":"ses_voice","message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":command}}]}});
    let result =
        json!({"type":"result","subtype":"success","is_error":false,"session_id":"ses_voice","result":"ok","usage":{"input_tokens":3,"output_tokens":4}});
    emit(json!({"type":"system","subtype":"init","session_id":"ses_voice","mcp_servers":[{"name":"craft-anything","status":"connected"}]}));
    emit(text(&format!(
        "model={} builder={} resume={}",
        arg("model").unwrap_or_default(),
        arg("builder").unwrap_or_default(),
        arg("resume").unwrap_or_default()
    )));
    emit(text(&format!("got: {}", prompt.lines().next().unwrap_or_default())));
    match mode.to_string_lossy().as_ref() {
        "done" => {
            emit(tool("t1", "explorer ."));
            emit(json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}));
            emit(text("I opened the folder."));
            emit(result);
        }
        "approve" => {
            let inbox = Inbox::new(arg("inbox").unwrap());
            let input = json!({"command": "git push origin main"});
            emit(tool("t1", "git push origin main"));
            let verdict = approve_with(Some(inbox), true, false, "Bash", &input, &|| false, &mut || {}).unwrap();
            let said = match verdict {
                Ok(()) => "approved: pushed".to_string(),
                Err(why) => format!("denied: {why}"),
            };
            emit(text(&said));
            emit(result);
        }
        "hang" => {
            let lock = std::fs::File::create(arg("lock").unwrap()).unwrap();
            lock.lock().unwrap();
            emit(text("started"));
            std::thread::sleep(Duration::from_secs(60));
            drop(lock);
        }
        other => panic!("unknown stand-in mode {other}"),
    }
}

/// Runs `voice_opus_child` the way the Claude Code adapter runs Claude Code: prompt on stdin,
/// stream-json parsed by Claude Code's own parser, sessions resumed.
struct StandInClaude {
    mode: &'static str,
    lock: Option<PathBuf>,
}

impl AgentAdapter for StandInClaude {
    fn ids(&self) -> &'static [&'static str] {
        &["test-voice-opus"]
    }
    fn args(&self, cx: &ArgsCx) -> Vec<String> {
        // Extra words are test-name filters that match nothing; the child reads them back.
        let mut a: Vec<String> = ["--exact", "--test-threads=1", "--nocapture", "voice::tests::voice_opus_child"].map(String::from).to_vec();
        if let Some(i) = cx.mcp.args.iter().position(|x| x == "--inbox") {
            a.push(format!("inbox={}", cx.mcp.args[i + 1]));
        }
        a.push(format!("model={}", cx.options.cli_model.clone().unwrap_or_default()));
        a.push(format!("builder={}", cx.builder));
        if let Some(r) = cx.resume {
            a.push(format!("resume={r}"));
        }
        if let Some(l) = &self.lock {
            a.push(format!("lock={}", l.display()));
        }
        a
    }
    fn streams_json(&self) -> bool {
        true
    }
    fn resumes(&self) -> bool {
        true
    }
    fn prompt_on_stdin(&self) -> bool {
        true
    }
    fn mcp_env(&self, _mcp: &McpCommand) -> Result<Option<(&'static str, String)>, ToolError> {
        Ok(Some((CHILD_ENV, self.mode.to_string())))
    }
    fn parse(&self, p: &mut StreamParser, v: &Value) -> Vec<AgentEvent> {
        ClaudeCode::default().parse(p, v)
    }
}

fn opus(mode: &'static str, lock: Option<PathBuf>, dir: &Path) -> OpusExecutor {
    let config = BackendConfig {
        name: "Claude Code".into(),
        kind: BackendKind::AgentCli,
        model: "test-voice-opus".into(),
        base_url: None,
        ssh_host: None,
        enabled: true,
    };
    let options = BackendOptions { command: Some(std::env::current_exe().unwrap().display().to_string()), ..Default::default() };
    let mut adapters = AgentAdapters::built_in();
    adapters.register(Arc::new(StandInClaude { mode, lock }));
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let setup = OpusSetup { task_folder: Some(work), state_dir: dir.join("voice"), deep: false, resume: None };
    OpusExecutor::new(config, options, setup, adapters).unwrap()
}

/// Ticks the brain until `done` holds for what it said (at most `most`).
fn run_until(brain: &mut Brain, rx: &Receiver<BrainEvent>, most: Duration, done: impl Fn(&[BrainEvent]) -> bool) -> Vec<BrainEvent> {
    let deadline = Instant::now() + most;
    let mut all = Vec::new();
    while !done(&all) {
        assert!(Instant::now() < deadline, "timed out: {all:#?}");
        brain.tick(Instant::now());
        all.extend(rx.try_iter());
        std::thread::sleep(Duration::from_millis(20));
    }
    all
}

fn finished(events: &[BrainEvent]) -> bool {
    events.iter().any(|e| matches!(e, BrainEvent::JobDone { .. } | BrainEvent::JobFailed { .. }))
}

fn job_text(events: &[BrainEvent]) -> String {
    events.iter().find_map(|e| if let BrainEvent::JobDone { text, .. } = e { Some(text.clone()) } else { None }).unwrap_or_default()
}

/// A spoken instruction runs as a Claude Code builder turn on Opus 5.5 with the person's words
/// first on its stdin; the next instruction resumes the same session.
#[test]
fn opus_runs_the_instruction_and_resumes_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let exec = opus("done", None, dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut brain = Brain::new(BrainConfig::default(), Box::new(ScriptedModel { asked: Arc::default(), answer: sensible }), Box::new(exec), tx);
    brain.command(BrainCommand::Utterance(Utterance::verified("open the trailer folder")));
    let first = run_until(&mut brain, &rx, Duration::from_secs(60), finished);
    let text = job_text(&first);
    assert!(text.contains("model=claude-opus-5-5 builder=true resume=\n"), "Opus 5.5, as a builder, a new session: {text}");
    assert!(text.contains("got: open the trailer folder"), "verbatim first: {text}");
    assert!(text.contains("I opened the folder."), "{text}");
    assert!(first.iter().any(|e| matches!(e, BrainEvent::JobDone { summary, .. } if summary.starts_with("Opus opened the folder"))), "{first:#?}");
    assert_eq!(brain.status().session.as_deref(), Some("ses_voice"));

    brain.command(BrainCommand::Utterance(Utterance::verified("now play the newest clip")));
    let second = run_until(&mut brain, &rx, Duration::from_secs(60), finished);
    assert!(job_text(&second).contains("resume=ses_voice"), "the standing session: {}", job_text(&second));
}

/// Opus's permission request (through the real policy, in Auto mode) is read out as outward;
/// a spoken yes lets it go ahead, and with no answer it's a no after the timeout.
#[test]
fn opus_asks_by_voice_and_silence_is_a_no() {
    for (answer, want) in [(Some("yes, go ahead"), "approved: pushed"), (None, "denied: The person said no: no answer came in time")] {
        let dir = tempfile::tempdir().unwrap();
        let exec = opus("approve", None, dir.path());
        let (tx, rx) = std::sync::mpsc::channel();
        let config = BrainConfig { approval_timeout: Duration::from_millis(500), ..BrainConfig::default() };
        let mut brain = Brain::new(config, Box::new(ScriptedModel { asked: Arc::default(), answer: sensible }), Box::new(exec), tx);
        brain.command(BrainCommand::Utterance(Utterance::verified("push my code")));
        let asked = run_until(&mut brain, &rx, Duration::from_secs(60), |e| !needed(e).is_empty());
        let (id, outward, text) = needed(&asked).remove(0);
        assert!(outward, "{text}");
        assert!(text.starts_with("Opus wants to push the code to GitHub.") && text.contains("git push origin main") && text.ends_with("Yes or no?"), "{text}");
        if let Some(a) = answer {
            brain.command(BrainCommand::Utterance(Utterance::verified(a)));
        }
        let rest = run_until(&mut brain, &rx, Duration::from_secs(60), finished);
        let how = if answer.is_some() { Resolution::Voice } else { Resolution::TimedOut };
        let all: Vec<BrainEvent> = asked.into_iter().chain(rest).collect();
        assert!(all.contains(&BrainEvent::ApprovalResolved { id, approved: answer.is_some(), how }), "{all:#?}");
        assert!(job_text(&all).contains(want), "{}", job_text(&all));
    }
}

/// "Cancel" ends the running Claude Code's whole process: the stand-in's lock is released.
#[test]
fn a_spoken_cancel_kills_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("child.lock");
    let exec = opus("hang", Some(lock.clone()), dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let mut brain = Brain::new(BrainConfig::default(), Box::new(ScriptedModel { asked: Arc::default(), answer: sensible }), Box::new(exec), tx);
    brain.command(BrainCommand::Utterance(Utterance::verified("render the whole trailer")));
    // Wait until the child holds its lock.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "the stand-in never started");
        brain.tick(Instant::now());
        if std::fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_err()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let began = Instant::now();
    brain.command(BrainCommand::Utterance(Utterance::verified("Cancel!")));
    let events = run_until(&mut brain, &rx, Duration::from_secs(30), finished);
    assert!(events.iter().any(|e| matches!(e, BrainEvent::JobFailed { cancelled: true, .. })), "{events:#?}");
    assert!(began.elapsed() < Duration::from_secs(20), "stopped promptly: {:?}", began.elapsed());
    let free = Instant::now() + Duration::from_secs(10);
    while std::fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_err()) {
        assert!(Instant::now() < free, "the stand-in Claude Code is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The real Claude Code is started as a builder on Opus 5.5 with our approval tool as its
/// permission prompt, in the task folder, resuming the standing session.
#[test]
fn claude_code_is_asked_for_opus_with_the_approval_tool() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let config =
        BackendConfig { name: "Claude Code".into(), kind: BackendKind::AgentCli, model: "claude-code".into(), base_url: None, ssh_host: None, enabled: true };
    let claude_dir = dir.path().join("claude").display().to_string();
    let options = BackendOptions { cli_model: Some("claude-sonnet-5-5".into()), config_dir: Some(claude_dir.clone()), ..Default::default() };
    let setup = OpusSetup { task_folder: Some(work.clone()), state_dir: dir.path().join("voice"), deep: false, resume: Some("ses_old".into()) };
    let exec = OpusExecutor::new(config.clone(), options, setup, AgentAdapters::built_in()).unwrap();
    let spec = exec.spec();
    assert_eq!(spec.project.as_deref(), Some(work.as_path()), "works in the task folder");
    assert_eq!(spec.resume.as_deref(), Some("ses_old"));
    assert_eq!(spec.options.config_dir.as_deref(), Some(claude_dir.as_str()), "the person's Claude Code folder is kept");
    let inbox = spec.builder.as_ref().unwrap().inbox.clone();
    assert!(inbox.starts_with(dir.path().join("voice")));
    let adapter = spec.adapters.for_backend(&spec.config.model, &spec.options);
    let mcp = McpCommand::local_or_remote(false, None, Some(inbox));
    let args = crate::cli_agents::build_args(
        adapter.as_ref(),
        &spec.options,
        "open it",
        spec.preamble.as_deref().unwrap(),
        &mcp,
        spec.resume.as_deref(),
        crate::cli_agents::Access::CleanRoomBuild,
    );
    let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    assert_eq!(after("--model").as_deref(), Some(VOICE_OPUS_MODEL), "always Opus 5.5, whatever the backend chose");
    assert_eq!(after("--permission-prompt-tool").as_deref(), Some("mcp__craft-anything__core_action_approve"));
    assert_eq!(after("--resume").as_deref(), Some("ses_old"));
    assert!(spec.preamble.as_deref().unwrap().contains("core_person_ask with the exact text"));

    // Over SSH it would run elsewhere, away from the voice's approvals: refused.
    let remote = BackendConfig { ssh_host: Some("box".into()), ..config };
    let setup = OpusSetup { task_folder: Some(work), state_dir: dir.path().join("voice"), ..Default::default() };
    assert!(OpusExecutor::new(remote, BackendOptions::default(), setup, AgentAdapters::built_in()).is_err());
}

/// The backend is the person's Claude Code on this computer, "Claude Code" first.
#[test]
fn the_claude_code_backend_is_found_in_the_ai_settings() {
    let entry = |name: &str, model: &str, ssh: Option<&str>| crate::BackendEntry {
        config: BackendConfig {
            name: name.into(),
            kind: BackendKind::AgentCli,
            model: model.into(),
            base_url: None,
            ssh_host: ssh.map(str::to_string),
            enabled: true,
        },
        options: BackendOptions::default(),
    };
    let ai = crate::AiConfig {
        backends: vec![
            entry("Codex", "codex", None),
            entry("Remote", "claude-code", Some("box")),
            entry("Work", "claude-code", None),
            entry("Claude Code", "claude-code", None),
        ],
        ..Default::default()
    };
    assert_eq!(executor::claude_code_backend(&ai).unwrap().config.name, "Claude Code");
    let ai = crate::AiConfig { backends: vec![entry("Codex", "codex", None), entry("Remote", "claude-code", Some("box"))], ..Default::default() };
    assert!(executor::claude_code_backend(&ai).is_err());
}

/// The person's words come first, unchanged; what the voice understood and the conversation
/// follow, marked as context.
#[test]
fn opus_gets_the_words_verbatim_with_context_after() {
    let job = JobRequest {
        verbatim: "post the trailer to my socials".into(),
        instruction: "Post the trailer to social media.".into(),
        context: vec!["User: hi".into(), "Assistant: Hello!".into()],
    };
    let text = executor::compose(&job);
    assert!(text.starts_with("post the trailer to my socials\n\n(Spoken instruction"), "{text}");
    assert!(text.contains("understood it as: \"Post the trailer to social media.\"") && text.contains("User: hi\nAssistant: Hello!"), "{text}");
}

/// The thread API: commands in, events out, status read-only, and Stop ends it.
#[test]
fn the_voice_brain_thread_takes_commands_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = Inbox::new(prepare_workspace(&dir.path().join("tasks")).unwrap().inbox_dir());
    let log = ExecLog::default();
    let config = BrainConfig { tick: Duration::from_millis(10), ..BrainConfig::default() };
    let model = ScriptedModel { asked: Arc::default(), answer: sensible };
    let brain = VoiceBrain::start(config, Box::new(model), Box::new(FakeExec { log: log.clone(), inbox })).unwrap();
    brain.utterance(Utterance::verified("open the trailer folder"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while brain.status().running.is_none() {
        assert!(Instant::now() < deadline);
        all.extend(brain.poll());
        std::thread::sleep(Duration::from_millis(10));
    }
    brain.cancel();
    assert!(log.cancelled.load(Ordering::SeqCst), "cancelled at once, from the caller's thread");
    brain.stop();
    while !all.contains(&BrainEvent::Stopped) {
        assert!(Instant::now() < deadline, "{all:#?}");
        all.extend(brain.poll());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(all.iter().any(|e| matches!(e, BrainEvent::JobStarted { .. })));
}

/// What a finished job left waiting is answered no and never read out later; an executor that
/// starts again clears what an earlier run left.
#[test]
fn what_a_finished_job_left_waiting_is_dropped() {
    let mut r = rig(sensible);
    r.say("push my code");
    let raised = r.inbox.post(RequestKind::Approval, "Run a command: `git push`", "{}", vec![], false, Some("publish".into())).unwrap();
    r.tick();
    r.log.push(ExecEvent::Finished { job: 1, text: "Done.".into(), error: None, cancelled: false });
    let unseen = r.inbox.post(RequestKind::Approval, "Run a command: `git push -f`", "{}", vec![], false, Some("publish".into())).unwrap();
    let events = r.tick();
    assert!(events.contains(&BrainEvent::ApprovalResolved { id: raised.clone(), approved: false, how: Resolution::Withdrawn }), "{events:?}");
    assert!(needed(&events).is_empty(), "{events:?}");
    assert_eq!(answer_of(&r.inbox, &unseen).and_then(|a| a.allow), Some(false));
    assert!(needed(&r.tick()).is_empty());

    let dir = tempfile::tempdir().unwrap();
    let inbox = Inbox::new(prepare_workspace(&dir.path().join("voice").join("tasks")).unwrap().inbox_dir());
    inbox.post(RequestKind::Approval, "old", "", vec![], false, None).unwrap();
    let _exec = opus("done", None, dir.path());
    assert!(inbox.pending().is_empty(), "cleared on start");
}
