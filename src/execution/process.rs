use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::Cancellation;

pub(crate) struct Capture {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stop: Option<Stop>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Timeout,
    Cancelled,
    Output,
    Io,
}

const INPUT_CHUNK: usize = 16 * 1024;

struct InputPump {
    stream: UnixStream,
    bytes: Vec<u8>,
    offset: usize,
}

enum InputProgress {
    Pending,
    Complete,
    Failed,
}

impl InputPump {
    fn write_chunk(&mut self, chunk: usize) -> InputProgress {
        let end = self.offset.saturating_add(chunk).min(self.bytes.len());
        match self.stream.write(&self.bytes[self.offset..end]) {
            Ok(0) => InputProgress::Failed,
            Ok(count) => {
                self.offset += count;
                if self.offset == self.bytes.len() {
                    InputProgress::Complete
                } else {
                    InputProgress::Pending
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                InputProgress::Pending
            }
            Err(_) => InputProgress::Failed,
        }
    }
}

// The executable is an operator-selected trusted Docker CLI, never candidate code.
// Both streams share one retained-byte budget; excess bytes trigger termination.
pub(super) fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: u64,
    cancel: &Cancellation,
) -> std::io::Result<Capture> {
    capture_with_input(command, None, timeout, limit, cancel)
}

// `input` is submitted incrementally alongside output collection so neither
// direction can block the other. Stdin is null when input is absent.
pub(crate) fn capture_with_input(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: u64,
    cancel: &Cancellation,
) -> std::io::Result<Capture> {
    capture_with_input_inner(
        command,
        input,
        timeout,
        limit,
        cancel,
        INPUT_CHUNK,
        |_, _| {},
    )
}

fn capture_with_input_inner(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: u64,
    cancel: &Cancellation,
    input_chunk: usize,
    inspect_input_pair: impl FnOnce(&UnixStream, &UnixStream),
) -> std::io::Result<Capture> {
    if cancel.is_cancelled() {
        return Ok(Capture {
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stop: Some(Stop::Cancelled),
        });
    }
    let (out_reader, out_writer) = UnixStream::pair()?;
    let (err_reader, err_writer) = UnixStream::pair()?;
    out_reader.set_read_timeout(Some(Duration::from_millis(50)))?;
    err_reader.set_read_timeout(Some(Duration::from_millis(50)))?;
    let (mut input_pump, input_reader) = match input {
        Some(bytes) => {
            let (input_writer, input_reader) = UnixStream::pair()?;
            input_writer.set_nonblocking(true)?;
            inspect_input_pair(&input_writer, &input_reader);
            (
                Some(InputPump {
                    stream: input_writer,
                    bytes,
                    offset: 0,
                }),
                Some(input_reader),
            )
        }
        None => (None, None),
    };
    let out_writer: OwnedFd = out_writer.into();
    let err_writer: OwnedFd = err_writer.into();
    command
        .stdin(match input_reader {
            Some(input_reader) => Stdio::from(OwnedFd::from(input_reader)),
            None => Stdio::null(),
        })
        .stdout(Stdio::from(out_writer))
        .stderr(Stdio::from(err_writer));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if input_pump.is_some() {
                command.stdin(Stdio::null());
            }
            return Err(error);
        }
    };
    // Command retains reusable Stdio handles after spawn. Release all of our
    // endpoints so EOF reflects only the running child and the controller pump.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if input_pump
        .as_ref()
        .is_some_and(|pump| pump.bytes.is_empty())
    {
        input_pump = None;
    }
    let started = Instant::now();
    let seen = Arc::new(AtomicU64::new(0));
    let overflow = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let stdout = reader(out_reader, &seen, &overflow, &done, limit);
    let stderr = reader(err_reader, &seen, &overflow, &done, limit);
    let mut stop = None;
    let status = loop {
        if cancel.is_cancelled() {
            stop = Some(Stop::Cancelled);
        } else if overflow.load(Ordering::Acquire) {
            stop = Some(Stop::Output);
        } else if started.elapsed() >= timeout {
            stop = Some(Stop::Timeout);
        }
        if stop.is_some() {
            drop(input_pump.take());
            kill_group(&child);
            let _ = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if input_pump.take().is_some() {
                    stop = Some(Stop::Io);
                }
                break Ok(status);
            }
            Ok(None) => {}
            Err(error) => {
                kill_group(&child);
                let _ = child.kill();
                let _ = child.wait();
                break Err(error);
            }
        }
        if let Some(pump) = input_pump.as_mut() {
            match pump.write_chunk(input_chunk) {
                InputProgress::Pending => {}
                InputProgress::Complete => input_pump = None,
                InputProgress::Failed => {
                    drop(input_pump.take());
                    stop = Some(Stop::Io);
                    kill_group(&child);
                    let _ = child.kill();
                    break child.wait();
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    };
    done.store(true, Ordering::Release);
    // Bounded receive also handles a broken trusted CLI retaining a pipe. Such a
    // capture cannot produce successful evidence. Docker/container cleanup is separate.
    let (stdout, out_ok) = receive(stdout);
    let (stderr, err_ok) = receive(stderr);
    if !out_ok || !err_ok || status.is_err() {
        stop = Some(Stop::Io);
    } else if overflow.load(Ordering::Acquire) && stop.is_none() {
        stop = Some(Stop::Output);
    }
    Ok(Capture {
        code: status.ok().and_then(|status| status.code()),
        stdout,
        stderr,
        stop,
    })
}

/// Also kills what a child spawned with `process_group(0)` started (merge drivers,
/// filters). Safe while the child is unreaped: only it can own a group with its PID.
fn kill_group(child: &std::process::Child) {
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
}

fn receive(receiver: std::sync::mpsc::Receiver<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    receiver
        .recv_timeout(Duration::from_millis(250))
        .unwrap_or_default()
}

fn reader(
    mut stream: impl Read + Send + 'static,
    seen: &Arc<AtomicU64>,
    overflow: &Arc<AtomicBool>,
    done: &Arc<AtomicBool>,
    limit: u64,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let seen = Arc::clone(seen);
    let overflow = Arc::clone(overflow);
    let done = Arc::clone(done);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| {
            let mut retained = Vec::new();
            let mut buffer = [0u8; 4096];
            let mut drain_started = None;
            loop {
                if done.load(Ordering::Acquire) {
                    let started = drain_started.get_or_insert_with(Instant::now);
                    if started.elapsed() >= Duration::from_millis(100) {
                        return (retained, false);
                    }
                }
                let count = match stream.read(&mut buffer) {
                    Ok(count) => count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        // Only the loop's absolute drain deadline ends capture;
                        // a transient read timeout is not EOF or completion.
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return (retained, false),
                };
                if count == 0 {
                    return (retained, true);
                }
                let previous = seen.fetch_add(count as u64, Ordering::AcqRel);
                let keep = limit.saturating_sub(previous).min(count as u64) as usize;
                retained.extend_from_slice(&buffer[..keep]);
                if keep != count {
                    overflow.store(true, Ordering::Release);
                    // Stop reading to close the pipe. The supervisor kills the CLI
                    // and removes the container; output is never an unbounded log.
                    return (retained, true);
                }
            }
        })();
        let _ = send.send(result);
    });
    receive
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;

    const TEST_MAX_SOCKET_FILL: usize = 4 * 1024 * 1024;

    fn assert_input_writer_is_nonblocking(input_writer: &UnixStream) {
        // SAFETY: `input_writer` owns a live descriptor for the duration of this
        // read-only flag query; `fcntl(F_GETFL)` does not modify descriptor state.
        let flags = unsafe { libc::fcntl(input_writer.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    fn fill_input_socket_to_backpressure(input_writer: &UnixStream) -> usize {
        input_writer
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_input_writer_is_nonblocking(input_writer);
        let mut writer = input_writer.try_clone().unwrap();
        let filler = [b'f'; INPUT_CHUNK];
        let mut accepted = 0;
        while accepted < TEST_MAX_SOCKET_FILL {
            match writer.write(&filler) {
                Ok(0) => panic!("input socket closed before backpressure"),
                Ok(count) => accepted += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return accepted,
                Err(error) => panic!("input socket fill failed: {error}"),
            }
        }
        panic!("input socket accepted the bounded fill without backpressure");
    }

    fn capture_with_retained_input_reader(
        command: &mut Command,
        input: Vec<u8>,
        timeout: Duration,
        limit: u64,
        cancel: &Cancellation,
        before_spawn: impl FnOnce(),
    ) -> (Capture, UnixStream) {
        let mut retained_reader = None;
        let result = capture_with_input_inner(
            command,
            Some(input),
            timeout,
            limit,
            cancel,
            1,
            |input_writer, input_reader| {
                input_writer
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                retained_reader = Some(input_reader.try_clone().unwrap());
                before_spawn();
            },
        )
        .unwrap();
        (result, retained_reader.unwrap())
    }

    fn assert_input_writer_closed(mut retained_reader: UnixStream) {
        retained_reader.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut buffer = [0u8; INPUT_CHUNK];
        let mut drained = 0;
        loop {
            assert!(
                Instant::now() < deadline,
                "input writer did not close before the cleanup deadline"
            );
            match retained_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    drained += count;
                    assert!(drained <= TEST_MAX_SOCKET_FILL + INPUT_CHUNK);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("input reader cleanup failed: {error}"),
            }
        }
    }

    #[test]
    fn aggregate_output_is_bounded_and_cannot_report_success() {
        let result = capture(
            Command::new("/bin/sh").args([
                "-c",
                "while :; do printf abcdefghijklmnop; printf abcdefghijklmnop >&2; done",
            ]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Output));
        assert!(result.stdout.len() + result.stderr.len() <= 256);
    }

    #[test]
    fn timeout_kills_owned_cli_and_readers_finish() {
        let started = Instant::now();
        let result = capture(
            Command::new("/bin/sh").args(["-c", "exec /bin/sleep 5"]),
            Duration::from_millis(25),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_terminates_owned_cli() {
        let cancel = Cancellation::default();
        cancel.cancel();
        let result = capture(
            Command::new("/bin/sh").args(["-c", "exec /bin/sleep 5"]),
            Duration::from_secs(2),
            256,
            &cancel,
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Cancelled));
    }

    #[test]
    fn captures_exit_and_both_streams() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "printf one; printf two >&2; exit 7"]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.code, Some(7));
        assert_eq!(result.stop, None);
        assert_eq!(result.stdout, b"one");
        assert_eq!(result.stderr, b"two");
    }

    #[test]
    fn inherited_output_writer_cannot_claim_complete_capture() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "(/bin/sleep 0.3; printf late) & exit 0"]),
            Duration::from_secs(1),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
    }

    #[test]
    fn continuous_trickle_has_an_absolute_drain_deadline_and_retains_partial_bytes() {
        let started = Instant::now();
        let result = capture(Command::new("/usr/bin/python3").args(["-c", "import os,time; child=os.fork();\nif child: os._exit(0)\nfor _ in range(100):\n os.write(1,b'x'); time.sleep(0.01)\n"]), Duration::from_secs(2), 1024, &Cancellation::default()).unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
        assert!(!result.stdout.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn large_full_duplex_input_round_trips_without_deadlock() {
        let input = vec![b'x'; 1 << 20];
        let result = capture_with_input(
            Command::new("/usr/bin/python3").args([
                "-c",
                "import sys; sys.stdout.buffer.write(b'y' * (1 << 18)); sys.stdout.buffer.flush(); data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data)",
            ]),
            Some(input.clone()),
            Duration::from_secs(5),
            2 << 20,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, None);
        assert!(result.stdout[..1 << 18].iter().all(|byte| *byte == b'y'));
        assert_eq!(&result.stdout[1 << 18..], input.as_slice());
    }

    #[test]
    fn empty_input_closes_stdin_and_reports_success() {
        let result = capture_with_input(
            &mut Command::new("/bin/cat"),
            Some(Vec::new()),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stop, None);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn shutdown_input_writer_forces_io_without_exit_assumptions() {
        let result = capture_with_input_inner(
            Command::new("/bin/sleep").arg("5"),
            Some(vec![b'x'; 4096]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            1,
            |input_writer, _| {
                input_writer
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                input_writer.shutdown(Shutdown::Write).unwrap();
                let mut probe = input_writer.try_clone().unwrap();
                assert_eq!(
                    probe.write(b"x").unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
            },
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
    }

    #[test]
    fn nonblocking_backpressure_yields_to_timeout_and_drops_writer() {
        let mut retained_reader = None;
        let mut accepted_filler = 0;
        let result = capture_with_input_inner(
            Command::new("/bin/sleep").arg("5"),
            Some(vec![b'x'; INPUT_CHUNK * 2]),
            Duration::from_millis(50),
            256,
            &Cancellation::default(),
            INPUT_CHUNK,
            |input_writer, input_reader| {
                accepted_filler = fill_input_socket_to_backpressure(input_writer);
                let mut blocked_pump = InputPump {
                    stream: input_writer.try_clone().unwrap(),
                    bytes: vec![b'x'; INPUT_CHUNK],
                    offset: 0,
                };
                assert!(matches!(
                    blocked_pump.write_chunk(INPUT_CHUNK),
                    InputProgress::Pending
                ));
                assert_eq!(blocked_pump.offset, 0);
                retained_reader = Some(input_reader.try_clone().unwrap());
            },
        )
        .unwrap();
        assert!(accepted_filler > 0);
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert_input_writer_closed(retained_reader.unwrap());
    }

    #[test]
    fn retained_input_reader_cleanup_preserves_stop_reasons() {
        for _ in 0..3 {
            let cancel = Cancellation::default();
            let (result, retained_reader) = capture_with_retained_input_reader(
                Command::new("/usr/bin/python3")
                    .args(["-c", "import os; os.read(0, 1); os.write(1, b'ready')"]),
                vec![b'x'; 4096],
                Duration::from_secs(2),
                256,
                &cancel,
                || {},
            );
            assert_eq!(result.code, Some(0));
            assert_eq!(result.stop, Some(Stop::Io));
            assert_eq!(result.stdout, b"ready");
            assert_input_writer_closed(retained_reader);

            let cancel = Cancellation::default();
            let (result, retained_reader) = capture_with_retained_input_reader(
                Command::new("/bin/sleep").arg("5"),
                vec![b'x'; 4096],
                Duration::ZERO,
                256,
                &cancel,
                || {},
            );
            assert_eq!(result.stop, Some(Stop::Timeout));
            assert_input_writer_closed(retained_reader);

            let cancel = Cancellation::default();
            let (result, retained_reader) = capture_with_retained_input_reader(
                Command::new("/bin/sleep").arg("5"),
                vec![b'x'; 4096],
                Duration::from_secs(2),
                256,
                &cancel,
                || cancel.cancel(),
            );
            assert_eq!(result.stop, Some(Stop::Cancelled));
            assert_input_writer_closed(retained_reader);

            let cancel = Cancellation::default();
            let (result, retained_reader) = capture_with_retained_input_reader(
                Command::new("/bin/sh").args([
                    "-c",
                    "trap '' PIPE; while printf abcdefghijklmnop 2>/dev/null; do :; done; exec /bin/sleep 5",
                ]),
                vec![b'x'; 4096],
                Duration::from_secs(2),
                256,
                &cancel,
                || {},
            );
            assert_eq!(result.stop, Some(Stop::Output));
            assert_input_writer_closed(retained_reader);
        }
    }

    #[test]
    fn spawn_error_releases_retained_input_endpoint() {
        let mut retained_reader = None;
        let result = capture_with_input_inner(
            &mut Command::new("/path/that/does/not/exist/agentscm-test"),
            Some(vec![b'x'; 4096]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            INPUT_CHUNK,
            |_, input_reader| retained_reader = Some(input_reader.try_clone().unwrap()),
        );
        assert_eq!(result.err().unwrap().kind(), std::io::ErrorKind::NotFound);
        assert_input_writer_closed(retained_reader.unwrap());
    }
}
