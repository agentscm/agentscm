//! Merging what `init-repo` generates into instruction files that may already hold the
//! user's own text. Every function returns the full new text, or the reason the file is left
//! alone.

/// The start of the begin line, which identifies it whatever follows.
const BEGIN_PREFIX: &str = "<!-- agentscm:begin";
const BEGIN: &str = "<!-- agentscm:begin -->";
const END: &str = "<!-- agentscm:end -->";

/// Puts `body` between our markers: replacing the block when present, else appending it.
pub(super) fn block_file(existing: Option<&str>, body: &str) -> Result<String, String> {
    let text = existing.unwrap_or_default();
    let new = format!("{BEGIN}\n{}\n{END}\n", body.trim_end());
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let begin = lines
        .iter()
        .position(|l| l.trim().starts_with(BEGIN_PREFIX));
    let end = lines.iter().position(|l| l.trim() == END);
    match (begin, end) {
        (Some(b), Some(e)) if b < e => Ok(format!(
            "{}{new}{}",
            lines[..b].concat(),
            lines[e + 1..].concat()
        )),
        (None, None) => {
            let mut out = text.to_owned();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            Ok(out + &new)
        }
        _ => Err("its agentscm markers are unbalanced".into()),
    }
}

/// Ensures the file has `line` (an `@AGENTS.md` import), creating it if needed.
pub(super) fn import_file(existing: Option<&str>, line: &str) -> String {
    let text = existing.unwrap_or_default();
    if text.lines().any(|l| l.trim() == line) {
        return text.to_owned();
    }
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out + line + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_replaced_not_duplicated() {
        let once = block_file(Some("# Mine\n"), "one").unwrap();
        assert_eq!(
            once,
            "# Mine\n\n<!-- agentscm:begin -->\none\n<!-- agentscm:end -->\n"
        );
        let twice = block_file(Some(&format!("{once}\nafter\n")), "two").unwrap();
        assert_eq!(
            twice,
            "# Mine\n\n<!-- agentscm:begin -->\ntwo\n<!-- agentscm:end -->\n\nafter\n"
        );
        assert!(block_file(Some("<!-- agentscm:begin -->\n"), "x").is_err());
    }

    #[test]
    fn import_line_is_added_once() {
        assert_eq!(import_file(None, "@AGENTS.md"), "@AGENTS.md\n");
        assert_eq!(import_file(Some("hi"), "@AGENTS.md"), "hi\n@AGENTS.md\n");
        assert_eq!(
            import_file(Some("a\n@AGENTS.md\n"), "@AGENTS.md"),
            "a\n@AGENTS.md\n"
        );
    }
}
