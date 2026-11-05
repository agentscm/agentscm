//! Machine-local FIFO for expensive validation. Compile with the pinned rustc, edition 2024.
//! Queue files contain only numeric tickets and process identities, never command arguments.
use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::Duration,
};

static SIGNAL: AtomicI32 = AtomicI32::new(0);
// These Unix interfaces are used only for signal forwarding and inheriting the slot lock.
unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
    fn kill(pid: i32, signal: i32) -> i32;
    fn getuid() -> u32;
}
extern "C" fn interrupted(signal: i32) {
    SIGNAL.store(signal, Ordering::Relaxed);
}
fn pause() {
    thread::sleep(Duration::from_millis(1000));
}
fn private(metadata: &fs::Metadata) -> io::Result<()> {
    // SAFETY: getuid has no arguments or failure mode.
    if metadata.uid() != unsafe { getuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other("queue state must be owner-private"));
    }
    Ok(())
}
fn open(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "macos")]
    const NOFOLLOW: i32 = 0x100;
    #[cfg(target_os = "linux")]
    const NOFOLLOW: i32 = 0x20000;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    private(&metadata)?;
    if !metadata.is_file() {
        return Err(io::Error::other("queue state is not a regular file"));
    }
    Ok(file)
}
fn mutex(root: &Path) -> io::Result<File> {
    let file = open(&root.join("mutex"))?;
    file.lock()?;
    Ok(file)
}
fn entries(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(root)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "job"))
        .collect::<Vec<_>>();
    for path in &entries {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if name.len() != 20 || !name.bytes().all(|b| b.is_ascii_digit()) {
            return Err(io::Error::other("invalid queue ticket filename"));
        }
    }
    entries.sort();
    Ok(entries)
}
fn group_alive(pid: u32) -> io::Result<bool> {
    // Ignore orphan zombies, which cannot execute and may await a slow init reaper.
    // A reused process-group identity conservatively delays work instead of permitting overlap.
    let output = Command::new("ps").args(["-axo", "pgid=,stat="]).output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot inspect process groups"));
    }
    let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    Ok(text.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|s| s.parse::<u32>().ok()) == Some(pid)
            && fields.next().is_some_and(|s| !s.starts_with('Z'))
    }))
}
fn recorded_group(path: &Path) -> io::Result<Option<u32>> {
    let text = fs::read_to_string(path)?;
    if text.is_empty() {
        return Ok(None);
    }
    text.trim().parse().map(Some).map_err(io::Error::other)
}
fn remove_idle(root: &Path, own: &Path) -> io::Result<Vec<PathBuf>> {
    let mut active = Vec::new();
    for path in entries(root)? {
        if path == own {
            active.push(path);
            continue;
        }
        let file = open(&path)?;
        if file.try_lock().is_ok()
            && !recorded_group(&path)?.is_some_and(|pid| group_alive(pid).unwrap_or(true))
        {
            fs::remove_file(path)?;
        } else {
            active.push(path);
        }
    }
    Ok(active)
}
fn legacy_ready(root: &Path, own: &Path, legacy: &Path) -> io::Result<bool> {
    let intent = root.join("legacy-owner");
    if let Ok(previous) = fs::read_to_string(&intent)
        && previous != own.file_name().unwrap().to_string_lossy()
        && !root.join(previous.trim()).exists()
        && legacy.is_dir()
        && fs::read_to_string(legacy.join("pid"))
            .unwrap_or_default()
            .is_empty()
    {
        // A new supervisor died between mkdir and PID publication. All participants must
        // have upgraded: an old wrapper has no recoverable registration for this crash gap.
        fs::remove_dir(legacy)?;
    }
    fs::write(
        &intent,
        own.file_name().unwrap().to_string_lossy().as_bytes(),
    )?;
    match fs::create_dir(legacy) {
        Ok(()) => {
            fs::write(legacy.join("pid"), process::id().to_string())?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Existing old wrappers have no reliable birth identity. A reused live PID blocks
            // conservatively; missing/invalid metadata requires operator inspection.
            if let Ok(text) = fs::read_to_string(legacy.join("pid"))
                && let Ok(pid) = text.trim().parse::<i32>()
                && pid > 0
                // SAFETY: signal zero only inspects the positive PID; EPERM is treated as live.
                && unsafe { kill(pid, 0) } != 0
                && io::Error::last_os_error().raw_os_error() == Some(3)
            {
                fs::remove_file(legacy.join("pid"))?;
                fs::remove_dir(legacy)?;
            }
            Ok(false)
        }
        Err(e) => Err(e),
    }
}
fn release_legacy(legacy: &Path) -> io::Result<()> {
    if fs::read_to_string(legacy.join("pid")).ok().as_deref() == Some(&process::id().to_string()) {
        fs::remove_file(legacy.join("pid"))?;
        fs::remove_dir(legacy)?;
    }
    Ok(())
}
fn run(root: &Path, legacy: &Path, args: &[std::ffi::OsString]) -> io::Result<i32> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(io::Error::other("queue root is not a directory"));
    }
    private(&metadata)?;
    let guard = mutex(root)?;
    let sequence = root.join("sequence");
    let next = match fs::read_to_string(&sequence) {
        Ok(s) => s
            .trim()
            .parse::<u64>()
            .map_err(io::Error::other)?
            .checked_add(1)
            .ok_or_else(|| io::Error::other("queue sequence exhausted"))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
        Err(e) => return Err(e),
    };
    // Publish the counter atomically. A crash may leave a gap, never a reused ticket.
    let mut counter = open(&root.join("sequence.next"))?;
    counter.set_len(0)?;
    counter.write_all(next.to_string().as_bytes())?;
    fs::rename(root.join("sequence.next"), sequence)?;
    let path = root.join(format!("{next:020}.job"));
    let mut slot = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    slot.lock()?;
    drop(guard);
    eprintln!("attest-pr: queued job {next}");
    let mut position = 0;
    loop {
        let guard = mutex(root)?;
        let active = remove_idle(root, &path)?;
        let current = active
            .iter()
            .position(|p| p == &path)
            .ok_or_else(|| io::Error::other("queue entry disappeared"))?
            + 1;
        if current != position {
            eprintln!("attest-pr: job {next} position {current}");
            position = current;
        }
        if SIGNAL.load(Ordering::Relaxed) != 0 {
            fs::remove_file(&path)?;
            return Ok(128 + SIGNAL.load(Ordering::Relaxed));
        }
        if current == 1 && legacy_ready(root, &path, legacy)? {
            break;
        }
        drop(guard);
        pause();
    }
    // Old waiters do not understand queue ownership and can reclaim a dead supervisor's
    // legacy PID while descendants remain. Drain old wrappers before upgrading; the
    // descendant and FIFO guarantees apply among participating new wrappers.
    // File locks normally close on exec. Keep this one alive in the command and descendants,
    // so killing this supervisor cannot release the queue slot while inherited work survives.
    // F_SETFD=2 and flags=0 are the Unix fcntl ABI on supported macOS/Linux hosts.
    // SAFETY: slot owns a valid descriptor; this changes only that descriptor's exec flag.
    if unsafe { fcntl(slot.as_raw_fd(), 2, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // A small launch barrier closes the spawn-to-metadata crash gap: the child records its own
    // process group before executing work, and retains the inherited lock if the parent dies.
    let mut child = Command::new(env::current_exe()?)
        .arg("--execute")
        .arg(&path)
        .args(args)
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    // The launcher writes through the same open file description; do not overwrite it here.
    slot.flush()?;
    eprintln!("attest-pr: running job {next}");
    let status = loop {
        let received = SIGNAL.swap(0, Ordering::Relaxed);
        if received != 0 {
            // SAFETY: a negative child PID addresses the dedicated command process group.
            unsafe {
                kill(-(pid as i32), received);
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        pause();
    };
    drop(slot);
    // Descendants may close inherited descriptors. Process-group liveness is an additional
    // conservative fence. Escaping the group and closing the descriptor is unsupported.
    while group_alive(pid)? {
        pause();
    }
    let guard = mutex(root)?;
    let probe = open(&path)?;
    while probe.try_lock().is_err() {
        pause();
    }
    fs::remove_file(&path)?;
    release_legacy(legacy)?;
    drop(guard);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
fn main() {
    // SAFETY: the handler only stores an atomic integer and never touches allocation or IO.
    for value in [1, 2, 15] {
        unsafe {
            signal(value, interrupted);
        }
    }
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|s| s == "--execute") {
        if args.len() < 3 {
            process::exit(2);
        }
        let path = Path::new(&args[1]);
        if let Err(e) = fs::write(path, process::id().to_string()) {
            eprintln!("attest-pr: cannot record child identity: {e}");
            process::exit(1);
        }
        let error = Command::new(&args[2]).args(&args[3..]).exec();
        eprintln!("attest-pr: cannot execute queued command: {error}");
        process::exit(if error.kind() == io::ErrorKind::NotFound {
            127
        } else {
            126
        });
    }
    if args.len() < 4 || args[0] != "--run" {
        eprintln!("usage: heavy-job-queue --run ROOT LEGACY COMMAND [ARG...]");
        process::exit(2);
    }
    match run(Path::new(&args[1]), Path::new(&args[2]), &args[3..]) {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("attest-pr: queue failed: {e}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        binary: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "agentscm-queue-test-{}-{}",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let binary = root.join("queue");
            assert!(
                Command::new("rustc")
                    .args([
                        "--edition=2024",
                        "-D",
                        "warnings",
                        "tools/heavy-job-queue.rs",
                        "-o"
                    ])
                    .arg(&binary)
                    .status()
                    .unwrap()
                    .success()
            );
            Self { root, binary }
        }
        fn start(&self, script: &str, args: &[&Path]) -> process::Child {
            Command::new(&self.binary)
                .arg("--run")
                .arg(self.root.join("state"))
                .arg(self.root.join("legacy"))
                .args(["sh", "-c", script, "queue-test"])
                .args(args)
                .spawn()
                .unwrap()
        }
        fn wait(&self, predicate: impl Fn() -> bool) {
            let start = Instant::now();
            while !predicate() {
                assert!(
                    start.elapsed() < Duration::from_secs(15),
                    "timed out: {}",
                    self.root.display()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        fn ticket_count(&self) -> usize {
            entries(&self.root.join("state"))
                .map(|e| e.len())
                .unwrap_or(0)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn send(child: &process::Child, value: i32) {
        // SAFETY: the child PID is a live synthetic test process.
        assert_eq!(unsafe { kill(child.id() as i32, value) }, 0);
    }
    #[test]
    fn fifo_cancellation_status_and_argument_boundaries() {
        let f = Fixture::new();
        let release = f.root.join("release");
        let started = f.root.join("started");
        let output = f.root.join("order");
        let mut first = f.start(
            "touch \"$1\"; while test ! -e \"$2\"; do sleep .05; done; printf 'first\\n' >> \"$3\"",
            &[&started, &release, &output],
        );
        f.wait(|| started.exists());
        let mut canceled = f.start("printf 'canceled\\n' >> \"$1\"", &[&output]);
        f.wait(|| f.ticket_count() == 2);
        let mut second = f.start(
            "printf '%s\\n' \"$2\" >> \"$1\"; exit 37",
            &[&output, Path::new("second argument ; $literal")],
        );
        f.wait(|| f.ticket_count() == 3);
        let mut third = f.start("printf 'third\\n' >> \"$1\"", &[&output]);
        f.wait(|| f.ticket_count() == 4);
        send(&canceled, 15);
        assert_eq!(canceled.wait().unwrap().code(), Some(143));
        fs::write(&release, "").unwrap();
        assert!(first.wait().unwrap().success());
        assert_eq!(second.wait().unwrap().code(), Some(37));
        assert!(third.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            "first\nsecond argument ; $literal\nthird\n"
        );
        assert_eq!(f.ticket_count(), 0);
    }
    #[test]
    fn killed_supervisor_and_closed_descriptor_descendant_remain_exclusive() {
        let f = Fixture::new();
        let started = f.root.join("started");
        let release = f.root.join("release");
        let next = f.root.join("next");
        // Python closes every extra descriptor. The process-group fence must still protect it.
        let mut first = Command::new(&f.binary).arg("--run").arg(f.root.join("state")).arg(f.root.join("legacy"))
            .args(["python3", "-c", "import os,sys,time; os.closerange(3,1024); open(sys.argv[1],'w').close();\nwhile not os.path.exists(sys.argv[2]): time.sleep(.02)"])
            .args([&started, &release]).spawn().unwrap();
        f.wait(|| started.exists());
        send(&first, 9);
        assert!(!first.wait().unwrap().success());
        let mut second = f.start("touch \"$1\"", &[&next]);
        f.wait(|| f.ticket_count() == 2);
        thread::sleep(Duration::from_millis(500));
        assert!(!next.exists());
        fs::write(release, "").unwrap();
        f.wait(|| next.exists());
        assert!(second.wait().unwrap().success());
        assert_eq!(f.ticket_count(), 0);
    }
    #[test]
    fn active_termination_and_empty_legacy_crash_recovery() {
        let f = Fixture::new();
        let state = f.root.join("state");
        fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
        fs::create_dir(f.root.join("legacy")).unwrap();
        fs::write(state.join("legacy-owner"), "00000000000000000000.job").unwrap();
        let started = f.root.join("started");
        let mut child = f.start("touch \"$1\"; while :; do sleep .1; done", &[&started]);
        f.wait(|| started.exists());
        send(&child, 15);
        assert_eq!(child.wait().unwrap().code(), Some(143));
        assert_eq!(f.ticket_count(), 0);
        assert!(!f.root.join("legacy").exists());
    }
    #[test]
    fn legacy_live_holder_and_dead_waiter_recovery() {
        let f = Fixture::new();
        let legacy = f.root.join("legacy");
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("pid"), process::id().to_string()).unwrap();
        let output = f.root.join("started");
        let mut canceled = f.start("touch \"$1\"", &[&output]);
        f.wait(|| f.ticket_count() == 1);
        send(&canceled, 9);
        canceled.wait().unwrap();
        let mut next = f.start("touch \"$1\"", &[&output]);
        f.wait(|| f.ticket_count() == 1);
        thread::sleep(Duration::from_millis(300));
        assert!(!output.exists());
        fs::remove_file(legacy.join("pid")).unwrap();
        fs::remove_dir(legacy).unwrap();
        f.wait(|| output.exists());
        assert!(next.wait().unwrap().success());
    }
}
