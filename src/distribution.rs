//! Local native bundle generation. Inventory hashes are not release qualification.
#[path = "distribution_materials.rs"]
mod distribution_materials;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use agentscm::store::digest;
use agentscm::{Error, Result};
use distribution_materials::{MATERIAL_DIRECTORIES, MATERIALS};
use serde::Serialize;
use sha2::{Digest, Sha256};

const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const EXECUTABLE_NAME: &str = if cfg!(windows) {
    "agentscm.exe"
} else {
    "agentscm"
};
const VERIFIER_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "verifier/agentscm-portable-verifier.exe"
} else {
    "verifier/agentscm-portable-verifier"
};

const MAIN_MATERIALS: [(&str, &[u8]); 3] = [
    ("LICENSE", include_bytes!("../LICENSE")),
    (
        "THIRD_PARTY_NOTICES.md",
        include_bytes!("../THIRD_PARTY_NOTICES.md"),
    ),
    ("Cargo.lock", include_bytes!("../Cargo.lock")),
];

#[derive(Debug, Serialize)]
pub struct NativeBundle {
    format: &'static str,
    packager_declaration: PackagerDeclaration,
    assurance: Assurance,
    files: Vec<BundleFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verifier: Option<Verifier>,
}

#[derive(Debug, Serialize)]
struct PackagerDeclaration {
    package_version: &'static str,
    operating_system: &'static str,
    architecture: &'static str,
    debug_assertions: bool,
}

#[derive(Debug, Serialize)]
struct Assurance {
    inventory_basis: &'static str,
    metadata_basis: &'static str,
    publisher_authentication: &'static str,
    build_provenance: &'static str,
    license_completeness: &'static str,
    release_support: &'static str,
}

#[derive(Debug, Serialize)]
struct BundleFile {
    path: &'static str,
    bytes: u64,
    sha256: String,
    executable: bool,
}

#[derive(Debug, Serialize)]
struct Verifier {
    executable: &'static str,
    material_prefix: &'static str,
    metadata_basis: &'static str,
    executable_material_linkage: &'static str,
    target_compatibility: &'static str,
}

/// Create a new v1 directory only. A successful manifest is written after its members.
/// On failure a partial directory can remain; callers must not treat it as complete.
pub fn package_native(output: &Path) -> Result<NativeBundle> {
    let executable = std::env::current_exe()?;
    create_output(output)?;
    let mut files = vec![copy_executable(&executable, output, MAX_EXECUTABLE_BYTES)?];
    write_main_materials(output, &mut files)?;
    let manifest = native_bundle("agentscm.native-bundle/v1", files, None);
    write_member(
        &output.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        false,
    )?;
    sync_completion_directories(output)?;
    Ok(manifest)
}

/// Create a new v2 directory containing an opaque verifier and compiled materials.
///
/// The verifier is opened and its stable final-component identity is checked before
/// the output directory is created. The retained handle is copied but never executed.
pub fn package_native_with_verifier(output: &Path, verifier: &Path) -> Result<NativeBundle> {
    let verifier = open_verifier(verifier)?;
    package_native_with_open_verifier(output, verifier)
}

fn package_native_with_open_verifier(output: &Path, mut verifier: File) -> Result<NativeBundle> {
    let executable = std::env::current_exe()?;
    create_output(output)?;

    let mut files = vec![copy_executable(&executable, output, MAX_EXECUTABLE_BYTES)?];
    write_main_materials(output, &mut files)?;
    create_material_directories(output)?;
    files.push(copy_open_executable(
        &mut verifier,
        output,
        VERIFIER_EXECUTABLE_NAME,
        MAX_EXECUTABLE_BYTES,
        true,
    )?);
    for material in MATERIALS {
        write_member(&output.join(material.destination), material.bytes, false)?;
        files.push(bundle_file(material.destination, material.bytes, false));
    }
    sync_material_directories(output)?;
    check_v2_payload_budget(&files)?;
    let manifest = native_bundle(
        "agentscm.native-bundle/v2",
        files,
        Some(Verifier {
            executable: VERIFIER_EXECUTABLE_NAME,
            material_prefix: "verifier/",
            metadata_basis: "compiled_source_materials",
            executable_material_linkage: "not_established",
            target_compatibility: "not_established",
        }),
    );
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::Limit(
            "native bundle manifest exceeds size limit".into(),
        ));
    }
    write_member(&output.join("manifest.json"), &manifest_bytes, false)?;
    sync_completion_directories(output)?;
    Ok(manifest)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn package_native_with_verifier_after_check(
    output: &Path,
    verifier: &Path,
    after_initial_check: impl FnOnce(),
) -> Result<NativeBundle> {
    let verifier = open_verifier_after_check(verifier, after_initial_check)?;
    package_native_with_open_verifier(output, verifier)
}

fn create_output(output: &Path) -> Result<()> {
    fs::create_dir(output).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict("native bundle output already exists".into())
        } else {
            Error::Io(error)
        }
    })
}

fn write_main_materials(output: &Path, files: &mut Vec<BundleFile>) -> Result<()> {
    for (name, bytes) in MAIN_MATERIALS {
        write_member(&output.join(name), bytes, false)?;
        files.push(bundle_file(name, bytes, false));
    }
    Ok(())
}

fn native_bundle(
    format: &'static str,
    files: Vec<BundleFile>,
    verifier: Option<Verifier>,
) -> NativeBundle {
    NativeBundle {
        format,
        packager_declaration: PackagerDeclaration {
            package_version: env!("CARGO_PKG_VERSION"),
            operating_system: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            debug_assertions: cfg!(debug_assertions),
        },
        assurance: Assurance {
            inventory_basis: "copied_bytes",
            metadata_basis: "packager_build_declaration",
            publisher_authentication: "not_established",
            build_provenance: "not_established",
            license_completeness: "not_established",
            release_support: "not_established",
        },
        files,
        verifier,
    }
}

fn bundle_file(path: &'static str, bytes: &[u8], executable: bool) -> BundleFile {
    BundleFile {
        path,
        bytes: bytes.len() as u64,
        sha256: digest(bytes),
        executable,
    }
}

fn check_v2_payload_budget(files: &[BundleFile]) -> Result<()> {
    let main_material_bytes = MAIN_MATERIALS
        .iter()
        .map(|(_, bytes)| bytes.len() as u64)
        .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
        .ok_or_else(|| Error::Limit("native bundle payload size overflow".into()))?;
    let compiled_bytes = MATERIALS
        .iter()
        .map(|material| material.bytes.len() as u64)
        .try_fold(main_material_bytes, |total, bytes| total.checked_add(bytes))
        .ok_or_else(|| Error::Limit("native bundle payload size overflow".into()))?;
    let maximum = MAX_EXECUTABLE_BYTES
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(compiled_bytes))
        .ok_or_else(|| Error::Limit("native bundle payload size overflow".into()))?;
    let actual = files
        .iter()
        .try_fold(0_u64, |total, file| total.checked_add(file.bytes))
        .ok_or_else(|| Error::Limit("native bundle payload size overflow".into()))?;
    if actual > maximum {
        return Err(Error::Limit(
            "native bundle exceeds payload size limit".into(),
        ));
    }
    Ok(())
}

fn copy_executable(source: &Path, output: &Path, maximum: u64) -> Result<BundleFile> {
    let mut input = File::open(source)?;
    if input.metadata()?.len() > maximum {
        return Err(Error::Limit(
            "native executable exceeds bundle size limit".into(),
        ));
    }
    copy_open_executable(&mut input, output, EXECUTABLE_NAME, maximum, false)
}

fn copy_open_executable(
    input: &mut File,
    output: &Path,
    name: &'static str,
    maximum: u64,
    require_nonempty: bool,
) -> Result<BundleFile> {
    let path = output.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let (bytes, sha256) = copy_limited(input, &mut file, maximum)?;
    if require_nonempty && bytes == 0 {
        return Err(Error::Invalid("native executable is empty".into()));
    }
    set_member_permissions(&file, true)?;
    file.sync_all()?;
    Ok(BundleFile {
        path: name,
        bytes,
        sha256,
        executable: true,
    })
}

fn copy_limited(
    input: &mut impl Read,
    output: &mut impl Write,
    maximum: u64,
) -> Result<(u64, String)> {
    let mut bytes = 0_u64;
    let mut sha256 = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if count as u64 > maximum.saturating_sub(bytes) {
            return Err(Error::Limit(
                "native executable exceeds bundle size limit".into(),
            ));
        }
        output.write_all(&buffer[..count])?;
        sha256.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((bytes, format!("{:x}", sha256.finalize())))
}

fn write_member(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    #[cfg(all(test, unix))]
    if TEST_WRITE_FAILURE.with(
        |failure| matches!(failure.get(), Some(WriteFault::Partial(name)) if path.ends_with(name)),
    ) {
        file.write_all(&bytes[..bytes.len().min(1)])?;
        return Err(Error::Io(std::io::Error::other(
            "injected native bundle partial member write",
        )));
    }
    file.write_all(bytes)?;
    set_member_permissions(&file, executable)?;
    #[cfg(all(test, unix))]
    if TEST_WRITE_FAILURE.with(
        |failure| matches!(failure.get(), Some(WriteFault::Sync(name)) if path.ends_with(name)),
    ) {
        return Err(Error::Io(std::io::Error::other(
            "injected native bundle member sync failure",
        )));
    }
    file.sync_all()?;
    Ok(())
}

fn create_material_directories(output: &Path) -> Result<()> {
    for directory in MATERIAL_DIRECTORIES {
        fs::create_dir(output.join(*directory))?;
    }
    Ok(())
}

fn sync_material_directories(output: &Path) -> Result<()> {
    #[cfg(unix)]
    for directory in MATERIAL_DIRECTORIES.iter().rev() {
        sync_directory(&output.join(*directory), SyncPoint::Child)?;
    }
    #[cfg(not(unix))]
    let _ = output;
    Ok(())
}

fn sync_completion_directories(output: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        sync_directory(output, SyncPoint::Root)?;
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        sync_directory(parent, SyncPoint::Parent)?;
    }
    #[cfg(not(unix))]
    let _ = output;
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncPoint {
    Child,
    Root,
    Parent,
}

#[cfg(unix)]
fn sync_directory(path: &Path, _point: SyncPoint) -> Result<()> {
    #[cfg(test)]
    if TEST_SYNC_FAILURE.with(|failure| failure.get() == Some(_point)) {
        return Err(Error::Io(std::io::Error::other(
            "injected native bundle directory sync failure",
        )));
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, unix))]
thread_local! {
    static TEST_SYNC_FAILURE: std::cell::Cell<Option<SyncPoint>> = const { std::cell::Cell::new(None) };
    static TEST_WRITE_FAILURE: std::cell::Cell<Option<WriteFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteFault {
    Partial(&'static str),
    Sync(&'static str),
}

fn set_member_permissions(file: &File, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o644
        }))?;
    }
    #[cfg(not(unix))]
    let _ = (file, executable);
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_verifier(path: &Path) -> Result<File> {
    open_verifier_after_check(path, || {})
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_verifier_after_check(path: &Path, after_initial_check: impl FnOnce()) -> Result<File> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    let initial = fs::symlink_metadata(path)?;
    if !initial.file_type().is_file() {
        return Err(Error::Invalid(
            "portable verifier must be a regular non-symlink file".into(),
        ));
    }
    after_initial_check();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    let final_path = fs::symlink_metadata(path)?;
    if !opened.file_type().is_file()
        || !final_path.file_type().is_file()
        || (initial.dev(), initial.ino()) != (opened.dev(), opened.ino())
        || (final_path.dev(), final_path.ino()) != (opened.dev(), opened.ino())
    {
        return Err(Error::Invalid(
            "portable verifier changed while it was opened".into(),
        ));
    }
    if opened.len() == 0 {
        return Err(Error::Invalid("portable verifier is empty".into()));
    }
    if opened.len() > MAX_EXECUTABLE_BYTES {
        return Err(Error::Limit(
            "native executable exceeds bundle size limit".into(),
        ));
    }
    Ok(file)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_verifier(_path: &Path) -> Result<File> {
    Err(Error::Invalid(
        "portable verifier packaging is unavailable on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Seek};

    const ZERO_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[cfg(unix)]
    struct SyncFailureGuard;

    #[cfg(unix)]
    impl Drop for SyncFailureGuard {
        fn drop(&mut self) {
            TEST_SYNC_FAILURE.with(|failure| failure.set(None));
        }
    }

    #[cfg(unix)]
    fn fail_sync_at(point: SyncPoint) -> SyncFailureGuard {
        TEST_SYNC_FAILURE.with(|failure| {
            assert!(failure.replace(Some(point)).is_none());
        });
        SyncFailureGuard
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    struct WriteFailureGuard;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    impl Drop for WriteFailureGuard {
        fn drop(&mut self) {
            TEST_WRITE_FAILURE.with(|failure| failure.set(None));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn fail_write_at(fault: WriteFault) -> WriteFailureGuard {
        TEST_WRITE_FAILURE.with(|failure| {
            assert!(failure.replace(Some(fault)).is_none());
        });
        WriteFailureGuard
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn regular_file_count(path: &Path) -> usize {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                if metadata.is_dir() {
                    regular_file_count(&path)
                } else {
                    assert!(metadata.is_file());
                    1
                }
            })
            .sum()
    }

    #[test]
    fn v1_serializer_matches_exact_golden() {
        let serialized = serde_json::to_vec_pretty(&synthetic_bundle(false)).unwrap();
        assert_eq!(
            serialized,
            include_bytes!("../tests/fixtures/native-bundle-v1.json")
        );
    }

    #[test]
    fn v2_serializer_matches_exact_golden() {
        let serialized = serde_json::to_vec_pretty(&synthetic_bundle(true)).unwrap();
        assert_eq!(
            serialized,
            include_bytes!("../tests/fixtures/native-bundle-v2.json")
        );
    }

    fn synthetic_bundle(with_verifier: bool) -> NativeBundle {
        let mut files = vec![synthetic_file("agentscm", true)];
        files.extend([
            synthetic_file("LICENSE", false),
            synthetic_file("THIRD_PARTY_NOTICES.md", false),
            synthetic_file("Cargo.lock", false),
        ]);
        let verifier = if with_verifier {
            files.push(synthetic_file("verifier/agentscm-portable-verifier", true));
            files.extend(
                MATERIALS
                    .iter()
                    .map(|material| synthetic_file(material.destination, false)),
            );
            Some(Verifier {
                executable: "verifier/agentscm-portable-verifier",
                material_prefix: "verifier/",
                metadata_basis: "compiled_source_materials",
                executable_material_linkage: "not_established",
                target_compatibility: "not_established",
            })
        } else {
            None
        };
        NativeBundle {
            format: if with_verifier {
                "agentscm.native-bundle/v2"
            } else {
                "agentscm.native-bundle/v1"
            },
            packager_declaration: PackagerDeclaration {
                package_version: "0.0.0-fixture",
                operating_system: "fixture-os",
                architecture: "fixture-arch",
                debug_assertions: false,
            },
            assurance: Assurance {
                inventory_basis: "copied_bytes",
                metadata_basis: "packager_build_declaration",
                publisher_authentication: "not_established",
                build_provenance: "not_established",
                license_completeness: "not_established",
                release_support: "not_established",
            },
            files,
            verifier,
        }
    }

    fn synthetic_file(path: &'static str, executable: bool) -> BundleFile {
        BundleFile {
            path,
            bytes: 1,
            sha256: ZERO_SHA256.into(),
            executable,
        }
    }

    #[test]
    fn streaming_limit_rejects_growth_without_writing_past_the_cap() {
        let mut input = Cursor::new(vec![7_u8; 16 * 1024 + 1]);
        let mut output = Vec::new();
        assert!(matches!(
            copy_limited(&mut input, &mut output, 16 * 1024),
            Err(Error::Limit(_))
        ));
        assert_eq!(output, vec![7_u8; 16 * 1024]);
    }

    #[test]
    fn streaming_copy_hashes_exact_bytes_at_the_cap() {
        let mut input = Cursor::new(b"exact");
        let mut output = Vec::new();
        assert_eq!(
            copy_limited(&mut input, &mut output, 5).unwrap(),
            (5, digest(b"exact"))
        );
        assert_eq!(output, b"exact");
    }

    #[test]
    fn streaming_copy_reports_write_failure_without_consuming_more_input() {
        struct FailingWriter {
            written: Vec<u8>,
            maximum: usize,
        }

        impl Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.written.len() == self.maximum {
                    return Err(std::io::Error::other("injected write failure"));
                }
                let count = bytes.len().min(self.maximum - self.written.len());
                self.written.extend_from_slice(&bytes[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut input = Cursor::new(vec![9_u8; 32 * 1024]);
        let mut output = FailingWriter {
            written: Vec::new(),
            maximum: 7,
        };
        assert!(matches!(
            copy_limited(&mut input, &mut output, 32 * 1024),
            Err(Error::Io(_))
        ));
        assert_eq!(output.written, vec![9_u8; 7]);
        assert_eq!(input.stream_position().unwrap(), 16 * 1024);
    }

    #[test]
    fn oversized_executable_never_has_a_completion_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"too large").unwrap();
        let output = directory.path().join("bundle");
        fs::create_dir(&output).unwrap();
        assert!(matches!(
            copy_executable(&source, &output, 2),
            Err(Error::Limit(_))
        ));
        assert!(!output.join("manifest.json").exists());
        assert!(!output.join(EXECUTABLE_NAME).exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn verifier_replacement_is_rejected_before_any_read() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("verifier");
        let original = directory.path().join("original");
        let output = directory.path().join("bundle");
        fs::write(&source, b"accepted bytes").unwrap();
        let result = package_native_with_verifier_after_check(&output, &source, || {
            fs::rename(&source, &original).unwrap();
            fs::write(&source, b"replacement bytes").unwrap();
        });
        assert!(matches!(result, Err(Error::Invalid(_))));
        assert!(!output.exists());
        assert_eq!(fs::read(&source).unwrap(), b"replacement bytes");
        assert_eq!(fs::read(&original).unwrap(), b"accepted bytes");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn verifier_final_symlink_is_rejected_before_any_read() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("verifier");
        let original = directory.path().join("original");
        let output = directory.path().join("bundle");
        fs::write(&source, b"accepted bytes").unwrap();
        let result = package_native_with_verifier_after_check(&output, &source, || {
            fs::rename(&source, &original).unwrap();
            std::os::unix::fs::symlink(&original, &source).unwrap();
        });
        assert!(result.is_err());
        assert!(!output.exists());
        assert!(
            fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn verifier_special_file_replacement_fails_without_blocking_or_reading() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("verifier");
        let original = directory.path().join("original");
        let output = directory.path().join("bundle");
        fs::write(&source, b"accepted bytes").unwrap();
        let result = package_native_with_verifier_after_check(&output, &source, || {
            fs::rename(&source, &original).unwrap();
            UnixListener::bind(&source).unwrap();
        });
        assert!(result.is_err());
        assert!(!output.exists());
        assert!(!fs::symlink_metadata(&source).unwrap().file_type().is_file());
        assert_eq!(fs::read(&original).unwrap(), b"accepted bytes");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn verifier_fifo_replacement_is_bounded_and_rejected_before_reading() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::Command;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("verifier");
        let original = directory.path().join("original");
        let output = directory.path().join("bundle");
        let prepared_fifo = directory.path().join("prepared-fifo");
        let mut mkfifo = Command::new("mkfifo").arg(&prepared_fifo).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(status) = mkfifo.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                let _ = mkfifo.kill();
                let _ = mkfifo.wait();
                panic!("mkfifo fixture creation exceeded its deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(&source, b"accepted bytes").unwrap();
        let worker_source = source.clone();
        let worker_original = original.clone();
        let worker_output = output.clone();
        let worker_fifo = prepared_fifo.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result =
                package_native_with_verifier_after_check(&worker_output, &worker_source, || {
                    fs::rename(&worker_source, &worker_original).unwrap();
                    fs::rename(&worker_fifo, &worker_source).unwrap();
                });
            sender.send(result.is_err()).unwrap();
        });

        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(rejected) => {
                worker.join().unwrap();
                assert!(rejected);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _unblock = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                    .open(&source)
                    .unwrap();
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Ok(_) => {
                        worker.join().unwrap();
                        panic!("portable verifier FIFO open blocked");
                    }
                    Err(_) => {
                        drop(worker);
                        panic!("portable verifier FIFO worker did not finish after unblock");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(worker);
                panic!("portable verifier FIFO worker disconnected without a result");
            }
        }
        assert!(!output.exists());
        assert_eq!(fs::read(&original).unwrap(), b"accepted bytes");
    }

    #[cfg(unix)]
    #[test]
    fn late_parent_sync_failure_retains_unknown_manifest_and_refuses_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("bundle");
        let _failure = fail_sync_at(SyncPoint::Parent);
        assert!(matches!(package_native(&output), Err(Error::Io(_))));
        let manifest = fs::read(output.join("manifest.json")).unwrap();
        serde_json::from_slice::<serde_json::Value>(&manifest).unwrap();
        assert!(matches!(package_native(&output), Err(Error::Conflict(_))));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn v2_sync_failures_preserve_pre_manifest_and_unknown_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let verifier = directory.path().join("portable-verifier");
        fs::write(&verifier, b"opaque verifier bytes").unwrap();

        let child_failure = directory.path().join("child-sync-failure");
        {
            let _failure = fail_sync_at(SyncPoint::Child);
            assert!(matches!(
                package_native_with_verifier(&child_failure, &verifier),
                Err(Error::Io(_))
            ));
        }
        assert!(!child_failure.join("manifest.json").exists());
        assert_eq!(regular_file_count(&child_failure), 94);

        let parent_failure = directory.path().join("parent-sync-failure");
        {
            let _failure = fail_sync_at(SyncPoint::Parent);
            assert!(matches!(
                package_native_with_verifier(&parent_failure, &verifier),
                Err(Error::Io(_))
            ));
        }
        let manifest = fs::read(parent_failure.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(manifest["format"], "agentscm.native-bundle/v2");
        assert_eq!(manifest["files"].as_array().unwrap().len(), 94);
        assert_eq!(regular_file_count(&parent_failure), 95);
        assert!(matches!(
            package_native_with_verifier(&parent_failure, &verifier),
            Err(Error::Conflict(_))
        ));

        let fresh_output = directory.path().join("fresh-bundle");
        assert!(package_native_with_verifier(&fresh_output, &verifier).is_ok());
        assert_eq!(regular_file_count(&fresh_output), 95);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn v2_member_write_and_sync_failures_never_promote_retained_outputs() {
        #[derive(Clone, Copy)]
        enum RetainedState {
            PartialMember,
            FullUnsyncedMember,
            PartialManifest,
            FullUnsyncedManifest,
        }

        for (fault, retained) in [
            (WriteFault::Partial("LICENSE"), RetainedState::PartialMember),
            (
                WriteFault::Sync("LICENSE"),
                RetainedState::FullUnsyncedMember,
            ),
            (
                WriteFault::Partial("manifest.json"),
                RetainedState::PartialManifest,
            ),
            (
                WriteFault::Sync("manifest.json"),
                RetainedState::FullUnsyncedManifest,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let verifier = directory.path().join("portable-verifier");
            let output = directory.path().join("failed-bundle");
            fs::write(&verifier, b"opaque verifier bytes").unwrap();

            let result = {
                let _failure = fail_write_at(fault);
                package_native_with_verifier(&output, &verifier)
            };
            assert!(matches!(result, Err(Error::Io(_))));
            assert!(output.is_dir());

            match retained {
                RetainedState::PartialMember => {
                    assert_eq!(
                        fs::read(output.join("LICENSE")).unwrap(),
                        &include_bytes!("../LICENSE")[..1]
                    );
                    assert!(!output.join("manifest.json").exists());
                }
                RetainedState::FullUnsyncedMember => {
                    assert_eq!(
                        fs::read(output.join("LICENSE")).unwrap(),
                        include_bytes!("../LICENSE")
                    );
                    assert!(!output.join("manifest.json").exists());
                }
                RetainedState::PartialManifest => {
                    let manifest = fs::read(output.join("manifest.json")).unwrap();
                    assert_eq!(manifest, b"{");
                    assert!(serde_json::from_slice::<serde_json::Value>(&manifest).is_err());
                }
                RetainedState::FullUnsyncedManifest => {
                    let manifest = fs::read(output.join("manifest.json")).unwrap();
                    let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
                    assert_eq!(manifest["format"], "agentscm.native-bundle/v2");
                    assert_eq!(manifest["files"].as_array().unwrap().len(), 94);
                    assert_eq!(regular_file_count(&output), 95);
                }
            }

            assert!(matches!(
                package_native_with_verifier(&output, &verifier),
                Err(Error::Conflict(_))
            ));
            let fresh_output = directory.path().join("fresh-bundle");
            assert!(package_native_with_verifier(&fresh_output, &verifier).is_ok());
            assert_eq!(regular_file_count(&fresh_output), 95);
        }
    }
}
