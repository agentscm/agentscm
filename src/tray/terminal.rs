//! Linux terminal companion when this desktop has no qualified tray host.

use std::io::{Read, Write};
use std::sync::mpsc;

use agentscm::{Error, Result};

use super::companion_launch::{self, Agent, Event, OptionSet, Request};

const MAX_INPUT: usize = 16 * 1024;

pub(super) async fn run() -> Result<()> {
    let snapshot = crate::client_status::snapshot_for(true, None).await?;
    if snapshot.connections.is_empty() {
        return Err(Error::Invalid(
            "no connected checkouts; run `agentscm client setup` in a checkout first".into(),
        ));
    }
    println!("AgentSCM Companion · local terminal view");
    println!("Saved checkouts and reported participation:");
    for (index, connection) in snapshot.connections.iter().enumerate() {
        println!("  {}. {}", index + 1, connection.repository);
        if let Some(attention) = crate::client_cli::attention(&connection.root) {
            println!("     Needs attention: {attention}");
        }
        for row in connection.recent_participation.iter().take(3) {
            println!(
                "     {}: {} (reported by Hub; live task state unknown)",
                row.agent, row.state
            );
        }
    }
    println!("Prompting is optional; press Enter without a choice to leave.");
    let mut input = std::io::stdin().lock();
    let selected = ask(&mut input, "Checkout number: ", 8)?;
    if selected.is_empty() {
        return Ok(());
    }
    let index: usize = selected
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .filter(|index| *index < snapshot.connections.len())
        .ok_or_else(|| Error::Invalid("invalid checkout selection".into()))?;
    let agent = match ask(&mut input, "Agent [1 Claude Code, 2 Codex CLI]: ", 8)?.as_str() {
        "1" => Agent::ClaudeCode,
        "2" => Agent::Codex,
        _ => return Err(Error::Invalid("invalid agent selection".into())),
    };
    let option = match agent {
        Agent::ClaudeCode => {
            match ask(&mut input, "Mode [1 native defaults, 2 plan]: ", 8)?.as_str() {
                "1" => OptionSet::NativeDefault,
                "2" => OptionSet::Plan,
                _ => return Err(Error::Invalid("invalid mode selection".into())),
            }
        }
        Agent::Codex => {
            match ask(&mut input, "Mode [1 native defaults, 2 read only]: ", 8)?.as_str() {
                "1" => OptionSet::NativeDefault,
                "2" => OptionSet::ReadOnly,
                _ => return Err(Error::Invalid("invalid mode selection".into())),
            }
        }
    };
    let prompt = ask(&mut input, "Prompt (one line): ", MAX_INPUT)?;
    let request = Request {
        root: snapshot.connections[index].root.clone(),
        agent,
        option,
        prompt,
    };
    let (sender, receiver) = mpsc::channel();
    companion_launch::run(request, move |event| {
        let _ = sender.send(event);
    });
    while let Ok(event) = receiver.recv() {
        match event {
            Event::Started(pid) => {
                println!(
                    "{} client process {pid} started; no result is inferred from hook activity.",
                    agent.label()
                );
            }
            Event::Exited {
                success,
                session,
                preview,
            } => {
                println!(
                    "Client exited {}. Task outcome is unverified; inspect the native client session and your checkout.",
                    if success {
                        "with status 0"
                    } else {
                        "with an error"
                    }
                );
                if let Some(preview) = preview {
                    println!("Bounded native response preview: {preview}");
                }
                if let Some(session) = session {
                    println!(
                        "Resume in this checkout: {} {session}",
                        match agent {
                            Agent::ClaudeCode => "claude --resume",
                            Agent::Codex => "codex resume",
                        }
                    );
                }
                break;
            }
            Event::Failed(message) => return Err(Error::Invalid(message)),
        }
    }
    Ok(())
}

fn ask(input: &mut impl Read, prompt: &str, max: usize) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        if input.read(&mut byte)? == 0 || byte[0] == b'\n' {
            break;
        }
        if bytes.len() == max {
            return Err(Error::Invalid("input is too long".into()));
        }
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).map_err(|_| Error::Invalid("input is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_terminal_input_rejects_long_lines() {
        assert!(ask(&mut "12345\n".as_bytes(), "", 4).is_err());
        assert_eq!(ask(&mut "abc\n".as_bytes(), "", 4).unwrap(), "abc");
    }
}
