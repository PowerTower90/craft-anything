//! What one utterance is: the local model's JSON answer read robustly, the few short words that
//! never need the model ("yes", "cancel"), and the prompts that ask for it. Everything here is
//! pure text work, so it is tested without a model.

use serde_json::Value;

/// What the person meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Small talk or anything that needs no action: the local model answers.
    Chat,
    /// A question answered from the conversation or the jobs' status, read-only.
    Question,
    /// Something to be done: always Opus's work, never the local model's.
    Instruction,
    ConfirmYes,
    ConfirmNo,
    /// Stop the running job.
    Cancel,
    /// Say the last thing again.
    Repeat,
}

impl Intent {
    /// The kind as the model writes it (`confirm_yes`), with the synonyms small models use.
    pub fn parse(kind: &str) -> Option<Intent> {
        let k: String = kind.trim().to_ascii_lowercase().chars().map(|c| if c == '-' || c == ' ' { '_' } else { c }).collect();
        Some(match k.as_str() {
            "chat" | "smalltalk" | "small_talk" | "greeting" | "conversation" | "other" => Intent::Chat,
            "question" | "status" | "status_question" | "query" => Intent::Question,
            "instruction" | "task" | "action" | "command" | "request" | "do" => Intent::Instruction,
            "confirm_yes" | "yes" | "confirm" | "affirm" | "approve" | "approval" | "accept" => Intent::ConfirmYes,
            "confirm_no" | "no" | "deny" | "reject" | "decline" | "refuse" => Intent::ConfirmNo,
            "cancel" | "stop" | "abort" | "halt" => Intent::Cancel,
            "repeat" | "again" | "say_again" => Intent::Repeat,
            _ => return None,
        })
    }
}

/// The model's reading of one utterance.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub kind: Intent,
    /// What to say (chat and questions), short.
    pub reply: String,
    /// The instruction for Opus, cleaned up (falls back to the utterance itself).
    pub instruction: String,
    /// The model's answer couldn't be read and this came from the fallback rules.
    pub fallback: bool,
}

/// What's said when nothing better is known.
pub const DIDNT_CATCH: &str = "Sorry, I didn't catch that. Could you say it again?";

/// Read the model's answer for `utterance`: the first JSON object with a known `kind`, wherever
/// it is (after reasoning, inside a code fence, between words). An answer that can't be read
/// becomes an instruction when the words ask for something to be done, else chat.
pub fn parse_classification(raw: &str, utterance: &str) -> Classification {
    let text = super::local::strip_thinking(raw);
    let found = json_objects(text).into_iter().find_map(|v| {
        let kind = v.get("kind").or_else(|| v.get("type")).or_else(|| v.get("intent")).and_then(Value::as_str).and_then(Intent::parse)?;
        let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
        Some((kind, field("reply"), field("instruction")))
    });
    match found {
        Some((kind, reply, instruction)) => {
            let instruction = if instruction.is_empty() { utterance.trim().to_string() } else { instruction };
            Classification { kind, reply: spoken(&reply, 2, 320), instruction, fallback: false }
        }
        None => fallback(utterance),
    }
}

/// The rules used when the model can't be asked or its answer can't be read: an action request
/// goes to Opus, anything else is chat.
pub fn fallback(utterance: &str) -> Classification {
    let kind = quick_intent(utterance, true, true).unwrap_or(if looks_like_action(utterance) { Intent::Instruction } else { Intent::Chat });
    Classification {
        kind,
        reply: if kind == Intent::Chat { DIDNT_CATCH.to_string() } else { String::new() },
        instruction: utterance.trim().to_string(),
        fallback: true,
    }
}

/// Every JSON object in `text`, in order (strings and nesting respected).
fn json_objects(text: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(off) = text.get(start..).and_then(|t| t.find('{')) {
        let open = start + off;
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        let mut end = None;
        for (i, &b) in bytes.iter().enumerate().skip(open) {
            if in_str {
                match b {
                    _ if escaped => escaped = false,
                    b'\\' => escaped = true,
                    b'"' => in_str = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        match text.get(open..=end).and_then(|s| serde_json::from_str::<Value>(s).ok()) {
            Some(v) => {
                out.push(v);
                start = end + 1;
            }
            None => start = open + 1,
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// Lower-case words of `text`, punctuation dropped.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '\'')).map(|w| w.trim_matches('\'').to_string()).filter(|w| !w.is_empty()).collect()
}

const YES: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "ok",
    "okay",
    "go ahead",
    "do it",
    "approve",
    "approved",
    "confirm",
    "confirmed",
    "allow it",
    "please do",
    "yes please",
    "yes go ahead",
    "yes do it",
    "go for it",
    "affirmative",
    "of course",
];
const NO: &[&str] =
    &["no", "nope", "nah", "no thanks", "no thank you", "don't", "dont", "do not", "deny", "denied", "don't do it", "don't do that", "negative", "no way"];
const CANCEL: &[&str] = &[
    "cancel",
    "cancel it",
    "cancel that",
    "cancel the job",
    "cancel the task",
    "stop",
    "stop it",
    "stop that",
    "stop the job",
    "stop the task",
    "stop working",
    "stop opus",
    "abort",
    "abort it",
    "halt",
];
const REPEAT: &[&str] =
    &["repeat", "repeat that", "say that again", "say it again", "what did you say", "come again", "pardon", "sorry what", "could you repeat that"];

/// The intent of a few short words that never need the model: yes/no only while something waits
/// for an answer (`waiting`), cancel only while a job runs or waits (`working`). Exact phrases
/// only, so a longer sentence always goes to the model.
pub fn quick_intent(text: &str, waiting: bool, working: bool) -> Option<Intent> {
    let phrase = words(text).join(" ");
    let is = |list: &[&str]| list.contains(&phrase.as_str());
    if waiting && is(YES) {
        Some(Intent::ConfirmYes)
    } else if waiting && is(NO) {
        Some(Intent::ConfirmNo)
    } else if working && is(CANCEL) {
        Some(Intent::Cancel)
    } else if is(REPEAT) {
        Some(Intent::Repeat)
    } else {
        None
    }
}

/// Whether `text` clearly says yes: a yes word and no word that takes it back. An approval is
/// only given on this, whatever the model thought (a model can't say yes for the person).
pub fn is_affirmative(text: &str) -> bool {
    let w = words(text);
    let phrase = w.join(" ");
    let negated = w.iter().any(|x| matches!(x.as_str(), "no" | "not" | "don't" | "dont" | "never" | "nope" | "cancel" | "stop" | "wait" | "deny" | "nah"));
    let yes = YES.iter().any(|y| if y.contains(' ') { phrase.contains(y) } else { w.iter().any(|x| x == y) });
    yes && !negated
}

/// A safe answer: one that can only hold something back, so it's taken from any voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeAnswer {
    /// No to what waits (an approval, a question).
    No,
    /// Stop the running job (and say no to what waits).
    Cancel,
}

/// Words that may come before or after a clear no or stop without changing it.
const SAFE_FILLER: &[&str] = &[
    "please",
    "thanks",
    "thank",
    "you",
    "it",
    "that",
    "this",
    "the",
    "now",
    "right",
    "for",
    "just",
    "job",
    "task",
    "opus",
    "claude",
    "craft",
    "talking",
    "speaking",
    "everything",
    "anything",
    "all",
    "of",
    "send",
    "post",
    "do",
    "go",
    "ahead",
    "with",
    "don't",
    "dont",
    "not",
    "no",
    "nope",
    "nah",
    "stop",
    "cancel",
    "abort",
    "halt",
    "never",
    "mind",
    "never-mind",
    "i",
    "said",
    "deny",
    "denied",
    "negative",
    "way",
    "wait",
];

/// Whether `text` is clearly a no, or clearly a cancel or stop, and nothing else: the
/// negative counterpart of [`is_affirmative`]. It must start with a no word (no, nope, nah,
/// don't, deny, never, negative) or a stop word (cancel, stop, abort, halt), carry no yes word,
/// and every other word must be one that can't turn it into a request ("no, don't send it"
/// is a no; "no, delete it instead" isn't). Safe answers are taken even when the speaker check
/// fails or there's no enrolled voice yet: they can only hold something back.
pub fn safe_answer(text: &str) -> Option<SafeAnswer> {
    let w = words(text);
    let first = w.first()?.as_str();
    // Whole words only: "please don't" isn't "please do".
    let padded = format!(" {} ", w.join(" "));
    let yes = YES.iter().any(|y| padded.contains(&format!(" {y} ")));
    if yes || !w.iter().all(|x| SAFE_FILLER.contains(&x.as_str())) {
        return None;
    }
    match first {
        "no" | "nope" | "nah" | "don't" | "dont" | "deny" | "denied" | "never" | "negative" => Some(SafeAnswer::No),
        "cancel" | "stop" | "abort" | "halt" => Some(SafeAnswer::Cancel),
        _ => None,
    }
}

const ACTION_WORDS: &[&str] = &[
    "open", "close", "start", "launch", "run", "make", "create", "build", "write", "edit", "change", "fix", "delete", "remove", "move", "copy", "rename",
    "find", "search", "look", "show", "download", "install", "update", "post", "send", "upload", "publish", "buy", "order", "play", "render", "generate",
    "export", "import", "save", "add", "put", "set", "turn", "check", "test", "deploy", "commit", "push", "email", "share", "convert", "resize", "cut", "trim",
    "merge", "compile", "clean", "organise", "organize", "sort", "list", "go", "navigate", "browse", "draft", "reply", "schedule", "book",
];

/// Whether the words ask for something to be done: an action word first (after please/can you),
/// or "can/could/would you …", "please …", "I want/need you to …".
pub fn looks_like_action(text: &str) -> bool {
    let w = words(text);
    let skip = ["please", "hey", "ok", "okay", "so", "now", "then", "and", "just", "craft", "opus", "claude"];
    let first = w.iter().map(String::as_str).find(|x| !skip.contains(x)).unwrap_or_default();
    let phrase = w.join(" ");
    ACTION_WORDS.contains(&first)
        || ["can you ", "could you ", "would you ", "will you ", "i want you to ", "i need you to ", "i'd like you to ", "go and ", "let's "]
            .iter()
            .any(|p| phrase.starts_with(p) || phrase.contains(&format!(" {p}")))
        || phrase.starts_with("please ")
}

/// `text` made fit to speak: no markup, at most `sentences` sentences and `max` characters.
pub fn spoken(text: &str, sentences: usize, max: usize) -> String {
    let clean: String = text.chars().filter(|c| !matches!(c, '*' | '`' | '#' | '_' | '>' | '|')).collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    let mut count = 0;
    for (i, c) in clean.char_indices() {
        out.push(c);
        let next = clean.get(i + c.len_utf8()..).and_then(|r| r.chars().next());
        if matches!(c, '.' | '!' | '?') && next.is_none_or(char::is_whitespace) {
            count += 1;
            if count >= sentences {
                break;
            }
        }
    }
    if out.chars().count() > max {
        let cut: String = out.chars().take(max).collect();
        let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end_matches([',', ';', ':']);
        return format!("{cut}…");
    }
    out.trim().to_string()
}

// ---- Prompts ----

/// The system prompt for sorting an utterance. The model only talks: it is told it has no tools
/// and that Opus does every task.
pub const CLASSIFY_SYSTEM: &str = r#"You are the voice of Craft Anything, a desktop app. You only talk: you never do tasks yourself and you have no tools. Opus (Claude Code) does every task the user asks for.
Read the user's latest words (from speech recognition, so they may contain mistakes) and answer with ONE JSON object only, no other text:
{"kind": "...", "reply": "...", "instruction": "..."}
kind is one of:
- "chat": small talk or anything that needs no action. reply: a short friendly answer, at most 2 sentences.
- "question": a question you can answer from the conversation or the job status below without doing anything ("what is Opus doing?", "is it done?"). reply: the answer, at most 2 sentences. Never claim something was done unless the status says so.
- "instruction": the user wants something done (open, make, change, find, build, post, send, install, search, fix, play...). instruction: the request as one clear sentence, keeping every name, number and path exactly. reply: "".
- "confirm_yes" or "confirm_no": the user answers yes or no to what is waiting for them.
- "cancel": the user wants the running job stopped.
- "repeat": the user wants your last words again.
If you are unsure whether the user wants something done, choose "instruction". Otherwise, when unsure, choose "chat"."#;

/// The system prompt for one progress sentence.
pub const NARRATE_SYSTEM: &str = "You tell the user, in ONE short spoken sentence of at most 20 words, what a helper called Opus is doing right now, from its recent activity below. Plain words, no code, no long paths, no lists. Answer with the sentence only.";

/// The system prompt for a finished job.
pub const SUMMARISE_SYSTEM: &str = "You tell the user, in at most 2 short spoken sentences, what a helper called Opus did or found, from its final answer below, and whether anything needs the user. Plain words, no code, no long paths, no lists. Answer with the sentences only.";

/// The system prompt for an action Opus asks permission for.
pub const DESCRIBE_SYSTEM: &str = "Describe the action below in plain words, as the end of the sentence \"Opus wants to ...\", in at most 15 words (for example: delete 3 files in Downloads). Be exact about what it changes, sends or deletes. Answer with those words only.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_json_is_found_wherever_it_is() {
        let c = parse_classification(r#"{"kind":"instruction","reply":"","instruction":"Open the trailer folder."}"#, "open the trailer folder");
        assert_eq!((c.kind, c.instruction.as_str(), c.fallback), (Intent::Instruction, "Open the trailer folder.", false));
        let c = parse_classification("<think>hmm {not json}</think>\n```json\n{\"kind\": \"Confirm-Yes\"}\n```", "yes");
        assert_eq!(c.kind, Intent::ConfirmYes);
        assert_eq!(c.instruction, "yes", "an empty instruction falls back to the words");
        let c = parse_classification(r#"Sure! {"kind":"chat","reply":"Hi there! I'm fine. How are you? Lovely day."} hope that helps"#, "hello");
        assert_eq!(c.reply, "Hi there! I'm fine.", "at most two sentences");
        let c = parse_classification(r#"{"note":"x {y}"} {"type":"status","reply":"It's running."}"#, "is it done");
        assert_eq!((c.kind, c.reply.as_str()), (Intent::Question, "It's running."));
    }

    #[test]
    fn an_unreadable_answer_falls_back_by_the_words() {
        for (raw, said, want) in [
            ("", "open the downloads folder", Intent::Instruction),
            ("I think you want me to", "could you post the trailer", Intent::Instruction),
            ("{\"kind\":\"dance\"}", "please render it again", Intent::Instruction),
            ("garbage", "what a nice day", Intent::Chat),
            ("{broken", "how are you", Intent::Chat),
        ] {
            let c = parse_classification(raw, said);
            assert!(c.fallback, "{raw}");
            assert_eq!(c.kind, want, "{said}");
        }
        assert_eq!(fallback("how are you").reply, DIDNT_CATCH);
    }

    #[test]
    fn short_words_need_no_model_only_when_they_make_sense() {
        assert_eq!(quick_intent("Yes.", true, false), Some(Intent::ConfirmYes));
        assert_eq!(quick_intent("Yes.", false, false), None, "nothing waits for a yes");
        assert_eq!(quick_intent("No, thanks!", true, true), Some(Intent::ConfirmNo));
        assert_eq!(quick_intent("Stop it", false, true), Some(Intent::Cancel));
        assert_eq!(quick_intent("stop it", false, false), None, "nothing to stop");
        assert_eq!(quick_intent("Say that again?", false, false), Some(Intent::Repeat));
        assert_eq!(quick_intent("yes and also make it blue", true, true), None, "a sentence goes to the model");
    }

    #[test]
    fn only_a_clear_yes_is_a_yes() {
        for yes in ["Yes", "yeah go ahead", "OK, do it.", "sure", "yes please"] {
            assert!(is_affirmative(yes), "{yes}");
        }
        for no in ["no", "yes, no wait", "don't do it", "okay stop", "I'm not sure", "what is it doing", "", "yesterday's file"] {
            assert!(!is_affirmative(no), "{no}");
        }
    }

    #[test]
    fn spoken_text_is_short_and_plain() {
        assert_eq!(spoken("**Done.** I opened `C:/x`. Then more.", 2, 200), "Done. I opened C:/x.");
        assert_eq!(spoken("Version 1.5 is out. Next.", 1, 200), "Version 1.5 is out.");
        let long = spoken(&"word ".repeat(100), 2, 40);
        assert!(long.chars().count() <= 41 && long.ends_with('…'), "{long}");
    }
}
