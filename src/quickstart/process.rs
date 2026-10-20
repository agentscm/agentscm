//! Bounded subprocess execution for local service and Docker control commands.
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) timed_out: bool,
}

fn drain<R: Read>(mut reader: R) -> (Vec<u8>, bool) {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = OUTPUT_LIMIT.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(remaining)]);
                truncated |= read > remaining;
            }
        }
    }
    (retained, truncated)
}

pub(crate) fn run(command: &mut Command, deadline: Duration) -> Result<Output, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("cannot capture command stdout")?;
    let stderr = child.stderr.take().ok_or("cannot capture command stderr")?;
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = stdout_tx.send(drain(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_tx.send(drain(stderr));
    });
    #[cfg(unix)]
    let process_group = child.id();
    #[cfg(unix)]
    let kill_group = || {
        // SAFETY: the child was placed in a fresh process group whose id is its positive pid.
        // A negative id addresses only that group. ESRCH is harmless after every member exits.
        let _ = unsafe { libc::kill(-(process_group as i32), libc::SIGKILL) };
    };
    let end = Instant::now() + deadline;
    let (status, timed_out) = loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => break (status, false),
            None if Instant::now() >= end => {
                #[cfg(unix)]
                kill_group();
                let _ = child.kill();
                break (child.wait().map_err(|error| error.to_string())?, true);
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    };
    // A completed command must not leave descendants holding the capture pipes open forever.
    #[cfg(unix)]
    kill_group();
    let drain_end = Instant::now() + Duration::from_secs(1);
    let receive = |receiver: mpsc::Receiver<(Vec<u8>, bool)>| {
        receiver
            .recv_timeout(drain_end.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| (Vec::new(), true))
    };
    let render = |(bytes, truncated): (Vec<u8>, bool)| {
        let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if truncated {
            text.push_str("\n[output truncated]");
        }
        text
    };
    Ok(Output {
        success: status.success() && !timed_out,
        code: status.code(),
        stdout: render(receive(stdout_rx)),
        stderr: render(receive(stderr_rx)),
        timed_out,
    })
}

pub(crate) fn diagnostic(output: &Output) -> String {
    let text = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let text = text.lines().take(8).collect::<Vec<_>>().join("\n");
    if output.timed_out {
        format!(
            "command timed out{}",
            if text.is_empty() {
                String::new()
            } else {
                format!(": {text}")
            }
        )
    } else {
        text
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn drains_both_pipes_and_bounds_retained_output() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "yes x | head -n 200000 & (yes y | head -n 200000) >&2; wait",
        ]);
        let output = run(&mut command, Duration::from_secs(5)).unwrap();
        assert!(output.success);
        assert!(output.stdout.len() <= OUTPUT_LIMIT + 32);
        assert!(output.stderr.len() <= OUTPUT_LIMIT + 32);
        assert!(output.stdout.contains("output truncated"));
        assert!(output.stderr.contains("output truncated"));
    }

    #[test]
    fn kills_and_reaps_at_the_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let output = run(&mut command, Duration::from_millis(50)).unwrap();
        assert!(output.timed_out && !output.success);
    }

    #[test]
    fn completed_parent_cannot_leave_drain_pipes_open() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 60 & exit 0"]);
        let output = run(&mut command, Duration::from_secs(1)).unwrap();
        assert!(output.success);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
