//! The brain: one utterance at a time, it decides with the local model what the person meant and
//! acts on it: says the model's short reply, hands instructions to the executor (never to the
//! model), answers what waits for the person, stops the job. Between utterances ([`Brain::tick`])
//! it reads the job's progress, says a sentence about it now and then, reads out every approval
//! Opus asks for, and answers no to any left unanswered for too long.

use super::approvals::{self, PromptKind};
use super::executor::{ExecEvent, Executor, JobControl, JobRequest};
use super::intent::{self, Intent};
use super::local::{ChatMessage, ChatRequest, Converser, Purpose};
use super::{BrainCommand, BrainEvent, ENROL_FIRST, JobStatus, Resolution, RunningJob, Speaker, TranscriptLine, Utterance};
use craft_anything_engine::inbox::{Answer, Inbox, RequestKind};
use craft_anything_platform::sync::Mutex;
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// How the brain paces itself.
#[derive(Debug, Clone)]
pub struct BrainConfig {
    /// How long an approval waits for a yes before it's a no.
    pub approval_timeout: Duration,
    /// How long a question Opus asked waits before Opus is told to decide itself.
    pub question_timeout: Duration,
    /// How often a running job's progress is said, when it has done something new.
    pub narrate_every: Duration,
    /// The least time between a milestone sentence and the last thing said.
    pub milestone_gap: Duration,
    /// How often the approvals inbox is read.
    pub inbox_every: Duration,
    /// How often the brain's thread wakes when nothing is said.
    pub tick: Duration,
    /// Conversation lines kept for the model (and handed to Opus as context).
    pub history_lines: usize,
    /// Of which this many go to Opus with an instruction.
    pub context_lines: usize,
    /// Instructions kept waiting while a job runs.
    pub max_queue: usize,
}

impl Default for BrainConfig {
    fn default() -> Self {
        BrainConfig {
            approval_timeout: Duration::from_secs(120),
            question_timeout: Duration::from_secs(300),
            narrate_every: Duration::from_secs(25),
            milestone_gap: Duration::from_secs(4),
            inbox_every: Duration::from_millis(300),
            tick: Duration::from_millis(150),
            history_lines: 16,
            context_lines: 6,
            max_queue: 3,
        }
    }
}

/// Something waiting for the person.
#[derive(Debug, Clone)]
struct Prompt {
    id: String,
    kind: PromptKind,
    spoken: String,
    choices: Vec<String>,
    asked: Instant,
}

struct JobState {
    id: u64,
    instruction: String,
    started: Instant,
    /// Its latest steps.
    recent: VecDeque<String>,
    /// Something happened since the last progress sentence.
    fresh: bool,
    /// A milestone waits to be said.
    milestone: bool,
    last_said: Instant,
}

/// The brain's state, driven by [`Brain::command`] and [`Brain::tick`] (the [`super::VoiceBrain`]
/// thread does both).
pub struct Brain {
    config: BrainConfig,
    model: Box<dyn Converser>,
    exec: Box<dyn Executor>,
    control: JobControl,
    out: Sender<BrainEvent>,
    status: Arc<Mutex<JobStatus>>,
    history: VecDeque<String>,
    last_said: Option<String>,
    says: u64,
    prompts: Vec<Prompt>,
    /// Inbox requests already put to the person (answered or not).
    seen: HashSet<String>,
    job: Option<JobState>,
    queue: VecDeque<JobRequest>,
    last_result: Option<String>,
    inbox_read: Option<Instant>,
    stopped: bool,
}

impl Brain {
    pub fn new(config: BrainConfig, model: Box<dyn Converser>, exec: Box<dyn Executor>, out: Sender<BrainEvent>) -> Brain {
        let control = exec.control();
        Brain {
            config,
            model,
            exec,
            control,
            out,
            status: Arc::default(),
            history: VecDeque::new(),
            last_said: None,
            says: 0,
            prompts: Vec::new(),
            seen: HashSet::new(),
            job: None,
            queue: VecDeque::new(),
            last_result: None,
            inbox_read: None,
            stopped: false,
        }
    }

    /// The status the brain keeps up to date for the screen.
    pub fn shared_status(&self) -> Arc<Mutex<JobStatus>> {
        self.status.clone()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// The jobs now.
    pub fn status(&self) -> JobStatus {
        JobStatus {
            running: self.job.as_ref().map(|j| RunningJob {
                id: j.id,
                instruction: j.instruction.clone(),
                seconds: j.started.elapsed().as_secs(),
                recent: j.recent.iter().cloned().collect(),
            }),
            queued: self.queue.iter().map(|q| q.instruction.clone()).collect(),
            waiting: self.prompts.iter().map(|p| p.spoken.clone()).collect(),
            last_result: self.last_result.clone(),
            session: self.exec.session_id(),
        }
    }

    fn publish_status(&self) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = self.status();
    }

    fn emit(&self, e: BrainEvent) {
        let _ = self.out.send(e);
    }

    fn line(&self, speaker: Speaker, text: &str, score: Option<f32>) {
        self.emit(BrainEvent::Transcript(TranscriptLine { speaker, text: text.to_string(), score }));
    }

    fn remember(&mut self, line: String) {
        self.history.push_back(line);
        while self.history.len() > self.config.history_lines {
            self.history.pop_front();
        }
    }

    /// Speak `text` (and show it).
    fn say(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.says = self.says.saturating_add(1);
        self.emit(BrainEvent::Say { id: format!("v{}", self.says), text: text.to_string() });
        self.line(Speaker::Assistant, text, None);
        self.last_said = Some(text.to_string());
        self.remember(format!("Assistant: {text}"));
        if let Some(j) = self.job.as_mut() {
            j.last_said = Instant::now();
        }
    }

    pub fn command(&mut self, cmd: BrainCommand) {
        match cmd {
            BrainCommand::Utterance(u) => self.utterance(u),
            BrainCommand::ApproveYes(id) => self.resolve(&id, true, None, Resolution::Screen),
            BrainCommand::ApproveNo(id) => self.resolve(&id, false, None, Resolution::Screen),
            BrainCommand::Answer { id, text } => self.resolve(&id, true, Some(text), Resolution::Screen),
            BrainCommand::Cancel => self.cancel(true),
            BrainCommand::NewConversation => {
                self.cancel(false);
                self.exec.new_conversation();
                self.history.clear();
            }
            BrainCommand::Stop => {
                self.cancel(false);
                self.stopped = true;
                self.emit(BrainEvent::Stopped);
            }
        }
        self.publish_status();
    }

    /// One utterance. Someone else's voice is shown and nothing more: no model, no job. A clear
    /// no, cancel or stop is the exception ([`intent::safe_answer`]): it can only hold something
    /// back, so it counts from any voice, before enrolment too, and when a short clip leaves the
    /// speaker check unsure.
    fn utterance(&mut self, u: Utterance) {
        let text = u.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if (!u.speaker_ok || !u.verified)
            && let Some(safe) = intent::safe_answer(&text)
            && self.take_safe_answer(&text, safe, u.speaker_score)
        {
            return;
        }
        if !u.speaker_ok {
            self.line(Speaker::NotUser, &text, Some(u.speaker_score));
            return;
        }
        self.line(Speaker::User, &text, Some(u.speaker_score));
        let waiting = !self.prompts.is_empty();
        let working = self.job.is_some() || !self.queue.is_empty();
        let c = match intent::quick_intent(&text, waiting, working) {
            Some(kind) => intent::Classification { kind, reply: String::new(), instruction: text.clone(), fallback: false },
            None => self.classify(&text),
        };
        self.remember(format!("User: {text}"));
        if !u.verified && !matches!(c.kind, Intent::Chat | Intent::Question | Intent::Repeat) {
            // Nobody's voice can be told from another's yet: talk, but take no instruction and no
            // answer to anything waiting.
            self.say(ENROL_FIRST);
            self.emit(BrainEvent::EnrolmentNeeded);
            return;
        }
        self.act(&text, c);
    }

    /// Ask the local model what `text` is; its answer is only read, never run.
    fn classify(&mut self, text: &str) -> intent::Classification {
        let context = format!(
            "Job status: {}\nWaiting for the user's answer: {}\nConversation so far:\n{}\n\nThe user just said: \"{text}\"",
            self.status_text(),
            self.prompts.first().map_or_else(|| "nothing".to_string(), |p| p.spoken.clone()),
            if self.history.is_empty() { "(nothing yet)".to_string() } else { self.history.iter().cloned().collect::<Vec<_>>().join("\n") },
        );
        let req = ChatRequest {
            purpose: Purpose::Classify,
            messages: vec![ChatMessage::system(intent::CLASSIFY_SYSTEM), ChatMessage::user(context)],
            json: true,
            max_tokens: 300,
        };
        match self.model.chat(&req) {
            Ok(raw) => intent::parse_classification(&raw, text),
            Err(e) => {
                log::warn!("voice: the local model couldn't be asked: {}", e.message);
                self.line(Speaker::Notice, &format!("The local model didn't answer ({}).", e.message), None);
                intent::fallback(text)
            }
        }
    }

    /// The jobs in words, for the model (read-only facts).
    fn status_text(&self) -> String {
        let mut s = match &self.job {
            Some(j) => {
                let steps: Vec<&str> = j.recent.iter().rev().take(4).map(String::as_str).collect();
                format!(
                    "Opus has been working for {} seconds on: \"{}\". Its latest steps: {}.",
                    j.started.elapsed().as_secs(),
                    j.instruction,
                    if steps.is_empty() { "none yet".to_string() } else { steps.join("; ") }
                )
            }
            None => "Opus isn't working on anything.".to_string(),
        };
        if !self.queue.is_empty() {
            s.push_str(&format!(" Waiting to start: {}.", self.queue.iter().map(|q| format!("\"{}\"", q.instruction)).collect::<Vec<_>>().join(", ")));
        }
        if let Some(r) = &self.last_result {
            s.push_str(&format!(" The last finished job: {r}"));
        }
        s
    }

    fn act(&mut self, text: &str, c: intent::Classification) {
        // A question Opus asked takes the next words as its answer (unless they stop the job).
        if let Some(q) = self.prompts.iter().find(|p| p.kind == PromptKind::Question).cloned()
            && !matches!(c.kind, Intent::Cancel | Intent::Repeat)
            && self.prompts.first().is_some_and(|p| p.id == q.id)
        {
            let answer = match c.kind {
                Intent::ConfirmYes => q.choices.iter().find(|x| x.eq_ignore_ascii_case("yes")).cloned().unwrap_or_else(|| text.to_string()),
                Intent::ConfirmNo => q.choices.iter().find(|x| x.eq_ignore_ascii_case("no")).cloned().unwrap_or_else(|| text.to_string()),
                _ => text.to_string(),
            };
            self.resolve(&q.id, true, Some(answer), Resolution::Voice);
            return;
        }
        match c.kind {
            Intent::Chat | Intent::Question => {
                let reply = if c.reply.is_empty() { intent::DIDNT_CATCH.to_string() } else { c.reply };
                self.say(&reply);
            }
            Intent::Instruction => self.instruct(text, &c.instruction),
            Intent::ConfirmYes | Intent::ConfirmNo => {
                let Some(p) = self.prompts.first().cloned() else {
                    let reply = if c.reply.is_empty() { "There's nothing waiting for a yes or no right now.".to_string() } else { c.reply };
                    self.say(&reply);
                    return;
                };
                if c.kind == Intent::ConfirmYes && !intent::is_affirmative(text) {
                    // Only the person's own clear yes approves anything, whatever the model read.
                    self.say("Please say yes or no.");
                    return;
                }
                self.resolve(&p.id, c.kind == Intent::ConfirmYes, None, Resolution::Voice);
            }
            Intent::Cancel => self.cancel(true),
            Intent::Repeat => match self.last_said.clone() {
                Some(last) => {
                    self.says = self.says.saturating_add(1);
                    self.emit(BrainEvent::Say { id: format!("v{}", self.says), text: last });
                }
                None => self.say("I haven't said anything yet."),
            },
        }
    }

    /// Hand an instruction to Opus, or keep it until the running job ends.
    fn instruct(&mut self, verbatim: &str, instruction: &str) {
        let skip = self.history.len().saturating_sub(self.config.context_lines.saturating_add(1));
        // The last line is the utterance itself, which Opus gets verbatim anyway.
        let context: Vec<String> = self.history.iter().skip(skip).take(self.config.context_lines).cloned().collect();
        let job = JobRequest { verbatim: verbatim.to_string(), instruction: instruction.to_string(), context };
        if self.job.is_some() || self.exec.is_busy() {
            if self.queue.len() >= self.config.max_queue {
                self.say("Opus already has enough waiting. Say cancel to stop the current job, or wait a moment.");
                return;
            }
            self.queue.push_back(job);
            self.say("Opus is busy, so I'll pass that on when it's done.");
            return;
        }
        self.start(job);
    }

    fn start(&mut self, job: JobRequest) {
        match self.exec.submit(&job) {
            Ok(id) => {
                let now = Instant::now();
                self.job = Some(JobState {
                    id,
                    instruction: job.instruction.clone(),
                    started: now,
                    recent: VecDeque::new(),
                    fresh: false,
                    milestone: false,
                    last_said: now,
                });
                self.emit(BrainEvent::JobStarted { job: id, instruction: job.instruction });
                self.say("Okay, I've asked Opus to do that.");
            }
            Err(e) => {
                self.line(Speaker::Notice, &e.for_people(), None);
                self.say(&format!("Opus couldn't start: {}", intent::spoken(&e.message, 1, 200)));
            }
        }
    }

    /// A safe answer from a voice that isn't (or can't yet be) checked: a no to the first thing
    /// waiting, or a cancel of the job. False when there's nothing for it to act on, so it's
    /// treated like any other utterance.
    fn take_safe_answer(&mut self, text: &str, safe: intent::SafeAnswer, score: f32) -> bool {
        let working = self.job.is_some() || !self.queue.is_empty() || self.exec.is_busy();
        let first = self.prompts.first().cloned();
        match (safe, first) {
            (intent::SafeAnswer::No, Some(p)) => {
                self.line(Speaker::User, text, Some(score));
                self.remember(format!("User: {text}"));
                let answer = match p.kind {
                    PromptKind::Question => {
                        Some(p.choices.iter().find(|c| intent::safe_answer(c) == Some(intent::SafeAnswer::No)).cloned().unwrap_or_else(|| text.to_string()))
                    }
                    PromptKind::Approval => Some(format!("said no by voice: {text}")),
                };
                self.resolve(&p.id, false, answer, Resolution::Voice);
                true
            }
            (intent::SafeAnswer::Cancel, _) if working || !self.prompts.is_empty() => {
                self.line(Speaker::User, text, Some(score));
                self.remember(format!("User: {text}"));
                self.cancel(true);
                true
            }
            _ => false,
        }
    }

    /// Stop the job: its program is ended, what waits for an answer gets a no, the queue is
    /// dropped.
    fn cancel(&mut self, speak: bool) {
        let running = self.job.is_some() || self.exec.is_busy();
        if running {
            self.control.cancel();
        }
        let dropped = !self.queue.is_empty();
        self.queue.clear();
        for p in std::mem::take(&mut self.prompts) {
            self.answer_inbox(&p, false, None);
            self.emit(BrainEvent::ApprovalResolved { id: p.id, approved: false, how: Resolution::Cancelled });
        }
        if speak {
            self.say(if running || dropped { "Stopping Opus." } else { "Nothing is running." });
        }
    }

    /// Answer what waits: an approval yes or no, or a question with `text`.
    fn resolve(&mut self, id: &str, approved: bool, text: Option<String>, how: Resolution) {
        let Some(pos) = self.prompts.iter().position(|p| p.id == id) else { return };
        let p = self.prompts.remove(pos);
        let approved = match p.kind {
            PromptKind::Approval => approved,
            PromptKind::Question => true,
        };
        let text = match (p.kind, text) {
            (PromptKind::Question, None) => Some(if approved { "Yes" } else { "No" }.to_string()),
            (_, t) => t,
        };
        let delivered = self.answer_inbox(&p, approved, text);
        let how = if delivered { how } else { Resolution::Withdrawn };
        self.emit(BrainEvent::ApprovalResolved { id: p.id.clone(), approved: approved && delivered, how });
        if how == Resolution::Voice {
            match p.kind {
                PromptKind::Approval if approved => self.say("Okay, going ahead."),
                PromptKind::Approval => self.say("Okay, I told Opus no."),
                PromptKind::Question => self.say("Got it, I've told Opus."),
            }
        }
    }

    fn inbox(&self) -> Option<Inbox> {
        self.exec.inbox()
    }

    /// Write the answer for the approval tool to read. False when it can't be written.
    fn answer_inbox(&self, p: &Prompt, approved: bool, text: Option<String>) -> bool {
        let Some(inbox) = self.inbox() else { return false };
        let answer = match p.kind {
            PromptKind::Approval => Answer {
                allow: Some(approved),
                text: if approved { None } else { Some(text.unwrap_or_else(|| "said no by voice".to_string())) },
                ..Default::default()
            },
            PromptKind::Question => {
                let text = text.unwrap_or_default();
                let choice = p.choices.iter().find(|c| c.eq_ignore_ascii_case(text.trim())).cloned();
                Answer { choice: choice.clone(), text: if choice.is_some() { None } else { Some(text) }, ..Default::default() }
            }
        };
        match inbox.answer(&p.id, &answer) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("voice: couldn't answer {}: {}", p.id, e.message);
                false
            }
        }
    }

    /// Between utterances: the job's progress, new approvals, timeouts, narration.
    pub fn tick(&mut self, now: Instant) {
        if self.stopped {
            return;
        }
        for e in self.exec.poll() {
            self.exec_event(e, now);
        }
        if self.inbox_read.is_none_or(|t| now.saturating_duration_since(t) >= self.config.inbox_every) {
            self.inbox_read = Some(now);
            self.read_inbox(now);
        }
        self.time_out(now);
        self.narrate(now);
        if self.job.is_none()
            && !self.exec.is_busy()
            && let Some(next) = self.queue.pop_front()
        {
            self.start(next);
        }
        self.publish_status();
    }

    fn exec_event(&mut self, e: ExecEvent, now: Instant) {
        match e {
            ExecEvent::Activity { job, text, milestone } => {
                if let Some(j) = self.job.as_mut().filter(|j| j.id == job) {
                    j.recent.push_back(text.chars().take(200).collect());
                    while j.recent.len() > 12 {
                        j.recent.pop_front();
                    }
                    j.fresh = true;
                    j.milestone |= milestone;
                }
            }
            ExecEvent::Text { job, .. } => {
                if let Some(j) = self.job.as_mut().filter(|j| j.id == job) {
                    j.fresh = true;
                }
            }
            ExecEvent::Finished { job, text, error, cancelled } => {
                if self.job.as_ref().is_some_and(|j| j.id == job) {
                    self.job = None;
                }
                self.leftovers();
                self.finished(job, text, error, cancelled);
                self.inbox_read = Some(now);
            }
        }
    }

    /// What still waits in the inbox belonged to the job that ended: it's answered no, so a
    /// waiter that outlived its job stops, and it's never read out later.
    fn leftovers(&mut self) {
        let Some(inbox) = self.inbox() else { return };
        for p in std::mem::take(&mut self.prompts) {
            self.answer_inbox(&p, false, None);
            self.emit(BrainEvent::ApprovalResolved { id: p.id, approved: false, how: Resolution::Withdrawn });
        }
        for req in inbox.pending() {
            self.seen.insert(req.id.clone());
            let _ = inbox.answer(&req.id, &Answer { allow: Some(false), text: Some("the job ended".into()), ..Default::default() });
        }
    }

    fn finished(&mut self, job: u64, text: String, error: Option<String>, cancelled: bool) {
        if cancelled {
            self.emit(BrainEvent::JobFailed { job, message: "Stopped.".into(), cancelled: true });
            self.say("Opus has stopped.");
            return;
        }
        if let Some(err) = error.filter(|_| text.trim().is_empty()) {
            self.line(Speaker::Notice, &err, None);
            self.emit(BrainEvent::JobFailed { job, message: err.clone(), cancelled: false });
            self.say(&format!("Opus couldn't finish: {}", intent::spoken(&err, 1, 200)));
            return;
        }
        let full = text.trim().to_string();
        let summary = if full.is_empty() {
            "Opus is done.".to_string()
        } else {
            let req = ChatRequest {
                purpose: Purpose::Summarise,
                messages: vec![ChatMessage::system(intent::SUMMARISE_SYSTEM), ChatMessage::user(format!("Opus's final answer:\n{}", tail(&full, 6000)))],
                json: false,
                max_tokens: 120,
            };
            self.model.chat(&req).map(|s| intent::spoken(&s, 2, 320)).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| intent::spoken(&full, 2, 320))
        };
        if !full.is_empty() {
            self.line(Speaker::Opus, &full, None);
        }
        self.last_result = Some(summary.clone());
        self.emit(BrainEvent::JobDone { job, summary: summary.clone(), text: full });
        self.say(&summary);
    }

    /// New requests in the approvals inbox are read out; ones that went unanswered are dropped.
    fn read_inbox(&mut self, now: Instant) {
        let Some(inbox) = self.inbox() else { return };
        let pending = inbox.pending();
        let ids: HashSet<&str> = pending.iter().map(|r| r.id.as_str()).collect();
        let gone: Vec<Prompt> = self.prompts.iter().filter(|p| !ids.contains(p.id.as_str())).cloned().collect();
        for p in gone {
            self.prompts.retain(|q| q.id != p.id);
            self.emit(BrainEvent::ApprovalResolved { id: p.id, approved: false, how: Resolution::Withdrawn });
        }
        for req in pending {
            if !self.seen.insert(req.id.clone()) {
                continue;
            }
            let plain = match req.kind {
                RequestKind::Approval => self.describe_action(&req.title, &req.detail),
                RequestKind::Question => None,
            };
            let d = approvals::describe(&req, plain.as_deref());
            self.prompts.push(Prompt { id: req.id.clone(), kind: d.kind, spoken: d.spoken.clone(), choices: d.choices.clone(), asked: now });
            self.emit(BrainEvent::ApprovalNeeded {
                id: req.id.clone(),
                kind: d.kind,
                text: d.spoken.clone(),
                detail: d.detail,
                outward: d.outward,
                choices: d.choices,
            });
            self.say(&d.spoken);
        }
    }

    /// The local model's few words for an action (or none).
    fn describe_action(&self, title: &str, detail: &str) -> Option<String> {
        let req = ChatRequest {
            purpose: Purpose::Describe,
            messages: vec![
                ChatMessage::system(intent::DESCRIBE_SYSTEM),
                ChatMessage::user(format!("Action: {title}\nDetails: {}", detail.chars().take(2000).collect::<String>())),
            ],
            json: false,
            max_tokens: 60,
        };
        self.model.chat(&req).ok().map(|s| s.lines().next().unwrap_or_default().trim().trim_matches('"').to_string()).filter(|s| !s.is_empty())
    }

    /// An approval unanswered for too long is a no; a question, Opus decides itself.
    fn time_out(&mut self, now: Instant) {
        let late: Vec<Prompt> = self
            .prompts
            .iter()
            .filter(|p| {
                let limit = match p.kind {
                    PromptKind::Approval => self.config.approval_timeout,
                    PromptKind::Question => self.config.question_timeout,
                };
                now.saturating_duration_since(p.asked) >= limit
            })
            .cloned()
            .collect();
        for p in late {
            self.prompts.retain(|q| q.id != p.id);
            match p.kind {
                PromptKind::Approval => {
                    self.answer_inbox(&p, false, Some("no answer came in time, so it wasn't done".into()));
                    self.emit(BrainEvent::ApprovalResolved { id: p.id.clone(), approved: false, how: Resolution::TimedOut });
                    self.say("I didn't hear a yes, so I told Opus no.");
                }
                PromptKind::Question => {
                    let text = "No answer came by voice. Decide yourself, and say what you chose.".to_string();
                    self.answer_inbox(&p, true, Some(text));
                    self.emit(BrainEvent::ApprovalResolved { id: p.id.clone(), approved: false, how: Resolution::TimedOut });
                    self.say("No answer, so I've told Opus to decide.");
                }
            }
        }
    }

    /// Now and then, and on milestones, one sentence about what the job is doing.
    fn narrate(&mut self, now: Instant) {
        let Some(j) = self.job.as_ref() else { return };
        if !self.prompts.is_empty() || !j.fresh {
            return;
        }
        let since = now.saturating_duration_since(j.last_said);
        let due = since >= self.config.narrate_every || (j.milestone && since >= self.config.milestone_gap);
        if !due {
            return;
        }
        let (id, steps) = (j.id, j.recent.iter().cloned().collect::<Vec<_>>().join("\n"));
        let req = ChatRequest {
            purpose: Purpose::Narrate,
            messages: vec![ChatMessage::system(intent::NARRATE_SYSTEM), ChatMessage::user(format!("Opus's recent activity, newest last:\n{steps}"))],
            json: false,
            max_tokens: 60,
        };
        let sentence = self.model.chat(&req).map(|s| intent::spoken(&s, 1, 200)).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            let last = steps.lines().last().unwrap_or("working");
            intent::spoken(&format!("Opus is still working: {last}."), 1, 200)
        });
        if let Some(j) = self.job.as_mut() {
            j.fresh = false;
            j.milestone = false;
        }
        self.emit(BrainEvent::JobProgress { job: id, text: sentence.clone() });
        self.say(&sentence);
    }
}

/// The last `max` characters of `text`.
fn tail(text: &str, max: usize) -> String {
    let n = text.chars().count();
    text.chars().skip(n.saturating_sub(max)).collect()
}
