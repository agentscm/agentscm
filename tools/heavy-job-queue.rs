//! Machine-local FIFO for expensive validation. Compile with the pinned rustc, edition 2024.
//! Queue files contain only numeric tickets and process identities, never command arguments.
use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    const NOFOLLOW: i32 = 0x100 | 4;
    #[cfg(target_os = "linux")]
    const NOFOLLOW: i32 = 0x20000 | 2048;
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
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
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
const LIMIT: u64 = 16384;
fn read(path: &Path) -> io::Result<String> {
    let mut file = existing(path)?;
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        return Err(io::Error::other("oversized queue metadata"));
    }
    Ok(text)
}
fn existing(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "macos")]
    const NOFOLLOW: i32 = 0x100 | 4;
    #[cfg(target_os = "linux")]
    const NOFOLLOW: i32 = 0x20000 | 2048;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    private(&meta)?;
    if !meta.is_file() {
        return Err(io::Error::other("invalid queue metadata"));
    }
    Ok(file)
}
fn write(path: &Path, text: &str) -> io::Result<()> {
    let mut file = open(path)?;
    file.set_len(0)?;
    file.write_all(text.as_bytes())
}
fn bounded(command: &mut Command) -> io::Result<String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    #[cfg(target_os = "macos")]
    const NONBLOCK: i32 = 4;
    #[cfg(target_os = "linux")]
    const NONBLOCK: i32 = 2048;
    // SAFETY: stdout owns a valid pipe descriptor; preserve all flags except adding nonblocking.
    let configured = unsafe {
        let flags = fcntl(stdout.as_raw_fd(), 3);
        flags >= 0 && fcntl(stdout.as_raw_fd(), 4, flags | NONBLOCK) >= 0
    };
    if !configured {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::last_os_error());
    }
    let start = Instant::now();
    let mut data = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(io::Error::other("probe failed"));
                    }
                    return String::from_utf8(data).map_err(io::Error::other);
                }
            }
            Ok(n) => data.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if start.elapsed() > Duration::from_secs(3) || data.len() as u64 > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("probe timeout or output exceeded limit"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn group_alive(pid: u32) -> io::Result<bool> {
    // Bound the process-table snapshot; an oversized or failed probe blocks conservatively.
    let text = bounded(Command::new("ps").args(["-axo", "pgid=,stat="]))?;
    Ok(text.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|s| s.parse::<u32>().ok()) == Some(pid)
            && fields.next().is_some_and(|s| !s.starts_with('Z'))
    }))
}
fn validate_job(text: &str) -> io::Result<()> {
    let fields = text.lines().collect::<Vec<_>>();
    if fields.len() != 5
        || !["attestation", "focused", "other"].contains(&fields[1])
        || !fields[2].starts_with('"')
        || !fields[2].ends_with('"')
        || fields[3].parse::<u64>().is_err()
    {
        return Err(io::Error::other("malformed job metadata"));
    }
    Ok(())
}
fn recorded_group(path: &Path) -> io::Result<Option<u32>> {
    let text = read(path)?;
    validate_job(&text)?;
    let field = text
        .lines()
        .next()
        .ok_or_else(|| io::Error::other("missing job metadata"))?;
    let pid: u32 = field.parse().map_err(io::Error::other)?;
    Ok((pid != 0).then_some(pid))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn threshold(name: &str, default: u64) -> io::Result<u64> {
    match env::var(name) {
        Ok(s) => s.parse().map_err(io::Error::other),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(io::Error::other(e)),
    }
}
fn resources(repo: &Path) -> io::Result<()> {
    let minimum = threshold("AGENTSCM_HEAVY_MIN_MEMORY_PERCENT", 35)?;
    if minimum > 100 {
        return Err(io::Error::other("invalid memory threshold"));
    }
    #[cfg(target_os = "macos")]
    let memory: u64 = bounded(Command::new("sysctl").args(["-n", "kern.memorystatus_level"]))?
        .trim()
        .parse()
        .map_err(io::Error::other)?;
    #[cfg(target_os = "linux")]
    let memory: u64 = {
        let text = fs::read_to_string("/proc/meminfo")?;
        let value = |key: &str| -> io::Result<u64> {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.split_whitespace().next())
                .ok_or_else(|| io::Error::other("missing memory metric"))?
                .parse()
                .map_err(io::Error::other)
        };
        value("MemAvailable:")?
            .checked_mul(100)
            .and_then(|v| v.checked_div(value("MemTotal:").ok()?))
            .ok_or_else(|| io::Error::other("invalid memory metric"))?
    };
    if memory < minimum {
        return Err(io::Error::other(format!(
            "memory {memory}% below {minimum}%"
        )));
    }
    let minimum = threshold("AGENTSCM_HEAVY_MIN_DISK_BYTES", 10 * 1024 * 1024 * 1024)?;
    let mut paths = vec![repo.to_path_buf()];
    if let Some(target) = env::var_os("CARGO_TARGET_DIR") {
        let mut path = PathBuf::from(target);
        if path.is_relative() {
            path = repo.join(path);
        }
        while !path.exists() {
            if !path.pop() {
                return Err(io::Error::other("target filesystem unavailable"));
            }
        }
        paths.push(path);
    }
    for path in paths {
        let text = bounded(Command::new("df").args(["-Pk"]).arg(path))?;
        let available: u64 = text
            .lines()
            .nth(1)
            .and_then(|l| l.split_whitespace().nth(3))
            .ok_or_else(|| io::Error::other("missing disk metric"))?
            .parse()
            .map_err(io::Error::other)?;
        if available
            .checked_mul(1024)
            .ok_or_else(|| io::Error::other("invalid disk metric"))?
            < minimum
        {
            return Err(io::Error::other(format!("disk below {minimum} bytes")));
        }
    }
    Ok(())
}
fn status(root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("queue empty");
            return Ok(());
        }
        Err(e) => return Err(e),
        Ok(m) => {
            private(&m)?;
            if !m.is_dir() {
                return Err(io::Error::other("invalid queue root"));
            }
        }
    }
    let guard = existing(&root.join("mutex"))?;
    guard.lock()?;
    let mut count = 0;
    for path in entries(root)? {
        let text = read(&path)?;
        let fields = text.lines().collect::<Vec<_>>();
        if fields.len() != 5 || !["attestation", "focused", "other"].contains(&fields[1]) {
            return Err(io::Error::other("malformed job metadata"));
        }
        let pid = recorded_group(&path)?;
        let file = existing(&path)?;
        if file.try_lock().is_ok() && !pid.is_some_and(|p| group_alive(p).unwrap_or(true)) {
            continue;
        }
        count += 1;
        let since: u64 = fields[3].parse().map_err(io::Error::other)?;
        println!(
            "{} {} kind={} {}={}s pid={} repo={} blocked={}",
            if pid.is_some() { "holder" } else { "waiter" },
            count,
            fields[1],
            if pid.is_some() { "elapsed" } else { "waited" },
            now().saturating_sub(since),
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "pending".into()),
            fields[2],
            fields[4]
        );
    }
    if count == 0 {
        println!("queue empty");
    }
    Ok(())
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
    if let Ok(meta) = fs::symlink_metadata(legacy) {
        private(&meta)?;
        if !meta.is_dir() {
            return Err(io::Error::other("invalid legacy directory"));
        }
    }
    let previous = match read(&intent) {
        Ok(text) => {
            if text.len() != 24
                || !text.ends_with(".job")
                || !text.as_bytes()[..20].iter().all(|b| b.is_ascii_digit())
            {
                return Err(io::Error::other("invalid legacy owner"));
            }
            Some(text)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let legacy_empty = match read(&legacy.join("pid")) {
        Ok(text) => {
            if text.trim().parse::<u32>().ok().filter(|p| *p > 0).is_none() {
                return Err(io::Error::other("invalid legacy PID"));
            }
            false
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(e),
    };
    if let Some(previous) = previous
        && previous != own.file_name().unwrap().to_string_lossy()
        && !root.join(previous.trim()).exists()
        && legacy.is_dir()
        && legacy_empty
    {
        // A new supervisor died between mkdir and PID publication. All participants must
        // have upgraded: an old wrapper has no recoverable registration for this crash gap.
        fs::remove_dir(legacy)?;
    }
    write(&intent, &own.file_name().unwrap().to_string_lossy())?;
    match fs::DirBuilder::new().mode(0o700).create(legacy) {
        Ok(()) => {
            write(&legacy.join("pid"), &process::id().to_string())?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Existing old wrappers have no reliable birth identity. A reused live PID blocks
            // conservatively; missing/invalid metadata requires operator inspection.
            if let Ok(text) = read(&legacy.join("pid"))
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
    if read(&legacy.join("pid")).ok().as_deref() == Some(&process::id().to_string()) {
        fs::remove_file(legacy.join("pid"))?;
        fs::remove_dir(legacy)?;
    }
    Ok(())
}
fn run(root: &Path, legacy: &Path, kind: &str, args: &[std::ffi::OsString]) -> io::Result<i32> {
    if !["attestation", "focused", "other"].contains(&kind) {
        return Err(io::Error::other("invalid job kind"));
    }
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
    let next = match read(&sequence) {
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
    let repo = env::current_dir()?;
    let metadata = format!("0\n{kind}\n{:?}\n{}\nnone\n", repo, now());
    slot.write_all(metadata.as_bytes())?;
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
        if current == 1 {
            match resources(&repo) {
                Ok(()) => {
                    write(&path, &metadata)?;
                    if legacy_ready(root, &path, legacy)? {
                        break;
                    }
                    write(&path, &metadata.replace("\nnone\n", "\nlegacy holder\n"))?;
                }
                Err(error) => {
                    let reason = format!("{error:?}");
                    write(
                        &path,
                        &metadata.replace("\nnone\n", &format!("\n{reason}\n")),
                    )?;
                }
            }
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
        let recorded = (|| -> io::Result<()> {
            let _guard = mutex(
                path.parent()
                    .ok_or_else(|| io::Error::other("missing queue directory"))?,
            )?;
            let text = read(path)?;
            validate_job(&text)?;
            let fields = text.lines().collect::<Vec<_>>();
            write(
                path,
                &format!(
                    "{}\n{}\n{}\n{}\nnone\n",
                    process::id(),
                    fields[1],
                    fields[2],
                    now()
                ),
            )
        })();
        if let Err(e) = recorded {
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
    if args.len() == 2 && args[0] == "--queue" {
        if let Err(e) = status(Path::new(&args[1])) {
            eprintln!("attest-pr: queue failed: {e}");
            process::exit(1);
        }
        return;
    }
    if args.len() < 5 || args[0] != "--run" {
        eprintln!("usage: heavy-job-queue --run ROOT LEGACY COMMAND [ARG...]");
        process::exit(2);
    }
    match run(
        Path::new(&args[1]),
        Path::new(&args[2]),
        args[3].to_str().unwrap_or(""),
        &args[4..],
    ) {
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
    use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicU64};

    static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        binary: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "agentscm-queue-test-{}-{}-{}",
                process::id(),
                FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
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
            self.start_kind("other", script, args)
        }
        fn start_kind(&self, kind: &str, script: &str, args: &[&Path]) -> process::Child {
            Command::new(&self.binary)
                .arg("--run")
                .arg(self.root.join("state"))
                .arg(self.root.join("legacy"))
                .arg(kind)
                .env("AGENTSCM_HEAVY_MIN_MEMORY_PERCENT", "0")
                .env("AGENTSCM_HEAVY_MIN_DISK_BYTES", "0")
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
        let mut first = Command::new(&f.binary).arg("--run").arg(f.root.join("state")).arg(f.root.join("legacy")).arg("other").env("AGENTSCM_HEAVY_MIN_MEMORY_PERCENT", "0").env("AGENTSCM_HEAVY_MIN_DISK_BYTES", "0")
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
        fs::DirBuilder::new()
            .mode(0o700)
            .create(f.root.join("legacy"))
            .unwrap();
        fs::write(state.join("legacy-owner"), "00000000000000000000.job").unwrap();
        fs::set_permissions(
            state.join("legacy-owner"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
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
        fs::DirBuilder::new().mode(0o700).create(&legacy).unwrap();
        write(&legacy.join("pid"), &format!("{}\n", process::id())).unwrap();
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
    #[test]
    fn queue_status_is_read_only_and_orders_holder_and_waiters() {
        let f = Fixture::new();
        let output = Command::new(&f.binary)
            .arg("--queue")
            .arg(f.root.join("state"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!f.root.join("state").exists());
        let started = f.root.join("started");
        let release = f.root.join("release");
        let mut holder = f.start(
            "touch \"$1\"; while test ! -e \"$2\"; do sleep .05; done",
            &[&started, &release],
        );
        f.wait(|| started.exists());
        let mut a = f.start_kind("focused", "true", &[]);
        f.wait(|| f.ticket_count() == 2);
        let mut b = f.start_kind("attestation", "true", &[]);
        f.wait(|| f.ticket_count() == 3);
        let output = Command::new(&f.binary)
            .arg("--queue")
            .arg(f.root.join("state"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("holder 1 kind=other elapsed="));
        assert!(lines[1].starts_with("waiter 2 kind=focused waited="));
        assert!(lines[2].starts_with("waiter 3 kind=attestation waited="));
        assert!(text.contains("repo=\""));
        let fields = read(&f.root.join("state/00000000000000000001.job")).unwrap();
        let pid = fields.lines().next().unwrap();
        assert!(lines[0].contains(&format!(" pid={pid} ")));
        assert!(lines[1].contains(" pid=pending "));
        assert!(lines[2].contains(" pid=pending "));
        fs::write(release, "").unwrap();
        assert!(holder.wait().unwrap().success());
        assert!(a.wait().unwrap().success());
        assert!(b.wait().unwrap().success());
    }
    #[test]
    fn blocked_head_probes_alone_then_resumes() {
        let f = Fixture::new();
        let bin = f.root.join("bin");
        fs::create_dir(&bin).unwrap();
        let log = f.root.join("probes");
        let release = f.root.join("release");
        let memory_release = f.root.join("memory-release");
        let sysctl = bin.join("sysctl");
        fs::write(&sysctl, "#!/bin/sh\necho memory >> \"$PROBE_LOG\"\nif test -e \"$MEMORY_RELEASE\"; then echo 100; else echo 0; fi\n").unwrap();
        fs::set_permissions(&sysctl, fs::Permissions::from_mode(0o700)).unwrap();
        let df = bin.join("df");
        fs::write(&df, "#!/bin/sh\necho probe >> \"$PROBE_LOG\"\necho 'Filesystem 1024-blocks Used Available Capacity Mounted'\nif test -e \"$PROBE_RELEASE\"; then echo 'fake 999 0 999 0% /'; else echo 'fake 1 0 0 0% /'; fi\n").unwrap();
        fs::set_permissions(&df, fs::Permissions::from_mode(0o700)).unwrap();
        let start = || {
            Command::new(&f.binary)
                .arg("--run")
                .arg(f.root.join("state"))
                .arg(f.root.join("legacy"))
                .arg("focused")
                .args(["true"])
                .env(
                    "PATH",
                    format!("{}:{}", bin.display(), env::var("PATH").unwrap()),
                )
                .env("PROBE_LOG", &log)
                .env("PROBE_RELEASE", &release)
                .env("MEMORY_RELEASE", &memory_release)
                .env(
                    "AGENTSCM_HEAVY_MIN_MEMORY_PERCENT",
                    if cfg!(target_os = "macos") { "35" } else { "0" },
                )
                .env("AGENTSCM_HEAVY_MIN_DISK_BYTES", "1024")
                .spawn()
                .unwrap()
        };
        let mut head = start();
        f.wait(|| log.exists());
        let mut follower = start();
        f.wait(|| f.ticket_count() == 2);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        if cfg!(target_os = "macos") {
            let status = Command::new(&f.binary)
                .arg("--queue")
                .arg(f.root.join("state"))
                .output()
                .unwrap();
            assert!(
                String::from_utf8(status.stdout)
                    .unwrap()
                    .contains("memory 0% below 35%")
            );
            fs::write(&memory_release, "").unwrap();
            f.wait(|| {
                read(&f.root.join("state/00000000000000000001.job"))
                    .unwrap()
                    .contains("disk below")
            });
        }
        let output = Command::new(&f.binary)
            .arg("--queue")
            .arg(f.root.join("state"))
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("kind=focused"));
        assert!(text.contains("disk below 1024 bytes"));
        fs::write(release, "").unwrap();
        assert!(head.wait().unwrap().success());
        assert!(follower.wait().unwrap().success());
    }
    #[test]
    fn metadata_symlinks_malformed_and_oversized_fail_closed() {
        let f = Fixture::new();
        let outside = f.root.join("outside");
        fs::write(&outside, "untouched").unwrap();
        let link = f.root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(read(&link).is_err());
        assert!(write(&link, "overwrite").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
        let big = f.root.join("big");
        write(&big, &"x".repeat(LIMIT as usize + 1)).unwrap();
        assert!(read(&big).is_err());
        let job = f.root.join("bad.job");
        write(&job, "malformed").unwrap();
        assert!(recorded_group(&job).is_err());
    }
}
