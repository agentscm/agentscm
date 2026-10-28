use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::Cancellation;

pub(crate) struct Capture {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stop: Option<Stop>,
    /// Set when a stream drain failed outside its tee sink or waiting on the child
    /// failed, so ownership may be lost; tee sink failures are reported through `stop`.
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Timeout,
    Cancelled,
    Output,
    Io,
}

const INPUT_CHUNK: usize = 16 * 1024;
// After the leader exits, a concurrent spawn elsewhere in this process can hold
// inherited copies of our pipe ends until its exec completes, which takes
// hundreds of milliseconds on a loaded host. Only a retained pipe pays this wait.
const POST_EXIT_DRAIN: Duration = Duration::from_secs(2);
const BLOCKED_TEE_WAIT: Duration = Duration::from_millis(250);
const RECEIVE_POLL: Duration = Duration::from_millis(25);

struct CaptureSettings {
    input_chunk: usize,
    // A deterministic test seam: hold output reads until exit is observed.
    reader_gate: Option<Arc<AtomicBool>>,
}

#[derive(Default)]
struct TeeCallWatch {
    active: Option<(u64, Instant)>,
}

impl TeeCallWatch {
    fn blocked(&mut self, state: &AtomicU64) -> bool {
        let generation = state.load(Ordering::Acquire);
        if generation.is_multiple_of(2) {
            self.active = None;
            return false;
        }
        if self
            .active
            .as_ref()
            .is_none_or(|(active, _)| *active != generation)
        {
            self.active = Some((generation, Instant::now()));
        }
        self.active
            .as_ref()
            .is_some_and(|(_, started)| started.elapsed() >= BLOCKED_TEE_WAIT)
    }
}

impl CaptureSettings {
    fn new(input_chunk: usize) -> Self {
        Self {
            input_chunk,
            reader_gate: None,
        }
    }
}

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
        CaptureSettings::new(INPUT_CHUNK),
        |_, _| {},
        |_, _| Ok(()),
        None,
        None,
    )
}

// Runs the callback synchronously after a successful spawn and before supervision. The caller
// must configure `process_group(0)` before invoking this variant; that makes the child's PID its
// process-group ID. A callback failure terminates and reaps that owned group before the original
// callback error is returned. The sinks mirror only bytes admitted by the bounded capture. Sink
// errors stop the owned process group, and the existing timeout plus bounded receiver wait keep
// the supervisor finite. A stalled writer worker may be abandoned after that wait; this does not
// claim that the sink itself was interrupted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_with_input_tee_after_spawn<Out, Err, After>(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: u64,
    cancel: &Cancellation,
    stdout: Out,
    stderr: Err,
    after_spawn: After,
) -> std::io::Result<Capture>
where
    Out: Write + Send + 'static,
    Err: Write + Send + 'static,
    After: FnOnce(u32, u32) -> std::io::Result<()>,
{
    capture_with_input_inner(
        command,
        input,
        timeout,
        limit,
        cancel,
        CaptureSettings::new(INPUT_CHUNK),
        |_, _| {},
        after_spawn,
        Some(Box::new(stdout)),
        Some(Box::new(stderr)),
    )
}

// The private implementation keeps the existing no-tee API and its deterministic input-pump test
// seams explicit. Collapsing these into a loosely typed options bag would obscure process cleanup.
#[allow(clippy::too_many_arguments)]
fn capture_with_input_inner(
    command: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: u64,
    cancel: &Cancellation,
    settings: CaptureSettings,
    inspect_input_pair: impl FnOnce(&UnixStream, &UnixStream),
    after_spawn: impl FnOnce(u32, u32) -> std::io::Result<()>,
    stdout_tee: Option<Box<dyn Write + Send>>,
    stderr_tee: Option<Box<dyn Write + Send>>,
) -> std::io::Result<Capture> {
    if cancel.is_cancelled() {
        return Ok(Capture {
            code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stop: Some(Stop::Cancelled),
            incomplete: false,
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
    if let Err(error) = after_spawn(child.id(), child.id()) {
        drop(input_pump.take());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut owns_child = true;
        let mut wait_error = None;
        let mut group_signalled = false;
        signal_owned_group(
            &mut child,
            &mut owns_child,
            &mut wait_error,
            &mut group_signalled,
        );
        if owns_child {
            loop {
                match child.wait() {
                    Err(wait_error) if wait_error.kind() == std::io::ErrorKind::Interrupted => {}
                    _ => break,
                }
            }
        }
        return Err(error);
    }
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
    let tee_failed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let stdout_tee_state = Arc::new(AtomicU64::new(0));
    let stderr_tee_state = Arc::new(AtomicU64::new(0));
    let mut stdout_tee_watch = TeeCallWatch::default();
    let mut stderr_tee_watch = TeeCallWatch::default();
    let stdout = reader(
        out_reader,
        &seen,
        &overflow,
        &done,
        limit,
        settings.reader_gate.clone(),
        stdout_tee,
        &tee_failed,
        &stdout_tee_state,
    );
    let stderr = reader(
        err_reader,
        &seen,
        &overflow,
        &done,
        limit,
        settings.reader_gate.clone(),
        stderr_tee,
        &tee_failed,
        &stderr_tee_state,
    );
    let mut stop = None;
    let mut wait_error = None;
    let mut owns_child = true;
    let mut group_signalled = false;
    loop {
        enforce_supervision(
            &mut stop,
            cancel,
            &overflow,
            &tee_failed,
            &stdout_tee_state,
            &stderr_tee_state,
            &mut stdout_tee_watch,
            &mut stderr_tee_watch,
            started,
            timeout,
            &mut input_pump,
            &mut child,
            &mut owns_child,
            &mut wait_error,
            &mut group_signalled,
        );
        if stop.is_some() {
            break;
        }
        let exited = match child_exited_without_reaping(&child) {
            Ok(exited) => exited,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // An external reaper may have removed our PID reservation.
                // Never signal that process group after ownership is lost.
                owns_child = error.raw_os_error() != Some(libc::ECHILD);
                wait_error = Some(error);
                break;
            }
        };
        if exited {
            if input_pump.take().is_some() {
                stop = Some(Stop::Io);
            }
            break;
        }
        if let Some(pump) = input_pump.as_mut() {
            match pump.write_chunk(settings.input_chunk) {
                InputProgress::Pending => {}
                InputProgress::Complete => input_pump = None,
                InputProgress::Failed => {
                    drop(input_pump.take());
                    stop = Some(Stop::Io);
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    done.store(true, Ordering::Release);
    if let Some(gate) = settings.reader_gate {
        gate.store(true, Ordering::Release);
    }
    if wait_error.is_some() && stop.is_none() {
        stop = Some(Stop::Io);
    }
    enforce_supervision(
        &mut stop,
        cancel,
        &overflow,
        &tee_failed,
        &stdout_tee_state,
        &stderr_tee_state,
        &mut stdout_tee_watch,
        &mut stderr_tee_watch,
        started,
        timeout,
        &mut input_pump,
        &mut child,
        &mut owns_child,
        &mut wait_error,
        &mut group_signalled,
    );
    // Reader waits continue enforcing the original execution stop conditions. A
    // retained descendant pipe therefore cannot extend timeout or cancellation.
    let (stdout, out_ok, out_tee_failed) = {
        let mut tick = || {
            enforce_supervision(
                &mut stop,
                cancel,
                &overflow,
                &tee_failed,
                &stdout_tee_state,
                &stderr_tee_state,
                &mut stdout_tee_watch,
                &mut stderr_tee_watch,
                started,
                timeout,
                &mut input_pump,
                &mut child,
                &mut owns_child,
                &mut wait_error,
                &mut group_signalled,
            );
        };
        receive(stdout, &stdout_tee_state, &mut tick)
    };
    if !out_ok && stop.is_none() {
        stop = Some(Stop::Io);
    }
    enforce_supervision(
        &mut stop,
        cancel,
        &overflow,
        &tee_failed,
        &stdout_tee_state,
        &stderr_tee_state,
        &mut stdout_tee_watch,
        &mut stderr_tee_watch,
        started,
        timeout,
        &mut input_pump,
        &mut child,
        &mut owns_child,
        &mut wait_error,
        &mut group_signalled,
    );
    let (stderr, err_ok, err_tee_failed) = {
        let mut tick = || {
            enforce_supervision(
                &mut stop,
                cancel,
                &overflow,
                &tee_failed,
                &stdout_tee_state,
                &stderr_tee_state,
                &mut stdout_tee_watch,
                &mut stderr_tee_watch,
                started,
                timeout,
                &mut input_pump,
                &mut child,
                &mut owns_child,
                &mut wait_error,
                &mut group_signalled,
            );
        };
        receive(stderr, &stderr_tee_state, &mut tick)
    };
    if !err_ok && stop.is_none() {
        stop = Some(Stop::Io);
    }
    enforce_supervision(
        &mut stop,
        cancel,
        &overflow,
        &tee_failed,
        &stdout_tee_state,
        &stderr_tee_state,
        &mut stdout_tee_watch,
        &mut stderr_tee_watch,
        started,
        timeout,
        &mut input_pump,
        &mut child,
        &mut owns_child,
        &mut wait_error,
        &mut group_signalled,
    );
    let terminal_stop = primary_stop(stop);
    let non_tee_reader_failed = (!out_ok && !out_tee_failed) || (!err_ok && !err_tee_failed);
    // A drain that failed outside its tee sink left collection incomplete, and a
    // wait error means ownership of the child is uncertain. A tee sink failure is
    // not incomplete capture: it stops the child as `Io` only when no primary stop
    // was already observed.
    let mut incomplete = non_tee_reader_failed || wait_error.is_some();
    // An already observed timeout, cancellation or output limit stays the primary
    // cause; `incomplete` carries the missing-data evidence alongside it.
    if !terminal_stop
        && (non_tee_reader_failed || wait_error.is_some() || out_tee_failed || err_tee_failed)
    {
        stop = Some(Stop::Io);
    }
    // Even after a fast normal exit, late reader overflow or incomplete drain
    // must terminate descendants while the leader is still waitable. Recheck
    // ownership before that signal because an external reaper revokes our PID
    // reservation and makes any process-group signal unsafe.
    enforce_supervision(
        &mut stop,
        cancel,
        &overflow,
        &tee_failed,
        &stdout_tee_state,
        &stderr_tee_state,
        &mut stdout_tee_watch,
        &mut stderr_tee_watch,
        started,
        timeout,
        &mut input_pump,
        &mut child,
        &mut owns_child,
        &mut wait_error,
        &mut group_signalled,
    );
    let status = if let Some(error) = wait_error {
        if owns_child {
            let _ = child.wait();
        }
        Err(error)
    } else {
        child.wait()
    };
    if status.is_err() {
        incomplete = true;
        if !primary_stop(stop) {
            stop = Some(Stop::Io);
        }
    }
    let (code, signal) = match status {
        Ok(status) => (status.code(), status.signal()),
        Err(_) => (None, None),
    };
    Ok(Capture {
        code,
        signal,
        stdout,
        stderr,
        stop,
        incomplete,
    })
}

fn primary_stop(stop: Option<Stop>) -> bool {
    matches!(stop, Some(Stop::Timeout | Stop::Cancelled | Stop::Output))
}

// Observe terminal status without consuming it. si_signo is a portable POSIX
// siginfo field; no platform-specific child-pid union accessor is required.
fn child_exited_without_reaping(child: &std::process::Child) -> std::io::Result<bool> {
    // SAFETY: zero is a valid empty siginfo_t; waitid initializes it on an event.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: info is writable for the call. P_PID selects our live Child and
    // WNOWAIT preserves its waitable status/PID reservation until Child::wait.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(info.si_signo == libc::SIGCHLD)
    }
}

/// Also kills what a child spawned with `process_group(0)` started (merge drivers,
/// filters). Safe while the child is unreaped: only it can own a group with its PID.
fn kill_group(child: &std::process::Child) {
    #[cfg(test)]
    GROUP_SIGNAL_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
}

fn signal_owned_group(
    child: &mut std::process::Child,
    owns_child: &mut bool,
    wait_error: &mut Option<std::io::Error>,
    group_signalled: &mut bool,
) {
    if !*owns_child || *group_signalled {
        return;
    }
    refresh_child_ownership(child, owns_child, wait_error);
    if *owns_child {
        kill_group(child);
        *group_signalled = true;
        let _ = child.kill();
    }
}

#[allow(clippy::too_many_arguments)]
fn enforce_supervision(
    stop: &mut Option<Stop>,
    cancel: &Cancellation,
    overflow: &AtomicBool,
    tee_failed: &AtomicBool,
    stdout_tee_state: &AtomicU64,
    stderr_tee_state: &AtomicU64,
    stdout_tee_watch: &mut TeeCallWatch,
    stderr_tee_watch: &mut TeeCallWatch,
    started: Instant,
    timeout: Duration,
    input_pump: &mut Option<InputPump>,
    child: &mut std::process::Child,
    owns_child: &mut bool,
    wait_error: &mut Option<std::io::Error>,
    group_signalled: &mut bool,
) {
    let stdout_tee_blocked = stdout_tee_watch.blocked(stdout_tee_state);
    let stderr_tee_blocked = stderr_tee_watch.blocked(stderr_tee_state);
    if stop.is_none() {
        if cancel.is_cancelled() {
            *stop = Some(Stop::Cancelled);
        } else if overflow.load(Ordering::Acquire) {
            *stop = Some(Stop::Output);
        } else if started.elapsed() >= timeout {
            *stop = Some(Stop::Timeout);
        } else if tee_failed.load(Ordering::Acquire) || stdout_tee_blocked || stderr_tee_blocked {
            *stop = Some(Stop::Io);
        }
    }
    if stop.is_some() {
        drop(input_pump.take());
        signal_owned_group(child, owns_child, wait_error, group_signalled);
    }
}

fn refresh_child_ownership(
    child: &std::process::Child,
    owns_child: &mut bool,
    wait_error: &mut Option<std::io::Error>,
) {
    while *owns_child {
        match child_exited_without_reaping(child) {
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                *owns_child = error.raw_os_error() != Some(libc::ECHILD);
                if wait_error.is_none() {
                    *wait_error = Some(error);
                }
                return;
            }
        }
    }
}

fn receive(
    receiver: std::sync::mpsc::Receiver<(Vec<u8>, bool, bool)>,
    tee_state: &AtomicU64,
    on_poll: &mut impl FnMut(),
) -> (Vec<u8>, bool, bool) {
    // Stream reads are bounded by their read timeout and absolute drain deadline, so
    // scheduler delay alone cannot fail a capture. Only a writer currently inside an
    // operator-provided tee sink pays the separate blocked-sink deadline.
    let mut blocked_call = TeeCallWatch::default();
    loop {
        on_poll();
        match receiver.recv_timeout(RECEIVE_POLL) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return (Vec::new(), false, false);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if blocked_call.blocked(tee_state) {
                    return (Vec::new(), false, true);
                }
            }
        }
    }
}

// Reader state is passed explicitly so stdout and stderr share only bounded counters and stop
// flags; the stream and optional sink remain independently owned by each worker.
#[allow(clippy::too_many_arguments)]
fn reader(
    mut stream: impl Read + Send + 'static,
    seen: &Arc<AtomicU64>,
    overflow: &Arc<AtomicBool>,
    done: &Arc<AtomicBool>,
    limit: u64,
    gate: Option<Arc<AtomicBool>>,
    mut tee: Option<Box<dyn Write + Send>>,
    tee_failed: &Arc<AtomicBool>,
    tee_state: &Arc<AtomicU64>,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool, bool)> {
    let seen = Arc::clone(seen);
    let overflow = Arc::clone(overflow);
    let done = Arc::clone(done);
    let tee_failed = Arc::clone(tee_failed);
    let tee_state = Arc::clone(tee_state);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        if let Some(gate) = gate {
            while !gate.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let result = (|| {
            let mut retained = Vec::new();
            let mut buffer = [0u8; 4096];
            let mut drain_started = None;
            loop {
                let count = match stream.read(&mut buffer) {
                    Ok(0) => return (retained, true, false),
                    Ok(count) => count,
                    // A transient read timeout is not EOF or completion.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        0
                    }
                    Err(_) => return (retained, false, false),
                };
                let previous = seen.fetch_add(count as u64, Ordering::AcqRel);
                let keep = limit.saturating_sub(previous).min(count as u64) as usize;
                retained.extend_from_slice(&buffer[..keep]);
                let tee_write_failed = if keep > 0 {
                    tee.as_mut().is_some_and(|sink| {
                        let active = tee_state.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                        debug_assert_eq!(active % 2, 1);
                        let failed =
                            sink.write_all(&buffer[..keep]).is_err() || sink.flush().is_err();
                        let inactive = tee_state.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                        debug_assert_eq!(inactive % 2, 0);
                        failed
                    })
                } else {
                    false
                };
                if tee_write_failed {
                    tee_failed.store(true, Ordering::Release);
                    return (retained, false, true);
                }
                if keep != count {
                    overflow.store(true, Ordering::Release);
                    // Stop reading to close the pipe. The supervisor kills the CLI
                    // and removes the container; output is never an unbounded log.
                    return (retained, true, false);
                }
                // Only this absolute drain deadline ends capture. It is checked after
                // a read, so a reader descheduled past it still observes an EOF that
                // is already available instead of failing.
                if done.load(Ordering::Acquire)
                    && drain_started.get_or_insert_with(Instant::now).elapsed() >= POST_EXIT_DRAIN
                {
                    return (retained, false, false);
                }
            }
        })();
        let _ = send.send(result);
    });
    receive
}

#[cfg(test)]
std::thread_local! {
    static GROUP_SIGNAL_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::sync::{Condvar, Mutex};

    const TEST_MAX_SOCKET_FILL: usize = 4 * 1024 * 1024;

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FailedSink;

    impl Write for FailedSink {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "synthetic tee failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct SlowSink {
        bytes: Arc<Mutex<Vec<u8>>>,
        calls: Arc<AtomicU64>,
        delay: Duration,
    }

    impl SlowSink {
        fn new(delay: Duration) -> Self {
            Self {
                bytes: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicU64::new(0)),
                delay,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Acquire)
        }
    }

    impl Write for SlowSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            thread::sleep(self.delay);
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct BlockingSink(Arc<(Mutex<bool>, Condvar)>);

    impl BlockingSink {
        fn release(&self) {
            let (released, wake) = &*self.0;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
    }

    impl Write for BlockingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let (released, wake) = &*self.0;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

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
            CaptureSettings::new(1),
            |input_writer, input_reader| {
                input_writer
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                retained_reader = Some(input_reader.try_clone().unwrap());
                before_spawn();
            },
            |_, _| Ok(()),
            None,
            None,
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
    fn fast_exit_overflow_terminates_descendants_before_reaping_the_leader() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("late-write");
        let mut command = Command::new("/usr/bin/python3");
        command
            .process_group(0)
            .args([
                "-c",
                "import os,sys,time
child=os.fork()
if child == 0:
 os.close(1); os.close(2); time.sleep(0.4)
 open(sys.argv[1], 'w').write('late')
 os._exit(0)
os.write(1,b'x'*128)
os._exit(0)
",
            ])
            .arg(&marker);
        let result = capture_with_input_inner(
            &mut command,
            None,
            Duration::from_secs(2),
            64,
            &Cancellation::default(),
            CaptureSettings {
                input_chunk: INPUT_CHUNK,
                reader_gate: Some(Arc::new(AtomicBool::new(false))),
            },
            |_, _| {},
            |_, _| Ok(()),
            None,
            None,
        )
        .unwrap();
        // Readers cannot sample overflow until the leader is known to have
        // exited. The descendant closes both streams, so this is Output, not Io.
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stop, Some(Stop::Output));
        assert_eq!(result.stdout.len(), 64);
        thread::sleep(Duration::from_millis(650));
        assert!(
            !marker.exists(),
            "descendant survived a late overflow result"
        );
    }

    // A setsid grandchild leaves the leader's session and process group, so it
    // keeps the output pipes open after the leader's group is killed and the drain cannot
    // complete (it ends at the post-exit deadline).
    // The leader continues only after the holder has detached, so no stop can
    // kill the holder before it leaves the process group. It then touches the
    // marker named by argv[1].
    const DETACHED_HOLDER: &str = "import os,sys,time\nr,w=os.pipe()\nif os.fork()==0:\n os.setsid(); os.write(w,b'x'); time.sleep(10); os._exit(0)\nos.read(r,1)\nopen(sys.argv[1],'w').close()\n";

    fn capture_with_detached_holder(
        leader: &str,
        timeout: Duration,
        limit: u64,
        cancel: &Cancellation,
        cancel_when_detached: bool,
    ) -> Capture {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("detached");
        let script = format!("{DETACHED_HOLDER}{leader}");
        let mut command = Command::new("/usr/bin/python3");
        command
            .process_group(0)
            .args(["-c", script.as_str()])
            .arg(&marker);
        let cancel_in_hook = cancel.clone();
        capture_with_input_inner(
            &mut command,
            None,
            timeout,
            limit,
            cancel,
            CaptureSettings::new(INPUT_CHUNK),
            |_, _| {},
            move |_, _| {
                if cancel_when_detached {
                    thread::spawn(move || {
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !marker.exists() && Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(5));
                        }
                        cancel_in_hook.cancel();
                    });
                }
                Ok(())
            },
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn output_limit_stays_primary_when_drain_is_incomplete() {
        let result = capture_with_detached_holder(
            "try:\n while True: os.write(1, b'abcdefghijklmnop')\nexcept OSError: time.sleep(30)\n",
            Duration::from_secs(10),
            64,
            &Cancellation::default(),
            false,
        );
        assert_eq!(result.stop, Some(Stop::Output));
        assert!(result.incomplete);
        assert!(result.stdout.len() + result.stderr.len() <= 64);
    }

    #[test]
    fn timeout_stays_primary_when_drain_is_incomplete() {
        let result = capture_with_detached_holder(
            "time.sleep(30)\n",
            Duration::from_secs(3),
            256,
            &Cancellation::default(),
            false,
        );
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert!(result.incomplete);
    }

    #[test]
    fn cancellation_stays_primary_when_drain_is_incomplete() {
        let result = capture_with_detached_holder(
            "time.sleep(30)\n",
            Duration::from_secs(10),
            256,
            &Cancellation::default(),
            true,
        );
        assert_eq!(result.stop, Some(Stop::Cancelled));
        assert!(result.incomplete);
    }

    #[test]
    fn primary_stop_is_complete_when_collection_finishes() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "exec /bin/sleep 5"]),
            Duration::from_millis(25),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert!(!result.incomplete);
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
    fn drain_supervision_never_signals_an_externally_reaped_child() {
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| attempts.set(0));
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut status = 0;
        loop {
            // SAFETY: this test owns `child`; waiting for its exact PID deliberately
            // simulates the external-reaper transition supported by the supervisor.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited == pid {
                break;
            }
            assert_eq!(waited, -1);
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                panic!("failed to externally reap child: {error}");
            }
        }

        let cancel = Cancellation::default();
        cancel.cancel();
        let overflow = AtomicBool::new(false);
        let tee_failed = AtomicBool::new(false);
        let stdout_tee_state = AtomicU64::new(0);
        let stderr_tee_state = AtomicU64::new(0);
        let mut stdout_tee_watch = TeeCallWatch::default();
        let mut stderr_tee_watch = TeeCallWatch::default();
        let mut input_pump = None;
        let mut stop = None;
        let mut owns_child = true;
        let mut wait_error = None;
        let mut group_signalled = false;
        enforce_supervision(
            &mut stop,
            &cancel,
            &overflow,
            &tee_failed,
            &stdout_tee_state,
            &stderr_tee_state,
            &mut stdout_tee_watch,
            &mut stderr_tee_watch,
            Instant::now(),
            Duration::from_secs(1),
            &mut input_pump,
            &mut child,
            &mut owns_child,
            &mut wait_error,
            &mut group_signalled,
        );

        assert_eq!(stop, Some(Stop::Cancelled));
        assert!(!owns_child);
        assert_eq!(
            wait_error.as_ref().and_then(std::io::Error::raw_os_error),
            Some(libc::ECHILD)
        );
        assert!(!group_signalled);
        GROUP_SIGNAL_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 0));
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
            Command::new("/bin/sh")
                .process_group(0)
                .args(["-c", "printf one; printf two >&2; exit 7"]),
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
    fn captures_self_sigterm_without_reporting_a_normal_exit() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "kill -TERM $$"]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.code, None);
        assert_eq!(result.signal, Some(libc::SIGTERM));
        assert_eq!(result.stop, None);
    }

    #[test]
    fn captures_self_sigkill_without_reporting_a_normal_exit() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "kill -KILL $$"]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.code, None);
        assert_eq!(result.signal, Some(libc::SIGKILL));
        assert_eq!(result.stop, None);
    }

    #[test]
    fn tee_streams_and_retains_both_outputs_without_changing_exit_status() {
        let stdout = SharedSink::default();
        let stderr = SharedSink::default();
        let result = capture_with_input_tee_after_spawn(
            Command::new("/bin/sh").args(["-c", "printf one; printf two >&2; exit 7"]),
            None,
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            stdout.clone(),
            stderr.clone(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.code, Some(7));
        assert_eq!(result.stop, None);
        assert_eq!(result.stdout, b"one");
        assert_eq!(result.stderr, b"two");
        assert_eq!(stdout.bytes(), result.stdout);
        assert_eq!(stderr.bytes(), result.stderr);
    }

    #[test]
    fn after_spawn_observes_owned_group_and_normal_capture_reaps_child() {
        let identity = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&identity);
        let mut command = Command::new("/bin/sh");
        command.process_group(0).args(["-c", "printf observed"]);
        let result = capture_with_input_tee_after_spawn(
            &mut command,
            None,
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            SharedSink::default(),
            SharedSink::default(),
            move |pid, pgid| {
                *observed.lock().unwrap() = Some((pid, pgid));
                Ok(())
            },
        )
        .unwrap();
        let (pid, pgid) = identity.lock().unwrap().unwrap();
        assert_eq!(pid, pgid);
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stop, None);
        assert_eq!(result.stdout, b"observed");

        let mut status = 0;
        // SAFETY: this probes the PID observed from the now-completed child. The
        // capture owns and has already reaped it, so no waitable child remains.
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn after_spawn_failure_kills_and_reaps_the_owned_process_group() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("descendant-ready");
        let marker = root.path().join("late-write");
        let observed_pid = Arc::new(Mutex::new(None));
        let callback_pid = Arc::clone(&observed_pid);
        let callback_ready = ready.clone();
        let mut command = Command::new("/usr/bin/python3");
        command
            .process_group(0)
            .args([
                "-c",
                "import os,sys,time\nchild=os.fork()\nif child == 0:\n open(sys.argv[1], 'w').write('ready')\n time.sleep(0.4)\n open(sys.argv[2], 'w').write('late')\n os._exit(0)\ntime.sleep(5)\n",
            ])
            .arg(&ready)
            .arg(&marker);
        let started = Instant::now();
        let error = match capture_with_input_tee_after_spawn(
            &mut command,
            None,
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            SharedSink::default(),
            SharedSink::default(),
            move |pid, pgid| {
                assert_eq!(pid, pgid);
                *callback_pid.lock().unwrap() = Some(pid);
                let deadline = Instant::now() + Duration::from_secs(1);
                while !callback_ready.exists() {
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "descendant did not become ready",
                        ));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(std::io::Error::other("synthetic after-spawn failure"))
            },
        ) {
            Ok(_) => panic!("after-spawn failure unexpectedly produced a capture"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "synthetic after-spawn failure");
        assert!(started.elapsed() < Duration::from_secs(2));

        let pid = observed_pid.lock().unwrap().unwrap();
        let mut status = 0;
        // SAFETY: callback failure requires capture to reap its owned leader.
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        thread::sleep(Duration::from_millis(650));
        assert!(!marker.exists(), "descendant survived callback failure");
    }

    #[test]
    fn tee_timeout_retains_streamed_prefix_without_claiming_success() {
        let stdout = SharedSink::default();
        let stderr = SharedSink::default();
        let result = capture_with_input_tee_after_spawn(
            Command::new("/bin/sh")
                .process_group(0)
                .args(["-c", "printf before; exec /bin/sleep 5"]),
            None,
            Duration::from_millis(50),
            256,
            &Cancellation::default(),
            stdout.clone(),
            stderr,
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert_eq!(result.stdout, b"before");
        assert_eq!(stdout.bytes(), result.stdout);
    }

    #[test]
    fn tee_writes_only_the_prefix_admitted_by_the_shared_output_bound() {
        let stdout = SharedSink::default();
        let stderr = SharedSink::default();
        let result = capture_with_input_tee_after_spawn(
            Command::new("/usr/bin/python3")
                .process_group(0)
                .args(["-c", "import os,time; os.write(1,b'x'*1024); time.sleep(5)"]),
            None,
            Duration::from_secs(2),
            64,
            &Cancellation::default(),
            stdout.clone(),
            stderr,
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Output));
        assert_eq!(result.stdout.len() + result.stderr.len(), 64);
        assert_eq!(stdout.bytes(), result.stdout);
    }

    #[test]
    fn tee_sink_failure_stops_the_child_and_cannot_report_success() {
        let started = Instant::now();
        let result = capture_with_input_tee_after_spawn(
            Command::new("/bin/sh")
                .process_group(0)
                .args(["-c", "printf output; exec /bin/sleep 5"]),
            None,
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            FailedSink,
            SharedSink::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
        assert!(!result.incomplete);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn blocked_tee_sink_cannot_defeat_the_supervision_timeout() {
        let sink = BlockingSink::default();
        let started = Instant::now();
        let result = capture_with_input_tee_after_spawn(
            Command::new("/bin/sh")
                .process_group(0)
                .args(["-c", "printf output; exec /bin/sleep 5"]),
            None,
            Duration::from_millis(50),
            256,
            &Cancellation::default(),
            sink.clone(),
            SharedSink::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        sink.release();
        assert_eq!(result.stop, Some(Stop::Timeout));
        assert!(!result.incomplete);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn consecutive_slow_tee_calls_each_receive_their_own_bound() {
        let stdout = SlowSink::new(Duration::from_millis(150));
        let result = capture_with_input_tee_after_spawn(
            Command::new("/usr/bin/python3")
                .process_group(0)
                .args(["-c", "import os; os.write(1, b'x' * 8192)"]),
            None,
            Duration::from_secs(2),
            16 * 1024,
            &Cancellation::default(),
            stdout.clone(),
            SharedSink::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, None);
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stdout, vec![b'x'; 8192]);
        assert_eq!(stdout.bytes(), result.stdout);
        assert!(stdout.calls() >= 2);
    }

    #[test]
    fn post_exit_drain_keeps_the_original_timeout_authoritative() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("late-descendant-write");
        let mut command = Command::new("/usr/bin/python3");
        command
            .process_group(0)
            .args([
                "-c",
                "import os,sys,time\nchild=os.fork()\nif child == 0:\n time.sleep(0.4)\n open(sys.argv[1], 'w').write('late')\n os._exit(0)\nos.write(1,b'ok')\nos._exit(0)\n",
            ])
            .arg(&marker);
        let result = capture_with_input_tee_after_spawn(
            &mut command,
            None,
            Duration::from_millis(50),
            256,
            &Cancellation::default(),
            SharedSink::default(),
            SharedSink::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Timeout));
        thread::sleep(Duration::from_millis(550));
        assert!(
            !marker.exists(),
            "descendant acted after the original execution deadline"
        );
    }

    #[test]
    fn inherited_output_writer_cannot_claim_complete_capture() {
        let result = capture(
            Command::new("/bin/sh").args(["-c", "(/bin/sleep 4; printf late) & exit 0"]),
            Duration::from_secs(10),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
        assert!(result.incomplete);
    }

    #[test]
    fn a_write_end_released_shortly_after_exit_still_completes_capture() {
        // Stands in for a concurrent spawn that briefly holds an inherited copy of
        // the pipe on a loaded host; the old 100 ms bound reported this as Io.
        let result = capture(
            Command::new("/bin/sh").args(["-c", "printf ok; (/bin/sleep 0.3) & exit 0"]),
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(result.stop, None);
        assert!(!result.incomplete);
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stdout, b"ok");
    }

    #[test]
    fn tee_allows_a_write_end_released_shortly_after_exit() {
        // The tee itself finishes immediately. The reader must then use the stream's
        // drain deadline instead of confusing the inherited pipe with a blocked sink.
        let stdout = SharedSink::default();
        let stderr = SharedSink::default();
        let mut command = Command::new("/bin/sh");
        command
            .process_group(0)
            .args(["-c", "printf ok; (/bin/sleep 0.3) & exit 0"]);
        let result = capture_with_input_tee_after_spawn(
            &mut command,
            None,
            Duration::from_secs(2),
            256,
            &Cancellation::default(),
            stdout.clone(),
            stderr.clone(),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.stop, None);
        assert_eq!(result.code, Some(0));
        assert_eq!(result.stdout, b"ok");
        assert!(result.stderr.is_empty());
        assert_eq!(stdout.bytes(), result.stdout);
        assert_eq!(stderr.bytes(), result.stderr);
    }

    #[test]
    fn continuous_trickle_has_an_absolute_drain_deadline_and_retains_partial_bytes() {
        let result = capture(Command::new("/usr/bin/python3").args(["-c", "import os,time; child=os.fork();\nif child: os._exit(0)\nfor _ in range(400):\n os.write(1,b'x'); time.sleep(0.01)\n"]), Duration::from_secs(10), 1024, &Cancellation::default()).unwrap();
        assert_eq!(result.stop, Some(Stop::Io));
        assert!(result.incomplete);
        assert!(!result.stdout.is_empty());
        // The writer needs at least four seconds for all 400 bytes; the deadline ends the
        // capture first. Counting bytes keeps interpreter startup out of the measurement.
        assert!(result.stdout.len() < 400, "{}", result.stdout.len());
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
            CaptureSettings::new(1),
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
            |_, _| Ok(()),
            None,
            None,
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
            CaptureSettings::new(INPUT_CHUNK),
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
            |_, _| Ok(()),
            None,
            None,
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
            CaptureSettings::new(INPUT_CHUNK),
            |_, input_reader| retained_reader = Some(input_reader.try_clone().unwrap()),
            |_, _| Ok(()),
            None,
            None,
        );
        assert_eq!(result.err().unwrap().kind(), std::io::ErrorKind::NotFound);
        assert_input_writer_closed(retained_reader.unwrap());
    }
}
