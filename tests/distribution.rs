use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn run(directory: &TempDir, output: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(directory.path())
        .arg("--data-dir")
        .arg(directory.path().join("not-a-repository"))
        .arg("package-native")
        .arg(output)
        .output()
        .unwrap()
}

fn run_with_verifier(directory: &TempDir, output: &Path, verifier: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(directory.path())
        .arg("--data-dir")
        .arg(directory.path().join("not-a-repository"))
        .arg("package-native")
        .arg(output)
        .arg("--portable-verifier")
        .arg(verifier)
        .output()
        .unwrap()
}

fn inventory(path: &Path) -> (u64, String) {
    let mut file = File::open(path).unwrap();
    let mut bytes = 0;
    let mut sha256 = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        bytes += count as u64;
        sha256.update(&buffer[..count]);
    }
    (bytes, format!("{:x}", sha256.finalize()))
}

#[test]
fn native_bundle_is_self_contained_and_matches_its_inventory() {
    let directory = TempDir::new().unwrap();
    let bundle = directory.path().join("bundle with spaces");
    let result = run(&directory, Path::new("bundle with spaces"));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(response["ok"], true);
    let manifest: Value =
        serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(response["manifest"], manifest);
    assert_eq!(manifest["format"], "agentscm.native-bundle/v1");
    assert_eq!(
        manifest["packager_declaration"]["package_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        manifest["packager_declaration"]["operating_system"],
        std::env::consts::OS
    );
    assert_eq!(
        manifest["packager_declaration"]["architecture"],
        std::env::consts::ARCH
    );
    assert_eq!(
        manifest["packager_declaration"]["debug_assertions"],
        cfg!(debug_assertions)
    );
    assert_eq!(manifest["assurance"]["inventory_basis"], "copied_bytes");
    assert_eq!(
        manifest["assurance"]["metadata_basis"],
        "packager_build_declaration"
    );
    for field in [
        "publisher_authentication",
        "build_provenance",
        "license_completeness",
        "release_support",
    ] {
        assert_eq!(manifest["assurance"][field], "not_established");
    }
    let entries = manifest["files"].as_array().unwrap();
    let name = if cfg!(windows) {
        "agentscm.exe"
    } else {
        "agentscm"
    };
    let members: BTreeSet<_> = entries
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap(),
                entry["executable"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        members,
        BTreeSet::from([
            (name, true),
            ("LICENSE", false),
            ("THIRD_PARTY_NOTICES.md", false),
            ("Cargo.lock", false),
        ])
    );
    assert_eq!(entries.len(), 4);
    assert_eq!(members.len(), entries.len());
    assert_eq!(fs::read_dir(&bundle).unwrap().count(), 5);
    for entry in entries {
        let path = entry["path"].as_str().unwrap();
        let (bytes, sha256) = inventory(&bundle.join(path));
        assert_eq!(entry["bytes"].as_u64().unwrap(), bytes);
        assert_eq!(entry["sha256"], sha256);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let expected = if entry["executable"].as_bool().unwrap() {
                0o755
            } else {
                0o644
            };
            assert_eq!(
                fs::metadata(bundle.join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                expected
            );
        }
    }
    assert_eq!(
        inventory(&bundle.join(name)),
        inventory(Path::new(env!("CARGO_BIN_EXE_agentscm")))
    );
    assert_eq!(
        fs::read(bundle.join("LICENSE")).unwrap(),
        include_bytes!("../LICENSE")
    );
    assert_eq!(
        fs::read(bundle.join("THIRD_PARTY_NOTICES.md")).unwrap(),
        include_bytes!("../THIRD_PARTY_NOTICES.md")
    );
    assert_eq!(
        fs::read(bundle.join("Cargo.lock")).unwrap(),
        include_bytes!("../Cargo.lock")
    );
    let installed = Command::new(bundle.join(name))
        .current_dir(directory.path())
        .arg("--version")
        .output()
        .unwrap();
    assert!(installed.status.success());
    // A build from a branch also names its commit: `0.1.0+<commit>`.
    let printed = String::from_utf8(installed.stdout).unwrap();
    assert!(
        printed
            .trim()
            .strip_prefix(concat!("agentscm ", env!("CARGO_PKG_VERSION")))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('+')),
        "{printed}"
    );
    assert!(!directory.path().join(".agentscm").exists());
    assert!(!directory.path().join("not-a-repository").exists());
}

#[test]
fn native_bundle_refuses_existing_outputs_and_missing_parent_without_overwrite() {
    let directory = TempDir::new().unwrap();
    let output = directory.path().join("existing");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("keep"), b"unchanged").unwrap();
    let result = run(&directory, &output);
    assert!(!result.status.success());
    assert!(
        serde_json::from_slice::<Value>(&result.stderr).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(fs::read(output.join("keep")).unwrap(), b"unchanged");
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
    let output = directory.path().join("missing parent").join("bundle");
    assert!(!run(&directory, &output).status.success());
    assert!(!output.exists());
    assert!(!output.parent().unwrap().exists());
}

#[cfg(unix)]
#[test]
fn native_bundle_refuses_symlink_output_without_touching_the_target() {
    let directory = TempDir::new().unwrap();
    let target = directory.path().join("preserve");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), b"unchanged").unwrap();
    let output = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &output).unwrap();
    assert!(!run(&directory, &output).status.success());
    assert_eq!(fs::read(target.join("keep")).unwrap(), b"unchanged");
    assert_eq!(fs::read_dir(target).unwrap().count(), 1);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn bundle_paths(root: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut pending = vec![root.to_path_buf()];
    let mut files = BTreeSet::new();
    let mut directory_paths = BTreeSet::new();
    let mut directories = 0;
    while let Some(directory) = pending.pop() {
        directories += 1;
        assert!(directories <= 128);
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                directory_paths.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                pending.push(path);
            } else {
                assert!(metadata.is_file());
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                assert!(files.len() <= 95);
            }
        }
    }
    (files, directory_paths)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_v2_copies_exact_fixed_materials_and_never_executes_the_verifier() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let verifier = directory.path().join("opaque verifier input");
    let verifier_bytes = b"#!/bin/sh\ntouch verifier-was-executed\nexit 99\n";
    fs::write(&verifier, verifier_bytes).unwrap();
    fs::set_permissions(&verifier, fs::Permissions::from_mode(0o755)).unwrap();
    let bundle = directory.path().join("new v2 bundle");
    let result = run_with_verifier(&directory, &bundle, &verifier);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!directory.path().join("verifier-was-executed").exists());
    assert!(!directory.path().join("not-a-repository").exists());
    assert!(!directory.path().join(".agentscm").exists());

    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["manifest"], manifest);
    assert_eq!(manifest["format"], "agentscm.native-bundle/v2");
    assert_eq!(
        manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "format",
            "packager_declaration",
            "assurance",
            "files",
            "verifier"
        ])
    );
    assert_eq!(
        manifest["verifier"],
        serde_json::json!({
            "executable": "verifier/agentscm-portable-verifier",
            "material_prefix": "verifier/",
            "metadata_basis": "compiled_source_materials",
            "executable_material_linkage": "not_established",
            "target_compatibility": "not_established"
        })
    );
    for key in [
        "publisher_authentication",
        "build_provenance",
        "license_completeness",
        "release_support",
    ] {
        assert_eq!(manifest["assurance"][key], "not_established");
    }

    // The reviewed synthetic fixture independently fixes all expected paths.
    let expected: Value =
        serde_json::from_slice(include_bytes!("fixtures/native-bundle-v2.json")).unwrap();
    let expected_paths: BTreeSet<String> = expected["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .chain(["manifest.json".to_owned()])
        .collect();
    assert_eq!(expected_paths.len(), 95);
    let expected_directories: BTreeSet<String> = expected_paths
        .iter()
        .flat_map(|path| {
            Path::new(path)
                .ancestors()
                .skip(1)
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| parent.to_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(expected_directories.len(), 43);
    let (actual_files, actual_directories) = bundle_paths(&bundle);
    assert_eq!(actual_files, expected_paths);
    assert_eq!(actual_directories, expected_directories);

    let entries = manifest["files"].as_array().unwrap();
    assert_eq!(entries.len(), 94);
    let actual_paths: Vec<_> = entries
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    let fixed_paths: Vec<_> = expected["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(actual_paths, fixed_paths);
    for entry in entries {
        assert_eq!(
            entry
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["path", "bytes", "sha256", "executable"])
        );
        let relative = entry["path"].as_str().unwrap();
        let path = bundle.join(relative);
        let (bytes, sha256) = inventory(&path);
        assert_eq!(entry["bytes"].as_u64().unwrap(), bytes);
        assert_eq!(entry["sha256"], sha256);
        let executable =
            relative == "agentscm" || relative == "verifier/agentscm-portable-verifier";
        assert_eq!(entry["executable"], executable);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            if executable { 0o755 } else { 0o644 }
        );
        if let Some(material) = relative
            .strip_prefix("verifier/")
            .filter(|path| *path != "agentscm-portable-verifier")
        {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tools/portable-verifier")
                .join(material);
            assert_eq!(fs::read(path).unwrap(), fs::read(source).unwrap());
        }
    }
    assert_eq!(
        inventory(&bundle.join("agentscm")),
        inventory(Path::new(env!("CARGO_BIN_EXE_agentscm")))
    );
    assert_eq!(
        fs::read(bundle.join("verifier/agentscm-portable-verifier")).unwrap(),
        verifier_bytes
    );
    for name in ["LICENSE", "THIRD_PARTY_NOTICES.md", "Cargo.lock"] {
        assert_eq!(
            fs::read(bundle.join(name)).unwrap(),
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(name)).unwrap()
        );
    }
    for name in ["profile.md", "vectors.json"] {
        assert_eq!(
            fs::read(bundle.join("verifier/protocol/evidence-v1").join(name)).unwrap(),
            fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("protocol/evidence-v1")
                    .join(name)
            )
            .unwrap()
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_v2_rejects_invalid_verifier_inputs_before_creating_output() {
    let directory = TempDir::new().unwrap();
    let empty = directory.path().join("empty");
    fs::write(&empty, []).unwrap();
    let oversized = directory.path().join("oversized");
    File::create(&oversized)
        .unwrap()
        .set_len(128 * 1024 * 1024 + 1)
        .unwrap();
    let link = directory.path().join("linked-verifier");
    std::os::unix::fs::symlink(&empty, &link).unwrap();
    for (index, input) in [
        directory.path().join("missing"),
        empty,
        oversized,
        link,
        directory.path().to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let output = directory.path().join(format!("rejected-{index}"));
        let result = run_with_verifier(&directory, &output, input);
        assert!(!result.status.success());
        let error: Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert!(!output.exists());
    }
    assert!(!directory.path().join("not-a-repository").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_v2_refuses_existing_or_symlink_outputs_and_missing_parents() {
    let directory = TempDir::new().unwrap();
    let verifier = directory.path().join("verifier");
    fs::write(&verifier, b"opaque bytes").unwrap();
    let existing = directory.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"unchanged").unwrap();
    assert!(
        !run_with_verifier(&directory, &existing, &verifier)
            .status
            .success()
    );
    assert_eq!(fs::read_dir(&existing).unwrap().count(), 1);
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"unchanged");
    let linked = directory.path().join("linked-output");
    std::os::unix::fs::symlink(&existing, &linked).unwrap();
    assert!(
        !run_with_verifier(&directory, &linked, &verifier)
            .status
            .success()
    );
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"unchanged");
    let missing = directory.path().join("missing-parent").join("bundle");
    assert!(
        !run_with_verifier(&directory, &missing, &verifier)
            .status
            .success()
    );
    assert!(!missing.parent().unwrap().exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_v2_stdout_failure_keeps_the_existing_destination_unreusable() {
    use std::process::Stdio;
    use std::thread;
    use std::time::{Duration, Instant};

    let directory = TempDir::new().unwrap();
    let verifier = directory.path().join("verifier");
    fs::write(&verifier, b"opaque verifier bytes").unwrap();
    let output = directory.path().join("stdout-failure");
    let (reader, writer) = std::io::pipe().unwrap();
    // No reader exists before the child starts, so its first stdout write
    // fails after package_native_with_verifier has returned successfully.
    drop(reader);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(directory.path())
        .env("RUST_BACKTRACE", "0")
        .arg("--data-dir")
        .arg(directory.path().join("not-a-repository"))
        .arg("package-native")
        .arg(&output)
        .arg("--portable-verifier")
        .arg(&verifier)
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native bundle stdout-failure child exceeded its deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("failed printing to stdout"));
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "agentscm.native-bundle/v2");
    assert_eq!(manifest["files"].as_array().unwrap().len(), 94);
    // Readable content is not a durability receipt or permission to reuse it.
    let (files, directories) = bundle_paths(&output);
    assert_eq!(files.len(), 95);
    assert_eq!(directories.len(), 43);
    assert!(
        !run_with_verifier(&directory, &output, &verifier)
            .status
            .success()
    );
    let fresh_output = directory.path().join("fresh-after-stdout-failure");
    assert!(
        run_with_verifier(&directory, &fresh_output, &verifier)
            .status
            .success()
    );
    assert!(!directory.path().join("not-a-repository").exists());
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[test]
fn native_v2_is_explicitly_unavailable_without_the_safe_input_profile() {
    let directory = TempDir::new().unwrap();
    let verifier = directory.path().join("verifier");
    fs::write(&verifier, b"opaque bytes").unwrap();
    let output = directory.path().join("new-bundle");
    assert!(
        !run_with_verifier(&directory, &output, &verifier)
            .status
            .success()
    );
    assert!(!output.exists());
}
