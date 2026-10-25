//! Bounded subprocess execution for local service and Docker control commands.
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
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
    pub(crate) truncated: bool,
    /// A stream did not reach EOF.
    pub(crate) incomplete: bool,
    pub(crate) stdout_valid_utf8: bool,
}

#[derive(Default)]
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
    complete: bool,
}

fn drain<R: Read>(mut reader: R) -> CapturedStream {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                return CapturedStream {
                    bytes: retained,
                    truncated,
                    complete: true,
                };
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(read) => {
                let remaining = OUTPUT_LIMIT.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(remaining)]);
                truncated |= read > remaining;
            }
        }
    }
    CapturedStream {
        bytes: retained,
        truncated,
        complete: false,
    }
}

#[cfg(unix)]
fn child_exited_without_reaping(child: &Child) -> io::Result<bool> {
    // SAFETY: zero is a valid empty siginfo_t; waitid initializes it on an event.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: info is writable. WNOWAIT retains the child's PID reservation until
    // cleanup has signalled its process group and Child::wait reaps the leader.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info.si_signo == libc::SIGCHLD)
    }
}

#[cfg(unix)]
fn signal_owned_group(child: &mut Child) -> io::Result<()> {
    // Recheck ownership immediately before signalling, including after an exit
    // observation. ECHILD means the PID reservation is gone: never signal it.
    loop {
        match child_exited_without_reaping(child) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    #[cfg(test)]
    GROUP_SIGNAL_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    loop {
        // SAFETY: the unreaped child owns the fresh process group with its positive
        // PID; a negative PID addresses only that group. ESRCH means it is empty.
        if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            _ if error.raw_os_error() == Some(libc::ESRCH) => break,
            #[cfg(target_os = "macos")]
            _ if error.raw_os_error() == Some(libc::EPERM)
                && exited_leader_is_only_group_member(child)? =>
            {
                break;
            }
            _ => return Err(error),
        }
    }
    loop {
        match child.kill() {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn exited_leader_is_only_group_member(child: &Child) -> io::Result<bool> {
    loop {
        match child_exited_without_reaping(child) {
            Ok(true) => break,
            Ok(false) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    // Darwin's group signal skips zombies and reports EPERM when none remain
    // eligible. Accept that case only when the owned, exited leader is the sole
    // member; EPERM for any group with another member remains a cleanup failure.
    let mut members = [0 as libc::pid_t; 2];
    // SAFETY: members is writable for the supplied byte length. Two slots suffice
    // to distinguish the sole leader from a group containing any other member.
    let count = unsafe {
        libc::proc_listpgrppids(
            child.id() as libc::pid_t,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as libc::c_int,
        )
    };
    Ok(count == 1 && members[0] == child.id() as libc::pid_t)
}

fn reap(child: &mut Child) -> io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn wait_with_cleanup(child: &mut Child, deadline: Duration) -> io::Result<(ExitStatus, bool)> {
    let end = Instant::now() + deadline;
    loop {
        #[cfg(unix)]
        match child_exited_without_reaping(child) {
            Ok(exited) if exited || Instant::now() >= end => {
                // Kill descendants holding capture pipes before releasing the
                // leader's PID. No process-group signal is sent after reaping.
                signal_owned_group(child)?;
                return reap(child).map(|status| (status, !exited));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        #[cfg(not(unix))]
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, false)),
            Ok(None) if Instant::now() >= end => {
                let _ = child.kill();
                return reap(child).map(|status| (status, true));
            }
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        thread::sleep(Duration::from_millis(25));
    }
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
    let (status, timed_out) =
        wait_with_cleanup(&mut child, deadline).map_err(|error| error.to_string())?;
    let drain_end = Instant::now() + Duration::from_secs(1);
    let receive = |receiver: mpsc::Receiver<CapturedStream>| {
        receiver
            .recv_timeout(drain_end.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    let render = |stream: &CapturedStream| {
        let mut text = String::from_utf8_lossy(&stream.bytes).trim().to_owned();
        if stream.truncated || !stream.complete {
            text.push_str("\n[output truncated]");
        }
        text
    };
    let stdout = receive(stdout_rx);
    let stderr = receive(stderr_rx);
    Ok(Output {
        success: status.success() && !timed_out,
        code: status.code(),
        stdout: render(&stdout),
        stderr: render(&stderr),
        timed_out,
        truncated: stdout.truncated || stderr.truncated,
        incomplete: !stdout.complete || !stderr.complete,
        stdout_valid_utf8: std::str::from_utf8(&stdout.bytes).is_ok(),
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
thread_local! {
    static GROUP_SIGNAL_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

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
        assert!(output.truncated);
        assert!(!output.incomplete);
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
        assert!(!output.incomplete);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn normal_cleanup_signals_before_reaping_the_leader() {
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| attempts.set(0));
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let (status, timed_out) = wait_with_cleanup(&mut child, Duration::from_secs(5)).unwrap();
        assert!(status.success() && !timed_out);
        // signal_owned_group can increment only after WNOWAIT proves ownership;
        // after the supervisor returns, its wait must have released that ownership.
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 1));
        assert_eq!(
            child_exited_without_reaping(&child)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn externally_reaped_child_never_receives_a_group_signal() {
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| attempts.set(0));
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut status = 0;
        loop {
            // SAFETY: this test owns the exact Child and deliberately reaps it
            // outside Child to reproduce the supervisor's lost-ownership case.
            let result = unsafe { libc::waitpid(pid, &mut status, 0) };
            if result == pid {
                break;
            }
            let error = io::Error::last_os_error();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
        assert_eq!(
            signal_owned_group(&mut child).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(
            wait_with_cleanup(&mut child, Duration::from_secs(1))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zombie_group_exception_rejects_a_surviving_descendant() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 60 & exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !child_exited_without_reaping(&child).unwrap() {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(25));
        }
        let sole_leader = exited_leader_is_only_group_member(&child).unwrap();
        // Clean up before asserting so an unexpected inventory cannot leak sleep.
        signal_owned_group(&mut child).unwrap();
        assert!(reap(&mut child).unwrap().success());
        assert!(!sole_leader);
    }

    #[test]
    fn stream_read_errors_preserve_incomplete_metadata() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("synthetic read failure"))
            }
        }
        let stream = drain(FailingReader);
        assert!(!stream.complete);
        assert!(!stream.truncated);
    }
}
