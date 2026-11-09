//! A small, forgiving JavaScript tokenizer.
//!
//! It never fails: unterminated strings, comments and templates just end at the end of input. It
//! serves two jobs: estimating how deeply a source nests before we hand it to the real parser
//! (whose recursion is unbounded), and extracting facts when parsing is refused or fails.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Ident,
    Str,
    /// One chunk of a template literal (between backticks / `${` / `}`).
    Template,
    Number,
    Punct,
    Regex,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokKind,
    /// Raw source text of the token (quotes included for strings).
    pub text: &'a str,
    pub offset: usize,
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=",
    "*=", "/=", "%=", "&=", "|=", "^=", "**", "<<", ">>",
];

const REGEX_AFTER_KEYWORDS: &[&str] = &["return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield", "await"];

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b == b'\\' || b >= 0x80
}

fn is_ident_part(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// Tokenize `src`, stopping after `max_tokens` (the flag says whether it stopped early).
pub fn tokenize(src: &str, max_tokens: usize) -> (Vec<Token<'_>>, bool) {
    let mut out = Vec::new();
    let truncated = scan(src, max_tokens, |t| out.push(t));
    (out, truncated)
}

/// Stream tokens to `emit` without storing them. Returns true when `max_tokens` stopped it.
pub fn scan<'a>(src: &'a str, max_tokens: usize, mut emit: impl FnMut(Token<'a>)) -> bool {
    let b = src.as_bytes();
    let mut count = 0usize;
    let mut i = 0usize;
    // true = this brace opened a template substitution `${`.
    let mut braces: Vec<bool> = Vec::new();
    let slice = |a: usize, z: usize| src.get(a..z).unwrap_or("");
    let mut last_sig: Option<(TokKind, &str)> = None;
    while i < b.len() {
        let c = b.get(i).copied().unwrap_or(0);
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if count >= max_tokens {
            return true;
        }
        count += 1;
        let start = i;
        let next = b.get(i + 1).copied().unwrap_or(0);
        // Comments.
        if c == b'/' && next == b'/' {
            while i < b.len() && b.get(i) != Some(&b'\n') {
                i += 1;
            }
            emit(Token { kind: TokKind::Comment, text: slice(start, i), offset: start });
            continue;
        }
        if c == b'/' && next == b'*' {
            i += 2;
            while i < b.len() && !(b.get(i) == Some(&b'*') && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            emit(Token { kind: TokKind::Comment, text: slice(start, i), offset: start });
            continue;
        }
        let tok_kind;
        if c == b'"' || c == b'\'' {
            i += 1;
            while let Some(&d) = b.get(i) {
                i += 1;
                if d == b'\\' {
                    i += if b.get(i) == Some(&b'\r') && b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                } else if d == c || d == b'\n' {
                    break;
                }
            }
            i = i.min(b.len());
            tok_kind = TokKind::Str;
        } else if c == b'`' || (c == b'}' && braces.last() == Some(&true)) {
            if c == b'}' {
                braces.pop();
            }
            i += 1;
            loop {
                match b.get(i) {
                    None => break,
                    Some(b'\\') => i += 2,
                    Some(b'`') => {
                        i += 1;
                        break;
                    }
                    Some(b'$') if b.get(i + 1) == Some(&b'{') => {
                        i += 2;
                        braces.push(true);
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
            i = i.min(b.len());
            tok_kind = TokKind::Template;
        } else if is_ident_start(c) {
            while b.get(i).is_some_and(|&d| is_ident_part(d)) {
                i += 1;
            }
            tok_kind = TokKind::Ident;
        } else if c.is_ascii_digit() || (c == b'.' && next.is_ascii_digit()) {
            while b.get(i).is_some_and(|&d| d.is_ascii_alphanumeric() || d == b'.' || d == b'_') {
                i += 1;
            }
            tok_kind = TokKind::Number;
        } else if c == b'/' && regex_allowed(last_sig) {
            i += 1;
            let mut in_class = false;
            while let Some(&d) = b.get(i) {
                i += 1;
                match d {
                    b'\\' => i += 1,
                    b'[' => in_class = true,
                    b']' => in_class = false,
                    b'/' if !in_class => break,
                    b'\n' => break,
                    _ => {}
                }
            }
            while b.get(i).is_some_and(|&d| d.is_ascii_alphabetic()) {
                i += 1;
            }
            i = i.min(b.len());
            tok_kind = TokKind::Regex;
        } else {
            let rest = slice(i, (i + 4).min(b.len()));
            let len = PUNCTS.iter().find(|p| rest.starts_with(**p)).map_or(1, |p| p.len());
            // Never split a multi-byte character.
            let mut z = i + len;
            while z < b.len() && !src.is_char_boundary(z) {
                z += 1;
            }
            if c == b'{' {
                braces.push(false);
            } else if c == b'}' {
                braces.pop();
            }
            i = z;
            tok_kind = TokKind::Punct;
        }
        let text = slice(start, i);
        last_sig = Some((tok_kind, text));
        emit(Token { kind: tok_kind, text, offset: start });
    }
    false
}

fn regex_allowed(prev: Option<(TokKind, &str)>) -> bool {
    match prev {
        None => true,
        Some((TokKind::Punct, p)) => !matches!(p, ")" | "]" | "}"),
        Some((TokKind::Ident, w)) => REGEX_AFTER_KEYWORDS.contains(&w),
        Some(_) => false,
    }
}

/// Decode the value of a string token (quotes stripped, common escapes resolved).
pub fn string_value(raw: &str) -> String {
    let inner = raw.get(1..).unwrap_or("");
    let inner = match raw.chars().next() {
        Some(q) if inner.ends_with(q) => inner.get(..inner.len().saturating_sub(1)).unwrap_or(inner),
        _ => inner,
    };
    unescape(inner)
}

/// Resolve JavaScript escape sequences (lenient: unknown escapes keep the character).
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some('0') => out.push('\0'),
            Some('x') => {
                let h: String = it.by_ref().take(2).collect();
                if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some('u') => {
                let hex: String = if it.peek() == Some(&'{') {
                    it.next();
                    it.by_ref().take_while(|c| *c != '}').take(8).collect()
                } else {
                    it.by_ref().take(4).collect()
                };
                let Ok(mut code) = u32::from_str_radix(&hex, 16) else { continue };
                if (0xD800..0xDC00).contains(&code) {
                    let mut ahead = it.clone();
                    if ahead.next() == Some('\\') && ahead.next() == Some('u') {
                        let lo: String = ahead.by_ref().take(4).collect();
                        if let Some(lo) = u32::from_str_radix(&lo, 16).ok().filter(|l| lo.len() == 4 && (0xDC00..0xE000).contains(l)) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                            it = ahead;
                        }
                    }
                }
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            Some('\n') => {}
            Some('\r') => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// An upper-bound style estimate of how deep the syntax tree for `tokens` will nest.
///
/// Brackets add a level; operators that nest expressions (member access, binary and assignment
/// operators, conditionals, arrows, unary prefixes), nesting statement keywords and statement
/// labels (`name:`) add a level until the next `,` or `;` at the same bracket depth. An `else`
/// keeps its level across terminators, because `if … else if … else if …` is one statement that
/// the parser descends into once per `else`, however many `;` it contains. Over-estimates are
/// fine: they only send a file to the lexer fallback.
///
/// Why this matters: the parser recurses once per level with no depth check, and a stack
/// overflow is not a panic but an abort of the whole process. Measured on oxc 0.153 (opt-level 2),
/// the costliest level is about 2.2 KB (`(`, `[`, `{`, `${`, TypeScript generics), a label about
/// 0.5 KB and an `else` about 0.4 KB, against the 64 KB per level that the 256 MB parse stack
/// allows at the 4096-level limit, so every construct that costs the parser a frame must count
/// here at least once.
pub fn nesting_estimate(tokens: &[Token<'_>]) -> usize {
    let mut n = Nesting::default();
    for t in tokens {
        n.feed(t);
    }
    n.max
}

/// Keywords that make the parser descend into what follows them: one frame each, and they chain
/// (`typeof typeof x`, `if (a) if (b) …`, `class extends class extends …`, `keyof keyof T`).
const NESTING_KEYWORDS: &[&str] = &[
    "typeof",
    "void",
    "delete",
    "await",
    "new",
    "if",
    "else",
    "for",
    "while",
    "do",
    "with",
    "in",
    "instanceof",
    "yield",
    "extends",
    "keyof",
    "readonly",
    "infer",
    "unique",
    "asserts",
    "as",
    "satisfies",
];

/// One bracket level of [`Nesting`].
#[derive(Debug, Default, Clone, Copy)]
struct Frame {
    /// Levels the current statement or list item has added; `,` and `;` take them back.
    stmt: usize,
    /// Levels added by `else`: an `if` chain survives the `;` that ends each branch.
    carry: usize,
    /// A `;` or `}` just ended a statement: the carry stays only if `else` comes next.
    pending: bool,
}

/// Streaming state for [`nesting_estimate`].
#[derive(Debug)]
pub struct Nesting {
    stack: Vec<Frame>,
    sum: usize,
    /// The previous non-comment token was an identifier (a label or a keyword prefix candidate).
    prev_ident: bool,
    pub max: usize,
}

impl Default for Nesting {
    fn default() -> Self {
        Nesting { stack: vec![Frame::default()], sum: 0, prev_ident: false, max: 0 }
    }
}

impl Nesting {
    pub fn feed(&mut self, t: &Token<'_>) {
        if t.kind == TokKind::Comment {
            return;
        }
        let is_else = t.kind == TokKind::Ident && t.text == "else";
        let closes = match t.kind {
            TokKind::Punct => matches!(t.text, ")" | "]" | "}"),
            TokKind::Template => t.text.starts_with('}'),
            _ => false,
        };
        let opens = match t.kind {
            TokKind::Punct => matches!(t.text, "(" | "[" | "{"),
            TokKind::Template => t.text.ends_with("${"),
            _ => false,
        };
        // A statement ended just before this token; only `else` continues the `if` chain.
        if !is_else
            && let Some(f) = self.stack.last_mut()
            && f.pending
        {
            self.sum = self.sum.saturating_sub(f.carry);
            f.carry = 0;
            f.pending = false;
        }
        if closes
            && self.stack.len() > 1
            && let Some(f) = self.stack.pop()
        {
            self.sum = self.sum.saturating_sub(f.stmt + f.carry + 1);
            // A block's `}` ends a statement (`if (a) {…}`), after which only `else` may continue
            // it; `)` and `]` end nothing, so they leave the enclosing level's carry alone.
            if t.kind == TokKind::Punct
                && t.text == "}"
                && let Some(outer) = self.stack.last_mut()
            {
                outer.pending = true;
            }
        }
        if opens {
            self.stack.push(Frame::default());
            self.sum += 1;
        }
        if !opens && !closes {
            let nests = match t.kind {
                // `name:` at statement position is a label: the parser recurses into the statement.
                TokKind::Punct => (t.text == ":" && self.prev_ident) || !matches!(t.text, "," | ";" | ":" | "++" | "--"),
                TokKind::Ident => NESTING_KEYWORDS.contains(&t.text),
                _ => false,
            };
            if let Some(f) = self.stack.last_mut() {
                if matches!((t.kind, t.text), (TokKind::Punct, "," | ";")) {
                    self.sum = self.sum.saturating_sub(f.stmt);
                    f.stmt = 0;
                    if t.text == ";" {
                        f.pending = true;
                    }
                } else if is_else {
                    f.carry += 1;
                    f.pending = false;
                    self.sum += 1;
                } else if nests {
                    f.stmt += 1;
                    self.sum += 1;
                }
            }
        }
        self.prev_ident = t.kind == TokKind::Ident;
        self.max = self.max.max(self.sum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(TokKind, String)> {
        tokenize(s, 1000).0.into_iter().map(|t| (t.kind, t.text.to_string())).collect()
    }

    #[test]
    fn whitespace_does_not_count_against_limit() {
        let (t, truncated) = tokenize("a b c d", 4);
        assert_eq!(t.len(), 4);
        assert!(!truncated);
        let (t, truncated) = tokenize("a b c d", 3);
        assert_eq!(t.len(), 3);
        assert!(truncated);
    }

    #[test]
    fn crlf_line_continuation_stays_in_string() {
        let k = kinds("x = 'a\\\r\nb';");
        assert!(k.contains(&(TokKind::Str, "'a\\\r\nb'".into())), "{k:?}");
        assert!(k.contains(&(TokKind::Punct, ";".into())), "{k:?}");
    }

    #[test]
    fn unescape_crlf_line_continuation() {
        assert_eq!(unescape("a\\\r\nb"), "ab");
        assert_eq!(unescape("a\\\rb"), "ab");
        assert_eq!(unescape("a\\\nb"), "ab");
    }

    #[test]
    fn unescape_backspace_formfeed_vtab() {
        assert_eq!(unescape(r"a\fb\vc\bd"), "a\u{c}b\u{b}c\u{8}d");
        assert_eq!(string_value(r#""\b\f\v""#), "\u{8}\u{c}\u{b}");
    }

    #[test]
    fn unescape_combines_surrogate_pairs() {
        assert_eq!(unescape("\\uD83D\\uDE00"), "😀");
        assert_eq!(unescape("a\\uD83D\\u0041"), "a\u{FFFD}A");
        assert_eq!(unescape("\\uDE00x"), "\u{FFFD}x");
        assert_eq!(unescape("\\u{1F600}"), "😀");
    }

    #[test]
    fn tokenizes_strings_templates_regex() {
        let k = kinds(r#"a = "x\"y" + `t${b}u` / 2; r = /a\/b[/]c/g"#);
        assert!(k.contains(&(TokKind::Str, r#""x\"y""#.into())));
        assert!(k.contains(&(TokKind::Template, "`t${".into())));
        assert!(k.contains(&(TokKind::Template, "}u`".into())));
        assert!(k.contains(&(TokKind::Regex, "/a\\/b[/]c/g".into())));
        assert!(k.contains(&(TokKind::Punct, "/".into())));
        assert_eq!(string_value(r#""aA\x42\n""#), "aAB\n");
    }

    #[test]
    fn nesting_grows_with_depth() {
        let shallow = nesting_estimate(&tokenize("a(b, c); d[e];", 100).0);
        let deep_src = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let deep = nesting_estimate(&tokenize(&deep_src, 10_000).0);
        let chain = nesting_estimate(&tokenize(&"a+".repeat(500), 10_000).0);
        let nots = nesting_estimate(&tokenize(&format!("{}x", "!".repeat(400)), 10_000).0);
        assert!(shallow < 5, "{shallow}");
        assert!(deep >= 300 && chain >= 500 && nots >= 400, "{deep} {chain} {nots}");
    }

    #[test]
    fn nesting_counts_labels_and_else_chains() {
        let est = |s: &str| nesting_estimate(&tokenize(s, 10_000_000).0);
        // One parser frame per level, so at least one estimated level each.
        assert!(est(&format!("{}x;", "a:".repeat(1000))) >= 1000);
        assert!(est(&format!("{}c;", "if(a)b;else ".repeat(1000))) >= 1000);
        assert!(est(&format!("{}{{c}}", "if(a){b}else ".repeat(1000))) >= 1000);
        assert!(est(&format!("{}c", "if(a)b\nelse ".repeat(1000))) >= 1000);
        assert!(est(&format!("x={}{}{};", "class extends ".repeat(1000), "{}", "{}".repeat(1000))) >= 1000);
        assert!(est(&format!("type T={}X;", "keyof ".repeat(1000))) >= 1000);
        // Siblings, object keys and conditionals stay flat.
        for flat in ["if(a)b;else c;", "if(a){b};", "a?b:c;", "x={a:1,b:2};", "l:x;"] {
            let d = est(&flat.repeat(1000));
            assert!(d < 8, "{flat}: {d}");
        }
    }

    #[test]
    fn never_panics_on_junk() {
        for s in ["\"", "`${", "/*", "'\\", "}}}}", "`${`${`${", "/[", "é/é/", "\u{1F600}=`\u{1F600}${é}`", "a/**/b//"] {
            let (t, _) = tokenize(s, 100);
            let _ = nesting_estimate(&t);
            for tok in t {
                let _ = string_value(tok.text);
            }
        }
    }
}
