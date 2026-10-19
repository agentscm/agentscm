//! Gitignore rule parsing and matching.
//!
//! This is original code written from the documented behavior of gitignore patterns. Where the
//! documentation is silent, behavior was measured black-box against the `git` binary and is
//! pinned by the differential tests in `src/workspace.rs`
//! (`selector_matches_git_check_ignore_for_adversarial_patterns` and
//! `seeded_fuzz_patterns_match_git_check_ignore`); comments below name the test row or the
//! comparison that pins each such behavior. A line is parsed into a [`Rule`]; anything this
//! module does not implement is an error, so the scan fails closed.
//!
//! Matching works on bytes. It is a dynamic program over (pattern token, text position), so one
//! evaluation costs at most `tokens x text length` however many stars a pattern holds. Cheap
//! length and suffix checks reject most rules before the program runs, and callers count the
//! program's work against a per-scan budget.

use std::cell::{Cell, RefCell};

/// Longest accepted pattern, in bytes.
const MAX_PATTERN_BYTES: usize = 4096;

#[derive(Debug)]
pub(super) struct Rule {
    pub negated: bool,
    /// The line as written, minus the trailing spaces that are ignored; for reports.
    pub original: String,
    dir_only: bool,
    /// The pattern has a `/` other than a trailing one: it matches the whole path relative to
    /// its rule file's directory. Otherwise it matches the last path component only.
    anchored: bool,
    ignore_case: bool,
    tokens: Vec<Token>,
    /// The fewest bytes a matching subject can have.
    min_len: usize,
    /// Literal bytes (case-folded) every matching subject must end with.
    suffix: Vec<u8>,
}

#[derive(Debug)]
enum Token {
    /// One byte; the text byte is lower-cased before comparing when ignoring case.
    Literal(u8),
    /// `?`: any one byte except `/`.
    Any,
    /// `*`: any run of bytes except `/`.
    Star,
    /// `**/` between components: zero or more whole components (each with its `/`).
    Components,
    /// A final `**` after `/` (or alone): any run of bytes, `/` included, possibly empty.
    Remainder,
    /// A bracket class, as a table over all byte values; never contains `/`.
    Class(ClassTable),
}

/// `Ok(None)` for a line that holds no rule: blank, a comment, or a pattern that can never match
/// (a trailing backslash, an unclosed class; rows `k[a`, `k[` and `k\\` of the adversarial table).
/// `Err` for a line this module refuses to interpret.
pub(super) fn parse_line(line: &str, ignore_case: bool) -> Result<Option<Rule>, ()> {
    if line.as_bytes().contains(&0) {
        return Err(());
    }
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let original = strip_trailing_spaces(line);
    let mut bytes = original.as_bytes();
    let negated = bytes.first() == Some(&b'!');
    if negated {
        bytes = &bytes[1..];
    }
    let dir_only = bytes.last() == Some(&b'/');
    if dir_only {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_PATTERN_BYTES {
        return Err(());
    }
    let anchored = bytes.contains(&b'/');
    if bytes.first() == Some(&b'/') {
        bytes = &bytes[1..];
        if bytes.is_empty() {
            return Ok(None);
        }
    }
    let Some(tokens) = tokenize(bytes, ignore_case)? else {
        return Ok(None);
    };
    let min_len = tokens
        .iter()
        .filter(|token| matches!(token, Token::Literal(_) | Token::Any | Token::Class(_)))
        .count();
    let mut suffix: Vec<u8> = tokens
        .iter()
        .rev()
        .map_while(|token| match token {
            Token::Literal(byte) => Some(*byte),
            _ => None,
        })
        .collect();
    suffix.reverse();
    Ok(Some(Rule {
        negated,
        original: original.to_owned(),
        dir_only,
        anchored,
        ignore_case,
        tokens,
        min_len,
        suffix,
    }))
}

impl Rule {
    /// Whether the rule matches `path` (relative to its rule file's directory, `/`-separated);
    /// `basename` is its last component, computed once per path by the caller. `work` is charged
    /// for everything done: one unit per rule considered plus the suffix comparison, and the
    /// dynamic program's cost, tokens times subject length.
    pub(super) fn matches_counted(
        &self,
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
        work: &Cell<u64>,
    ) -> bool {
        let charge = |units: u64| work.set(work.get().saturating_add(units));
        charge(1);
        if self.dir_only && !is_dir {
            return false;
        }
        let subject = if self.anchored { path } else { basename };
        // Sound pre-checks: every match consumes at least `min_len` bytes and ends with the
        // pattern's trailing literals.
        if subject.len() < self.min_len || subject.len() < self.suffix.len() {
            return false;
        }
        charge(self.suffix.len() as u64);
        let tail = &subject[subject.len() - self.suffix.len()..];
        if !tail
            .iter()
            .zip(&self.suffix)
            .all(|(&byte, &expected)| fold(byte, self.ignore_case) == expected)
        {
            return false;
        }
        charge(self.tokens.len() as u64 * (subject.len() as u64 + 1));
        run(&self.tokens, subject, self.ignore_case)
    }

    #[cfg(test)]
    pub(super) fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        self.matches_counted(path, basename(path), is_dir, &Cell::new(0))
    }
}

/// The last `/`-separated component of `path`.
pub(super) fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|&byte| byte == b'/').next().unwrap_or(path)
}

/// Trailing spaces are ignored unless a backslash escapes them (rows `a `, `a\\ ` and `a\\\\ `
/// of `selection_matches_git_for_trailing_whitespace_rules`).
fn strip_trailing_spaces(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b' ' {
        let backslashes = bytes[..end - 1]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

fn fold(byte: u8, ignore_case: bool) -> u8 {
    if ignore_case {
        byte.to_ascii_lowercase()
    } else {
        byte
    }
}

fn tokenize(pattern: &[u8], ignore_case: bool) -> Result<Option<Vec<Token>>, ()> {
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < pattern.len() {
        match pattern[at] {
            b'\\' => {
                // A trailing backslash can never match anything.
                let Some(&escaped) = pattern.get(at + 1) else {
                    return Ok(None);
                };
                // Ignoring case, the text is lower-cased but an escaped byte is kept as written,
                // so `\\A` matches nothing (the fuzz comparison in
                // `seeded_fuzz_patterns_match_git_check_ignore` pins this).
                tokens.push(Token::Literal(escaped));
                at += 2;
            }
            b'?' => {
                tokens.push(Token::Any);
                at += 1;
            }
            b'*' => {
                let run = pattern[at..]
                    .iter()
                    .take_while(|&&byte| byte == b'*')
                    .count();
                let after = at + run;
                // A run of two or more stars is accepted only as exactly `**` forming a whole path segment: the
                // pattern's start or a `/` before it, and the pattern's end or a `/` after it.
                // Other runs read differently across Git versions, so they are refused (rows
                // `a**/b.key`, `a/***/b.key`, `***`, `k**`, `**k` of the adversarial table).
                let whole_segment = (at == 0 || pattern[at - 1] == b'/')
                    && (after == pattern.len() || pattern[after] == b'/');
                if run >= 2 && (run > 2 || !whole_segment) {
                    return Err(());
                }
                if run == 1 {
                    tokens.push(Token::Star);
                } else if pattern.get(after) == Some(&b'/') {
                    // `**/` spans zero or more whole components.
                    tokens.push(Token::Components);
                    at = after + 1;
                    continue;
                } else {
                    tokens.push(Token::Remainder);
                }
                at = after;
            }
            b'[' => match parse_class(pattern, at, ignore_case)? {
                Some((class, next)) => {
                    tokens.push(Token::Class(class));
                    at = next;
                }
                // An unclosed class (or an unknown POSIX name) never matches.
                None => return Ok(None),
            },
            byte => {
                tokens.push(Token::Literal(fold(byte, ignore_case)));
                at += 1;
            }
        }
    }
    Ok(Some(tokens))
}

/// Which byte values a class accepts.
type ClassTable = Box<[bool; 256]>;

enum Item {
    Byte(u8),
    Range(u8, u8),
    Posix(&'static str),
}

/// Parse the class opened at `pattern[open]`. `Ok(None)` means it can never match; `Err` means
/// it uses something this module refuses (a non-ASCII range end).
fn parse_class(
    pattern: &[u8],
    open: usize,
    ignore_case: bool,
) -> Result<Option<(ClassTable, usize)>, ()> {
    let mut at = open + 1;
    let negated = matches!(pattern.get(at), Some(b'!' | b'^'));
    if negated {
        at += 1;
    }
    let mut items: Vec<Item> = Vec::new();
    // A literal that a following `-` may extend into a range.
    let mut range_start: Option<u8> = None;
    let mut first = true;
    loop {
        let Some(&byte) = pattern.get(at) else {
            return Ok(None);
        };
        if byte == b']' && !first {
            at += 1;
            break;
        }
        first = false;
        if byte == b'\\' {
            let Some(&escaped) = pattern.get(at + 1) else {
                return Ok(None);
            };
            items.push(Item::Byte(escaped));
            range_start = Some(escaped);
            at += 2;
        } else if byte == b'-'
            && range_start.is_some()
            && pattern.get(at + 1).is_some_and(|&next| next != b']')
        {
            let start = range_start.take().expect("checked above");
            let mut end_at = at + 1;
            if pattern[end_at] == b'\\' {
                end_at += 1;
            }
            let Some(&end) = pattern.get(end_at) else {
                return Ok(None);
            };
            if !end.is_ascii() || !start.is_ascii() {
                return Err(());
            }
            items.push(Item::Range(start, end));
            at = end_at + 1;
        } else if byte == b'[' && pattern.get(at + 1) == Some(&b':') {
            let name_start = at + 2;
            let Some(close) = pattern[name_start..]
                .iter()
                .position(|&b| b == b']')
                .map(|offset| name_start + offset)
            else {
                return Ok(None);
            };
            if close > name_start && pattern[close - 1] == b':' {
                let Some(name) = posix_name(&pattern[name_start..close - 1]) else {
                    return Ok(None);
                };
                items.push(Item::Posix(name));
                range_start = None;
                at = close + 1;
            } else {
                // Not a POSIX class: an ordinary `[`.
                items.push(Item::Byte(b'['));
                range_start = Some(b'[');
                at += 1;
            }
        } else {
            items.push(Item::Byte(byte));
            range_start = Some(byte);
            at += 1;
        }
    }
    let mut table = Box::new([false; 256]);
    for value in 0..=255u8 {
        let matched = items
            .iter()
            .any(|item| item_matches(item, value, ignore_case));
        table[usize::from(value)] = matched != negated && value != b'/';
    }
    Ok(Some((table, at)))
}

/// Whether one class item accepts `text`. Ignoring case, the text byte is compared lower-cased
/// against single members, and both its cases are tried against range ends; the `upper` and
/// `lower` classes both accept any letter. Measured, and pinned by the ignorecase rows
/// (`k[A].key`, `k[!A].key`, `[A-C]`) of the adversarial table and the fuzz comparison.
fn item_matches(item: &Item, text: u8, ignore_case: bool) -> bool {
    let lower = text.to_ascii_lowercase();
    let upper = text.to_ascii_uppercase();
    match *item {
        Item::Byte(member) => {
            if ignore_case {
                lower == member
            } else {
                text == member
            }
        }
        Item::Range(low, high) => {
            let within = |byte: u8| low <= byte && byte <= high;
            if ignore_case {
                within(lower) || within(upper)
            } else {
                within(text)
            }
        }
        Item::Posix(name) => {
            let probe = if ignore_case { lower } else { text };
            match name {
                "alnum" => probe.is_ascii_alphanumeric(),
                "alpha" => probe.is_ascii_alphabetic(),
                "blank" => probe == b' ' || probe == b'\t',
                "cntrl" => probe.is_ascii_control(),
                "digit" => probe.is_ascii_digit(),
                "graph" => probe.is_ascii_graphic(),
                "lower" => {
                    if ignore_case {
                        text.is_ascii_alphabetic()
                    } else {
                        text.is_ascii_lowercase()
                    }
                }
                "print" => probe.is_ascii_graphic() || probe == b' ',
                "punct" => probe.is_ascii_punctuation(),
                "space" => matches!(probe, b' ' | b'\t' | b'\n' | b'\r'),
                "upper" => {
                    if ignore_case {
                        text.is_ascii_alphabetic()
                    } else {
                        text.is_ascii_uppercase()
                    }
                }
                "xdigit" => probe.is_ascii_hexdigit(),
                _ => false,
            }
        }
    }
}

fn posix_name(name: &[u8]) -> Option<&'static str> {
    [
        "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
        "upper", "xdigit",
    ]
    .into_iter()
    .find(|candidate| candidate.as_bytes() == name)
}

thread_local! {
    /// Reach sets reused across evaluations, so matching allocates nothing per call.
    static SCRATCH: RefCell<(Vec<bool>, Vec<bool>)> = const { RefCell::new((Vec::new(), Vec::new())) };
}

/// Whether `tokens` match all of `text`. `reach[i]` says the tokens so far can consume exactly
/// `text[..i]`; each token maps one reach set to the next in a single pass.
fn run(tokens: &[Token], text: &[u8], ignore_case: bool) -> bool {
    SCRATCH.with(|scratch| {
        let (reach, next) = &mut *scratch.borrow_mut();
        walk(tokens, text, ignore_case, reach, next)
    })
}

fn walk(
    tokens: &[Token],
    text: &[u8],
    ignore_case: bool,
    reach: &mut Vec<bool>,
    next: &mut Vec<bool>,
) -> bool {
    let length = text.len();
    reach.clear();
    reach.resize(length + 1, false);
    next.clear();
    next.resize(length + 1, false);
    reach[0] = true;
    for token in tokens {
        next.fill(false);
        match token {
            Token::Literal(byte) => {
                for at in 0..length {
                    next[at + 1] = reach[at] && fold(text[at], ignore_case) == *byte;
                }
            }
            Token::Any => {
                for at in 0..length {
                    next[at + 1] = reach[at] && text[at] != b'/';
                }
            }
            Token::Class(table) => {
                for at in 0..length {
                    next[at + 1] = reach[at] && table[usize::from(text[at])];
                }
            }
            Token::Star => {
                let mut open = false;
                for at in 0..=length {
                    open |= reach[at];
                    next[at] = open;
                    if at < length && text[at] == b'/' {
                        open = false;
                    }
                }
            }
            Token::Components => {
                let mut seen = false;
                for at in 0..=length {
                    next[at] = reach[at] || (seen && at > 0 && text[at - 1] == b'/');
                    seen |= reach[at];
                }
            }
            Token::Remainder => {
                let mut open = false;
                for at in 0..=length {
                    open |= reach[at];
                    next[at] = open;
                }
            }
        }
        std::mem::swap(reach, next);
        if !reach.iter().any(|&reached| reached) {
            return false;
        }
    }
    reach[length]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(line: &str) -> Rule {
        parse_line(line, false).unwrap().unwrap()
    }

    #[test]
    fn parses_negation_directory_only_and_anchoring() {
        let r = rule("!build/");
        assert!(r.negated && r.dir_only && !r.anchored);
        assert!(rule("/a").anchored && rule("a/b").anchored && !rule("a").anchored);
        assert!(parse_line("# c", false).unwrap().is_none());
        assert!(parse_line("", false).unwrap().is_none());
        assert!(parse_line("a\0b", false).is_err());
        assert!(parse_line("foo\\", false).unwrap().is_none());
        assert_eq!(rule("a  ").original, "a");
        assert_eq!(rule("a\\ ").original, "a\\ ");
    }

    #[test]
    fn double_star_before_an_escaped_slash_is_refused() {
        assert!(parse_line("a/**\\/b", false).is_err());
        assert!(parse_line("**\\/b", false).is_err());
        assert!(parse_line("a/**/b", false).is_ok());
    }

    #[test]
    fn suffix_and_length_checks_never_reject_a_match() {
        let work = Cell::new(0);
        let r = rule("*.rs");
        assert!(r.matches_counted(b"dir/main.rs", basename(b"dir/main.rs"), false, &work));
        assert!(!r.matches_counted(b"dir/main.py", basename(b"dir/main.py"), false, &work));
        assert!(!rule("???").matches_counted(b"ab", basename(b"ab"), false, &work));
        let folded = parse_line("*.RS", true).unwrap().unwrap();
        assert!(folded.matches_counted(b"x.rs", basename(b"x.rs"), false, &work));
    }

    #[test]
    fn matches_components_and_stars() {
        assert!(rule("*.key").matches(b"sub/x.key", false));
        assert!(!rule("/*.key").matches(b"sub/x.key", false));
        assert!(rule("a/**/b").matches(b"a/b", false));
        assert!(rule("a/**/b").matches(b"a/x/y/b", false));
        assert!(rule("a/**").matches(b"a/x/y", false));
        assert!(!rule("a/**").matches(b"a", false));
        assert!(parse_line("a**/**", false).is_err());
        assert!(rule("**/**").matches(b"a", false));
        assert!(rule("foo/").matches(b"x/foo", true));
        assert!(!rule("foo/").matches(b"x/foo", false));
        assert!(!rule("a?b").matches(b"a/b", false));
    }

    #[test]
    fn pathological_star_runs_finish_quickly() {
        let started = std::time::Instant::now();
        let nested = rule(&format!("{}z", "**/*a/".repeat(40)));
        let path = "a/".repeat(500) + "b";
        assert!(!nested.matches(path.as_bytes(), false));
        let wide = rule(&format!("{}b", "*a".repeat(300)));
        assert!(wide.matches(format!("{}b", "a".repeat(2000)).as_bytes(), false));
        let mixed = rule(&format!("{}x", "**/*/".repeat(60)));
        assert!(!mixed.matches("d/".repeat(1000).as_bytes(), false));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
