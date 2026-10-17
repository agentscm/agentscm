//! Plain-text questions on stdin/stdout. With `--yes` nothing is asked and every
//! question takes the recommended answer.
use std::io::{BufRead, Write};

pub(crate) struct Ui {
    pub(crate) yes: bool,
}

impl Ui {
    fn read(&self, prompt: &str) -> String {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        // End of input answers like Enter: the default.
        let _ = std::io::stdin().lock().read_line(&mut line);
        line.trim().to_owned()
    }

    pub(crate) fn confirm(&self, question: &str, default_yes: bool) -> bool {
        if self.yes {
            return true;
        }
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        match self
            .read(&format!("{question} {hint} "))
            .to_lowercase()
            .as_str()
        {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default_yes,
        }
    }

    /// The 0-based index of the chosen option; `default` on Enter or an unknown answer.
    pub(crate) fn choose(&self, question: &str, options: &[&str], default: usize) -> usize {
        if self.yes {
            return default;
        }
        println!("{question}");
        for (i, option) in options.iter().enumerate() {
            println!("  {}) {option}", i + 1);
        }
        let answer = self.read(&format!("Choose 1-{} [{}]: ", options.len(), default + 1));
        match answer.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => n - 1,
            _ => default,
        }
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
        let answer = self.read(&format!("{question}{hint}: "));
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
            self.read(&format!("{message} "));
        }
    }
}
