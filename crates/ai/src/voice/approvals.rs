//! Voice approvals: Claude Code's permission requests for a voice job come through Craft
//! Anything's approval tool (`--permission-prompt-tool mcp__craft-anything__core_action_approve`)
//! into an inbox of the voice's own, and the brain reads each one out and answers it.
//!
//! The approval policy runs in our tool server, which reads the project the inbox belongs to. The
//! voice's project ([`prepare_workspace`]) chooses "ask" for every outward kind (publishing,
//! deleting outside the project or temp, system changes, tools without a rule), so those are put
//! to the person even when Auto mode would let them through or refuse them unasked. This choice is
//! written again every time the executor starts, so it can't be changed for good.

use craft_anything_engine::evidence::ToolError;
use craft_anything_engine::inbox::{Request, RequestKind};
use craft_anything_engine::workspace::{ActionKind, Approval, Questions, TargetRef, Workspace};
use serde_json::Value;
use std::path::Path;

/// The kinds a voice job always asks about, whatever Auto mode says.
pub const ALWAYS_ASKED: &[ActionKind] = &[ActionKind::Publish, ActionKind::DeleteOutside, ActionKind::System, ActionKind::OtherTools];

/// The voice's project in `dir` (made when it isn't there), with every [`ALWAYS_ASKED`] kind set
/// to ask. Its inbox is where voice jobs' approvals wait.
pub fn prepare_workspace(dir: &Path) -> Result<Workspace, ToolError> {
    let mut ws = match Workspace::open(dir) {
        Ok(ws) => ws,
        Err(_) => Workspace::create_at(dir, "Voice tasks", "Tasks asked for by voice and done by Opus", TargetRef::None, None)?,
    };
    ws.set_person(|m| {
        for kind in ALWAYS_ASKED {
            m.approvals.insert(*kind, Approval::Ask);
        }
        // Opus's questions (`core_person_ask`) reach the person too, whatever Auto mode says:
        // the voice reads them out (a live run showed Auto mode's "decide yourself" answering them).
        m.questions = Some(Questions::Ask);
    })?;
    std::fs::create_dir_all(ws.inbox_dir()).map_err(|e| ToolError::io(ws.inbox_dir().display(), e))?;
    Ok(ws)
}

/// Remove requests and answers an earlier run left in the voice inbox: nobody waits for them any
/// more (a voice job's approval tool ends with its job), so none is read out as if it were new.
pub fn clear_inbox(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten().take(10_000) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".request.json") || name.ends_with(".answer.json") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// What waits for the person's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Permission for an action: yes or no.
    Approval,
    /// A question Opus asked (`core_person_ask`): any answer.
    Question,
}

/// One request turned into words.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub kind: PromptKind,
    /// What's said: "Opus wants to …. Yes or no?"
    pub spoken: String,
    /// Everything about it, for the screen.
    pub detail: String,
    /// It posts, sends, buys, deletes outside temp or changes the system.
    pub outward: bool,
    /// Suggested answers (questions).
    pub choices: Vec<String>,
}

/// The policy tags (`Request::action`) of the outward kinds that are remembered by kind.
const OUTWARD_TAGS: &[&str] = &["publish", "delete_outside", "system"];

/// Words in an action that reach other people or can't be undone.
const OUTWARD_WORDS: &[&str] = &[
    "post",
    "posts",
    "tweet",
    "publish",
    "upload",
    "send",
    "sends",
    "email",
    "mail",
    "sendmail",
    "send-mailmessage",
    "buy",
    "purchase",
    "checkout",
    "order",
    "pay",
    "payment",
    "rm",
    "del",
    "erase",
    "rmdir",
    "remove-item",
    "delete",
    "unlink",
    "shred",
    "push",
    "share",
    "message",
    "dm",
    "reply",
    "comment",
    "invite",
];

/// Whether a request is outward: by the policy's tag, or by its words.
pub fn is_outward(req: &Request) -> bool {
    if req.action.as_deref().is_some_and(|a| OUTWARD_TAGS.contains(&a)) {
        return true;
    }
    let text = format!("{} {}", req.title, req.detail);
    super::intent::words(&text).iter().any(|w| OUTWARD_WORDS.contains(&w.as_str()))
        || text.to_ascii_lowercase().contains("-x post")
        || text.to_ascii_lowercase().contains("method post")
}

/// The request in plain words. `plain` is the local model's few words for the action (it may be
/// wrong or missing, so the facts that matter are added from the request itself): for an outward
/// action, the text it would post or send and every file and link it uses are read out first.
pub fn describe(req: &Request, plain: Option<&str>) -> Described {
    let detail = if req.detail.trim().is_empty() { req.title.clone() } else { format!("{}\n\n{}", req.title, req.detail) };
    if req.kind == RequestKind::Question {
        let mut spoken = format!("Opus asks: {}", super::intent::spoken(&req.title, 3, 400));
        if !req.choices.is_empty() {
            spoken.push_str(&format!(" The choices are: {}.", or_list(&req.choices)));
        }
        return Described { kind: PromptKind::Question, spoken, detail, outward: is_outward(req), choices: req.choices.clone() };
    }
    let outward = is_outward(req);
    let what =
        plain.map(|p| super::intent::spoken(p.trim().trim_end_matches('.'), 1, 160)).filter(|p| !p.is_empty()).unwrap_or_else(|| plain_title(&req.title));
    let what = what.trim_end_matches(['.', '…']).to_string();
    // The model often starts its words the way the sentence does ("Opus wants to send…").
    let what = ["opus wants to ", "opus would like to ", "it wants to ", "wants to "]
        .iter()
        .find_map(|p| what.get(..p.len()).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| what[p.len()..].to_string()))
        .unwrap_or(what);
    let mut spoken = format!("Opus wants to {}.", lower_first(&what));
    if outward {
        let input: Value = serde_json::from_str(&req.detail).unwrap_or(Value::Null);
        let facts = Facts::of(&req.title, &input);
        spoken.push_str(" This reaches outside your computer or can't be undone.");
        if let Some(caption) = facts.texts.first() {
            spoken.push_str(&format!(" The text reads: \"{}\".", super::intent::spoken(caption, 4, 500)));
        }
        if !facts.files.is_empty() {
            spoken.push_str(&format!(" It uses {}.", and_list(&facts.files.iter().map(|f| spoken_path(f)).collect::<Vec<_>>())));
        }
        if !facts.links.is_empty() {
            spoken.push_str(&format!(" The link is {}.", and_list(&facts.links)));
        }
        if plain.is_some() {
            spoken.push_str(&format!(" Exactly: {}.", plain_title(&req.title).trim_end_matches('.')));
        }
    }
    spoken.push_str(" Yes or no?");
    Described { kind: PromptKind::Approval, spoken, detail, outward, choices: Vec::new() }
}

/// The policy's title ("Run a command: `git push`") read aloud: no backticks.
fn plain_title(title: &str) -> String {
    let t: String = title.chars().filter(|c| *c != '`').collect();
    super::intent::spoken(&t, 2, 300)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // "Run a command" → "run a command", but "GitHub" stays as it is.
        Some(f) if c.clone().next().is_some_and(char::is_lowercase) => f.to_lowercase().chain(c).collect(),
        Some(f) => std::iter::once(f).chain(c).collect(),
        None => String::new(),
    }
}

fn and_list(items: &[String]) -> String {
    join_list(items, "and")
}

fn or_list(items: &[String]) -> String {
    join_list(items, "or")
}

fn join_list(items: &[String], word: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} {word} {last}", head.join(", ")),
    }
}

/// A path as it's said: its file name, with the folder it's in.
fn spoken_path(p: &str) -> String {
    let path = Path::new(p);
    match (path.file_name(), path.parent().and_then(Path::file_name)) {
        (Some(f), Some(d)) => format!("the file {} in {}", f.to_string_lossy(), d.to_string_lossy()),
        (Some(f), None) => format!("the file {}", f.to_string_lossy()),
        _ => p.to_string(),
    }
}

/// The text, files and links an outward action uses.
#[derive(Debug, Default, PartialEq)]
pub struct Facts {
    pub texts: Vec<String>,
    pub files: Vec<String>,
    pub links: Vec<String>,
}

/// Field names and flags that carry the text to be posted or sent.
const TEXT_KEYS: &[&str] = &["caption", "text", "message", "body", "content", "status", "description", "title", "subject", "comment", "tweet", "post"];
/// File endings worth naming.
const FILE_ENDINGS: &[&str] =
    &["mp4", "mov", "webm", "mkv", "avi", "gif", "png", "jpg", "jpeg", "webp", "mp3", "wav", "ogg", "m4a", "pdf", "txt", "md", "zip", "doc", "docx", "csv"];

impl Facts {
    /// From the action's input: its fields for a tool, its arguments for a command.
    pub fn of(title: &str, input: &Value) -> Facts {
        let mut f = Facts::default();
        if let Some(map) = input.as_object() {
            for (k, v) in map {
                let Some(s) = v.as_str() else { continue };
                if k == "command" {
                    f.read_command(s);
                } else if TEXT_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
                    f.push_text(s);
                } else {
                    f.read_words(s);
                }
            }
        } else {
            f.read_command(title);
        }
        f
    }

    fn push_text(&mut self, s: &str) {
        let s = s.trim();
        if !s.is_empty() && !self.texts.iter().any(|t| t == s) && self.texts.len() < 4 {
            self.texts.push(s.to_string());
        }
    }

    fn read_words(&mut self, s: &str) {
        for w in shell_words(s) {
            self.word(&w);
        }
    }

    /// A command's arguments: `--caption "…"`-style values are the text; paths and links named.
    fn read_command(&mut self, command: &str) {
        let words = shell_words(command);
        let mut i = 0;
        while let Some(w) = words.get(i) {
            let bare = w.trim_start_matches('-');
            let (name, inline) = bare.split_once('=').map_or((bare, None), |(n, v)| (n, Some(v)));
            let is_text_flag = w.starts_with('-') && (TEXT_KEYS.contains(&name.to_ascii_lowercase().as_str()) || w.as_str() == "-m");
            if is_text_flag {
                match inline {
                    Some(v) => self.push_text(v),
                    None => {
                        if let Some(v) = words.get(i + 1) {
                            self.push_text(v);
                            i += 1;
                        }
                    }
                }
            } else {
                self.word(w);
            }
            i += 1;
        }
    }

    fn word(&mut self, w: &str) {
        let w = w.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '"' | '\''));
        if w.starts_with("http://") || w.starts_with("https://") {
            if !self.links.iter().any(|l| l == w) && self.links.len() < 3 {
                self.links.push(w.to_string());
            }
            return;
        }
        let file = w.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && FILE_ENDINGS.contains(&ext.to_ascii_lowercase().as_str()));
        let file = file && !w.starts_with('-');
        if file && !self.files.iter().any(|f| f == w) && self.files.len() < 4 {
            self.files.push(w.to_string());
        }
    }
}

/// A command's words as a shell reads them: quoted text stays one word.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(c);
                started = true;
            }
        }
        if words.len() >= 512 {
            break;
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(title: &str, detail: &str, action: Option<&str>) -> Request {
        Request {
            id: "1-1-1".into(),
            kind: RequestKind::Approval,
            title: title.into(),
            detail: detail.into(),
            choices: Vec::new(),
            allow_text: false,
            action: action.map(str::to_string),
            created_ms: 0,
        }
    }

    /// For a post, the caption and the file are read out before the question.
    #[test]
    fn a_post_reads_its_caption_and_names_its_file_first() {
        let cmd = r#"python post.py --caption "Riding into the Rift tonight!" --file "C:/clips/trailer v2.mp4" --url https://example.com/u"#;
        let input = serde_json::json!({"command": cmd});
        let req = approval(&format!("Run a command: `{cmd}`"), &input.to_string(), Some("bash:python"));
        let d = describe(&req, Some("post the trailer to social media"));
        assert!(d.outward, "posting is outward by its words");
        assert!(d.spoken.starts_with("Opus wants to post the trailer to social media."), "{}", d.spoken);
        assert!(d.spoken.contains("The text reads: \"Riding into the Rift tonight!\""), "{}", d.spoken);
        assert!(d.spoken.contains("the file trailer v2.mp4 in clips"), "{}", d.spoken);
        assert!(d.spoken.contains("https://example.com/u"), "{}", d.spoken);
        assert!(d.spoken.contains("Exactly: Run a command: python post.py"), "the model's words are checked by the real ones: {}", d.spoken);
        assert!(d.spoken.ends_with("Yes or no?"));
        let pos = |s: &str| d.spoken.find(s).unwrap();
        assert!(pos("The text reads") < pos("Yes or no?"));
    }

    #[test]
    fn the_policy_tag_marks_outward_kinds() {
        for tag in ["publish", "delete_outside", "system"] {
            assert!(is_outward(&approval("Run a command: `x`", "{}", Some(tag))), "{tag}");
        }
        let edit = approval("Change a file outside the project: C:/notes/today.txt", r#"{"file_path":"C:/notes/today.txt"}"#, Some("edit-outside"));
        assert!(!is_outward(&edit));
        let d = describe(&edit, None);
        assert_eq!(d.spoken, "Opus wants to change a file outside the project: C:/notes/today.txt. Yes or no?");
        let del = approval("Run a command: `rm -rf C:/Users/x/Downloads/a`", r#"{"command":"rm -rf C:/Users/x/Downloads/a"}"#, Some("delete_outside"));
        assert!(describe(&del, None).spoken.contains("can't be undone"));
    }

    #[test]
    fn a_question_reads_its_choices() {
        let mut q = approval("Which caption should I use?", "", None);
        q.kind = RequestKind::Question;
        q.choices = vec!["Short".into(), "Long".into(), "None".into()];
        let d = describe(&q, None);
        assert_eq!(d.kind, PromptKind::Question);
        assert_eq!(d.spoken, "Opus asks: Which caption should I use? The choices are: Short, Long or None.");
    }

    #[test]
    fn facts_come_from_tool_fields_too() {
        let f = Facts::of("Use post", &serde_json::json!({"caption": "Hello world", "media": "C:/a/b.png", "n": 3}));
        assert_eq!(f.texts, vec!["Hello world"]);
        assert_eq!(f.files, vec!["C:/a/b.png"]);
        let f = Facts::of("x", &serde_json::json!({"command": "git commit -m 'ship it' && git push"}));
        assert_eq!(f.texts, vec!["ship it"]);
    }

    /// The voice's project asks about every outward kind even in Auto mode, and keeps doing so
    /// after someone changed it.
    #[test]
    fn the_voice_project_always_asks_about_outward_kinds() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("voice");
        let mut ws = prepare_workspace(&dir).unwrap();
        for k in ALWAYS_ASKED {
            assert_eq!(ws.manifest.approvals.get(k), Some(&Approval::Ask));
        }
        ws.set_person(|m| {
            m.approvals.insert(ActionKind::Publish, Approval::Allow);
        })
        .unwrap();
        let ws = prepare_workspace(&dir).unwrap();
        assert_eq!(ws.manifest.approvals.get(&ActionKind::Publish), Some(&Approval::Ask), "set again on every start");
        assert_eq!(ws.manifest.questions, Some(Questions::Ask), "Opus's questions are read out, even in Auto mode");
        assert!(ws.inbox_dir().is_dir());
    }

    /// The model's words often start like the sentence they go into: said once.
    #[test]
    fn the_spoken_request_says_opus_wants_to_once() {
        let req = Request {
            id: "r".into(),
            kind: RequestKind::Approval,
            title: "Run a command: `curl -X POST --data hi https://httpbin.org/post`".into(),
            detail: serde_json::json!({"command": "curl -X POST --data hi https://httpbin.org/post"}).to_string(),
            choices: vec![],
            allow_text: false,
            action: Some("publish".into()),
            created_ms: 0,
        };
        let d = describe(&req, Some("Opus wants to send \"hi\" to httpbin.org"));
        assert!(d.spoken.starts_with("Opus wants to send \"hi\""), "{}", d.spoken);
        assert_eq!(d.spoken.matches("wants to").count(), 1, "{}", d.spoken);
        assert!(d.outward);
    }
}
