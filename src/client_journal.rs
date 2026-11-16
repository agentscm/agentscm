//! One private journal for native lifecycle operations and enqueue evidence, never heartbeats.
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const VERSION: u8 = 1;
const MAX_RECORDS: usize = 512;
const MAX_BYTES: u64 = 64 * 1024;
const LOCK_WAIT: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Registration,
    TerminalEnd,
    Enqueue,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Prepared,
    Admitted,
    Outcome,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) id: String,
    pub(crate) kind: Kind,
    pub(crate) phase: Phase,
    pub(crate) payload: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    record: Record,
}

#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub(crate) records: Vec<Record>,
    pub(crate) stalled: usize,
}

impl Scan {
    pub(crate) fn full(&self) -> bool {
        self.records.len() + self.stalled >= MAX_RECORDS
    }
}

fn invalid() -> io::Error {
    io::Error::other("native operation journal is unavailable or changed")
}

fn directory(config: &Path) -> PathBuf {
    config.join("agentscm").join("native-journal")
}

fn private(metadata: &std::fs::Metadata, directory: bool) -> bool {
    if if directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    } {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == if directory { 0o700 } else { 0o600 }
            && (directory || metadata.nlink() == 1)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// The shared config parent may be searchable; only its native journal must be private.
fn shared_directory(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn open(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !private(&file.metadata()?, false) {
        return Err(invalid());
    }
    Ok(file)
}

pub(crate) fn locked<T>(config: &Path, run: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    locked_until(config, Instant::now() + LOCK_WAIT, run)
}

pub(crate) fn locked_until<T>(
    config: &Path,
    deadline: Instant,
    run: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let directory = directory(config);
    for path in [
        config,
        directory.parent().ok_or_else(invalid)?,
        directory.as_path(),
    ] {
        let builder = &mut std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if path == config {
            if !metadata.is_dir() {
                return Err(invalid());
            }
            continue;
        }
        let valid = if path == directory.as_path() {
            private(&metadata, true)
        } else {
            shared_directory(&metadata)
        };
        if !valid {
            return Err(invalid());
        }
    }
    // The blocking host recorder can wait behind durable writes within its hook budget.
    let lease = loop {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match agentscm::lease::Lease::try_exclusive(open(&directory.join("journal.lock"), true)?) {
            Ok(lease) => break lease,
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(5)),
                );
            }
            Err(error) => return Err(error.into()),
        }
    };
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let result = run();
    drop(lease);
    result
}

fn path(config: &Path, id: &str) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(directory(config).join(format!("{:x}.json", Sha256::digest(id))))
}

fn read(path: &Path) -> io::Result<Record> {
    let file = open(path, false)?;
    if file.metadata()?.len() > MAX_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if saved.version != VERSION || bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    Ok(saved.record)
}

fn write(config: &Path, record: &Record) -> io::Result<()> {
    let destination = path(config, &record.id)?;
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if !private(&metadata, false) => return Err(invalid()),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let bytes = serde_json::to_vec(&Saved {
        version: VERSION,
        record: record.clone(),
    })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let mut file = tempfile::NamedTempFile::new_in(directory(config))?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(&destination).map_err(|error| error.error)?;
    File::open(directory(config))?.sync_all()?;
    Ok(())
}

pub(crate) fn put(config: &Path, record: &Record) -> io::Result<()> {
    locked(config, || {
        let destination = path(config, &record.id)?;
        if std::fs::symlink_metadata(&destination).is_ok() {
            return if read(&destination)? == *record {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        if std::fs::read_dir(directory(config))?
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))
            })
            .count()
            >= MAX_RECORDS
        {
            return Err(io::Error::other(
                "native operation journal is full; pending evidence preserved",
            ));
        }
        write(config, record)
    })
}

pub(crate) fn update(config: &Path, id: &str, phase: Phase, payload: Value) -> io::Result<()> {
    locked(config, || {
        let mut record = read(&path(config, id)?)?;
        if record.id != id
            || matches!(
                (record.phase, phase),
                (
                    Phase::Admitted | Phase::Unknown | Phase::Outcome,
                    Phase::Prepared
                ) | (Phase::Outcome, Phase::Admitted | Phase::Unknown)
                    | (Phase::Unknown, Phase::Admitted)
            )
        {
            return Err(invalid());
        }
        record.phase = phase;
        record.payload = payload;
        write(config, &record)
    })
}

/// Only one waiter may cross the durable boundary before invoking a native queue command.
pub(crate) fn admit_enqueue(config: &Path, expected: &Record) -> io::Result<bool> {
    if expected.kind != Kind::Enqueue || expected.phase != Phase::Prepared {
        return Err(invalid());
    }
    locked(config, || {
        if read(&path(config, &expected.id)?)? != *expected {
            return Ok(false);
        }
        let mut admitted = expected.clone();
        admitted.phase = Phase::Admitted;
        write(config, &admitted)?;
        Ok(true)
    })
}

/// An end intent admits one exact request even when the resident and native hook race.
pub(crate) fn compare_and_update(
    config: &Path,
    expected: &Record,
    next: &Record,
) -> io::Result<Record> {
    if expected.id != next.id
        || expected.kind != next.kind
        || expected.phase != Phase::Prepared
        || next.phase != Phase::Admitted
    {
        return Err(invalid());
    }
    locked(config, || {
        let current = read(&path(config, &expected.id)?)?;
        if current.id != expected.id || current.kind != expected.kind {
            return Err(invalid());
        }
        if current != *expected {
            return Ok(current);
        }
        write(config, next)?;
        Ok(next.clone())
    })
}

pub(crate) fn scan(config: &Path) -> io::Result<Scan> {
    locked(config, || inspect(config))
}

/// Status reads never create directories or locks, and atomic records need no reader lock.
pub(crate) fn inspect(config: &Path) -> io::Result<Scan> {
    let directory = directory(config);
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Scan::default()),
        Err(error) => return Err(error),
    };
    if !private(&metadata, true)
        || !shared_directory(&std::fs::symlink_metadata(
            directory.parent().ok_or_else(invalid)?,
        )?)
    {
        return Err(invalid());
    }
    let mut scan = Scan::default();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == "journal.lock" {
            continue;
        }
        if scan.full() {
            return Err(invalid());
        }
        match read(&entry.path()) {
            Ok(record) if path(config, &record.id)? == entry.path() => scan.records.push(record),
            _ => scan.stalled += 1,
        }
    }
    scan.records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(scan)
}

pub(crate) fn remove(config: &Path, id: &str) -> io::Result<()> {
    locked(config, || {
        let path = path(config, id)?;
        match read(&path) {
            Ok(record) if record.id == id => std::fs::remove_file(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => Err(invalid()),
        }
    })
}

/// A settled reply cannot remove a replacement admitted by a concurrent retry.
pub(crate) fn remove_if(config: &Path, expected: &Record) -> io::Result<()> {
    locked(config, || {
        let path = path(config, &expected.id)?;
        match read(&path) {
            Ok(record) if record == *expected => std::fs::remove_file(path),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Record {
        Record {
            id: "attempt".into(),
            kind: Kind::Enqueue,
            phase: Phase::Prepared,
            payload: json!({"observed_at":10}),
        }
    }

    #[test]
    fn inspecting_absent_state_creates_nothing() {
        let config = tempfile::tempdir().unwrap();
        assert!(inspect(config.path()).unwrap().records.is_empty());
        assert!(!config.path().join("agentscm").exists());
    }

    #[test]
    fn caller_budget_waits_beyond_the_default_lock_cap() {
        let config = tempfile::tempdir().unwrap();
        locked(config.path(), || Ok(())).unwrap();
        let lease = agentscm::lease::Lease::try_exclusive(
            open(&directory(config.path()).join("journal.lock"), true).unwrap(),
        )
        .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(LOCK_WAIT * 2);
                drop(lease);
            });
            locked_until(
                config.path(),
                Instant::now() + Duration::from_secs(2),
                || write(config.path(), &record()),
            )
            .unwrap();
        });
        assert_eq!(inspect(config.path()).unwrap().records, [record()]);
    }

    #[test]
    fn expired_caller_budget_does_not_run_the_write() {
        let config = tempfile::tempdir().unwrap();
        let error = locked_until::<()>(config.path(), Instant::now(), || {
            panic!("expired write must not run")
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(inspect(config.path()).unwrap().records.is_empty());
    }

    #[test]
    fn persistent_lock_contention_is_bounded_and_does_not_run_the_write() {
        let config = tempfile::tempdir().unwrap();
        locked(config.path(), || Ok(())).unwrap();
        let lease = agentscm::lease::Lease::try_exclusive(
            open(&directory(config.path()).join("journal.lock"), true).unwrap(),
        )
        .unwrap();
        let started = Instant::now();
        let error =
            locked::<()>(config.path(), || panic!("contended write must not run")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() >= LOCK_WAIT);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(inspect(config.path()).unwrap().records.is_empty());
        drop(lease);
        put(config.path(), &record()).unwrap();
    }

    #[test]
    fn an_existing_searchable_config_parent_keeps_the_journal_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let config = tempfile::tempdir().unwrap();
        let parent = config.path().join("agentscm");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let expected = record();
        put(config.path(), &expected).unwrap();
        assert_eq!(inspect(config.path()).unwrap().records, [expected]);
        assert_eq!(std::fs::metadata(&parent).unwrap().mode() & 0o777, 0o755);
        let journal = directory(config.path());
        assert!(private(&std::fs::symlink_metadata(&journal).unwrap(), true));
        for entry in std::fs::read_dir(&journal).unwrap() {
            assert!(private(&entry.unwrap().metadata().unwrap(), false));
        }
        for mode in [0o775, 0o777] {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(put(config.path(), &record()).is_err());
            assert!(inspect(config.path()).is_err());
        }
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(put(config.path(), &record()).is_err());
        assert!(inspect(config.path()).is_err());
    }

    #[test]
    fn a_shared_config_parent_symlink_is_refused_without_changing_its_target() {
        let config = tempfile::tempdir().unwrap();
        put(config.path(), &record()).unwrap();
        let parent = config.path().join("agentscm");
        let outside = config.path().join("outside");
        std::fs::rename(&parent, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &parent).unwrap();
        let retained = outside
            .join("native-journal")
            .join(path(config.path(), "attempt").unwrap().file_name().unwrap());
        let before = std::fs::read(&retained).unwrap();
        assert!(put(config.path(), &record()).is_err());
        assert!(inspect(config.path()).is_err());
        assert_eq!(std::fs::read(retained).unwrap(), before);
    }

    #[test]
    fn enqueue_attempt_has_only_one_dispatch_admission() {
        let config = tempfile::tempdir().unwrap();
        let record = record();
        put(config.path(), &record).unwrap();
        assert!(admit_enqueue(config.path(), &record).unwrap());
        assert!(!admit_enqueue(config.path(), &record).unwrap());
        assert!(put(config.path(), &record).is_err());
        assert_eq!(
            inspect(config.path()).unwrap().records[0].phase,
            Phase::Admitted
        );
    }

    #[test]
    fn capacity_is_visible_and_never_discards_uncertain_attempts() {
        let config = tempfile::tempdir().unwrap();
        locked(config.path(), || {
            for index in 0..MAX_RECORDS {
                let mut item = record();
                item.id = format!("retained-{index}");
                item.phase = Phase::Outcome;
                item.payload = json!({"disposition":"revoked", "outcome":"unknown"});
                write(config.path(), &item)?;
            }
            Ok(())
        })
        .unwrap();
        let before = inspect(config.path()).unwrap();
        assert!(before.full());
        assert!(put(config.path(), &record()).is_err());
        assert_eq!(inspect(config.path()).unwrap().records, before.records);
    }

    #[test]
    fn competing_admissions_keep_the_first_exact_request() {
        let config = tempfile::tempdir().unwrap();
        let expected = record();
        put(config.path(), &expected).unwrap();
        let mut first = expected.clone();
        first.phase = Phase::Admitted;
        first.payload = json!({"generation":1,"updated_seq":7});
        let mut second = first.clone();
        second.payload["updated_seq"] = json!(8);
        assert_eq!(
            compare_and_update(config.path(), &expected, &first).unwrap(),
            first
        );
        assert_eq!(
            compare_and_update(config.path(), &expected, &second).unwrap(),
            first
        );
        assert_eq!(inspect(config.path()).unwrap().records, [first]);
    }

    #[test]
    fn admitted_or_unknown_enqueue_cannot_be_prepared_again() {
        let config = tempfile::tempdir().unwrap();
        put(config.path(), &record()).unwrap();
        update(
            config.path(),
            "attempt",
            Phase::Admitted,
            json!({"observed_at":10}),
        )
        .unwrap();
        assert!(put(config.path(), &record()).is_err());
        update(
            config.path(),
            "attempt",
            Phase::Unknown,
            json!({"observed_at":10}),
        )
        .unwrap();
        assert!(update(config.path(), "attempt", Phase::Prepared, json!({})).is_err());
        assert_eq!(
            scan(config.path()).unwrap().records[0].phase,
            Phase::Unknown
        );
    }

    #[test]
    fn unknown_version_and_corrupt_entries_are_preserved() {
        let config = tempfile::tempdir().unwrap();
        put(config.path(), &record()).unwrap();
        let path = path(config.path(), "attempt").unwrap();
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\":1", "\"version\":2");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(scan(config.path()).unwrap().stalled, 1);
        assert!(remove(config.path(), "attempt").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[test]
    fn a_symlink_is_never_followed() {
        let config = tempfile::tempdir().unwrap();
        put(config.path(), &record()).unwrap();
        let path = path(config.path(), "attempt").unwrap();
        std::fs::remove_file(&path).unwrap();
        let outside = config.path().join("outside");
        std::fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(put(config.path(), &record()).is_err());
        assert_eq!(scan(config.path()).unwrap().stalled, 1);
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
    }
}
