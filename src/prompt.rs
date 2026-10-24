//! Plain-text questions on stdin/stdout. With `--yes` nothing is asked and every
//! question takes the recommended answer.
use std::io::{BufRead, Write};

const MAX_TRIES: usize = 3;

fn parse_confirm(answer: &str, default_yes: bool) -> Option<bool> {
    match answer.to_lowercase().as_str() {
        "" => Some(default_yes),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn ask(mut read: impl FnMut() -> Option<String>, default_yes: bool) -> Option<bool> {
    for _ in 0..MAX_TRIES {
        if let Some(choice) = parse_confirm(&read()?, default_yes) {
            return Some(choice);
        }
        println!("Please answer y or n.");
    }
    None
}

fn pick(
    mut read: impl FnMut() -> Option<String>,
    len: usize,
    default: usize,
    fallback: usize,
) -> usize {
    for _ in 0..MAX_TRIES {
        let Some(answer) = read() else {
            return fallback;
        };
        if answer.is_empty() {
            return default;
        }
        if let Ok(n) = answer.parse::<usize>()
            && (1..=len).contains(&n)
        {
            return n - 1;
        }
        println!("Enter a number from 1 to {len}.");
    }
    fallback
}

pub(crate) struct Ui {
    pub(crate) yes: bool,
}

impl Ui {
    /// `None` at end of input.
    fn read(&self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_owned()),
        }
    }

    /// Only an empty answer takes the default; anything unrecognised is asked again, and
    /// three misses or end of input mean no.
    pub(crate) fn confirm(&self, question: &str, default_yes: bool) -> bool {
        self.yes || self.answer(question, default_yes).unwrap_or(false)
    }

    /// Like `confirm`, but `None` when nobody answered (end of input or three misses), for
    /// choices that must not be recorded without a real answer.
    pub(crate) fn answer(&self, question: &str, default_yes: bool) -> Option<bool> {
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        ask(|| self.read(&format!("{question} {hint} ")), default_yes)
    }

    /// The 0-based index of the chosen option: `default` on Enter, and `fallback` (the
    /// caller's non-writing option) at end of input or after three unusable answers.
    pub(crate) fn choose(
        &self,
        question: &str,
        options: &[&str],
        default: usize,
        fallback: usize,
    ) -> usize {
        if self.yes {
            return default;
        }
        println!("{question}");
        for (i, option) in options.iter().enumerate() {
            println!("  {}) {option}", i + 1);
        }
        let hint = format!("Choose 1-{} [{}]: ", options.len(), default + 1);
        pick(|| self.read(&hint), options.len(), default, fallback)
    }

    /// A free-text answer, prefilled with `default`.
    pub(crate) fn ask(&self, question: &str, default: &str) -> String {
        if self.yes {
            return default.to_owned();
        }
        let hint = if default.is_empty() {
            String::new()
        } else {
            format!(" [{default}]")
        };
        let answer = self
            .read(&format!("{question}{hint}: "))
            .unwrap_or_default();
        if answer.is_empty() {
            default.to_owned()
        } else {
            answer
        }
    }

    /// Waits for Enter; `false` when the user typed `skip` or input has ended.
    pub(crate) fn retry(&self, message: &str) -> bool {
        print!("{message} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
        read > 0 && !line.trim().eq_ignore_ascii_case("skip")
    }

    pub(crate) fn pause(&self, message: &str) {
        if !self.yes {
            let _ = self.read(&format!("{message} "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_confirm;

    #[test]
    fn only_empty_input_takes_the_default() {
        assert_eq!(parse_confirm("", true), Some(true));
        assert_eq!(parse_confirm("", false), Some(false));
        assert_eq!(parse_confirm("Yes", false), Some(true));
        assert_eq!(parse_confirm("n", true), Some(false));
        assert_eq!(parse_confirm("yse", true), None);
    }

    #[test]
    fn no_answer_is_not_a_no() {
        let answers = |list: &[&str]| {
            let mut list: Vec<String> = list.iter().map(|a| a.to_string()).collect();
            list.reverse();
            move || list.pop()
        };
        assert_eq!(super::ask(answers(&[]), true), None);
        assert_eq!(super::ask(answers(&["yse", "x", "?"]), true), None);
        assert_eq!(super::ask(answers(&["yse", "n"]), true), Some(false));
        assert_eq!(super::ask(answers(&[""]), true), Some(true));
    }

    #[test]
    fn no_answer_at_a_menu_takes_the_fallback_not_the_default() {
        let answers = |list: &[&str]| {
            let mut list: Vec<String> = list.iter().map(|a| a.to_string()).collect();
            list.reverse();
            move || list.pop()
        };
        assert_eq!(super::pick(answers(&[]), 3, 0, 1), 1);
        assert_eq!(super::pick(answers(&["x", "9", "0"]), 3, 0, 1), 1);
        assert_eq!(super::pick(answers(&[""]), 3, 0, 1), 0);
        assert_eq!(super::pick(answers(&["x", "3"]), 3, 0, 1), 2);
    }
}
