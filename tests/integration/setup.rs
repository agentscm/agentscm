//! Source fixtures only until explicitly executed; no existing installation is used.
#![cfg(target_os = "macos")]
#[path = "../../src/setup.rs"]
#[allow(clippy::module_inception)] // the source under test keeps its own name
mod setup;

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use agentscm::authority::Authority;
use agentscm::local_recovery::IssueWorkflowRecoveryConfig;
use agentscm::store::Engine;
use serde_json::{Value, json};
use setup::{Blocker, Check, FreshRequest, OwnedRequest, Profile, SetupError};
use tempfile::TempDir;

fn fixture() -> (TempDir, FreshRequest) {
    let temp = TempDir::new().unwrap();
    let parent = fs::canonicalize(temp.path()).unwrap();
    (
        temp,
        FreshRequest {
            root: parent.join("installation"),
            profile: Profile::TrustedLocalNative,
        },
    )
}

fn install(request: &FreshRequest) -> setup::DoctorReport {
    let owned = setup::begin(request).unwrap();
    setup::resume(&owned).unwrap()
}

fn receipt(request: &OwnedRequest) -> Value {
    serde_json::from_slice(&fs::read(request.root.join(".agentscm-installation.json")).unwrap())
        .unwrap()
}

fn write_receipt(request: &OwnedRequest, value: &Value) {
    fs::write(
        request.root.join(".agentscm-installation.json"),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

#[test]
fn native_setup_fresh_and_exact_resume_preserve_identity_without_claiming_service_or_login() {
    let (_temp, request) = fixture();
    let first = install(&request);
    let owned = OwnedRequest {
        root: first.root.clone(),
        installation_id: first.installation_id.clone(),
    };
    let original = fs::read(owned.root.join(".agentscm-installation.json")).unwrap();
    let resumed = setup::resume(&owned).unwrap();
    assert_eq!(first.native, resumed.native);
    assert_eq!(
        original,
        fs::read(owned.root.join(".agentscm-installation.json")).unwrap()
    );
    assert!(matches!(resumed.native, Check::Observed { .. }));
    assert_eq!(resumed.service_health, Check::NotRun);
    assert_eq!(resumed.operator_authentication, Check::NotConfigured);
    assert_eq!(resumed.model, Check::NotConfigured);
    assert_eq!(resumed.team, Check::Unsupported);
    assert_eq!(resumed.platform_services, Check::Unsupported);
    assert_eq!(resumed.host_resources, Check::NotRun);
    assert_eq!(
        fs::metadata(&owned.root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(owned.root.join(".agentscm-installation.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!owned.root.join("data/.agentscm-installation.json").exists());
}

#[test]
fn native_setup_claimed_interruption_can_resume_actual_native_initialization() {
    let (_temp, request) = fixture();
    let owned = setup::begin(&request).unwrap();
    assert!(!owned.root.join("data").exists());
    assert_eq!(
        setup::doctor(&owned).unwrap().native,
        Check::Blocked {
            reason: Blocker::NativeNotInitialized
        }
    );
    let report = setup::resume(&owned).unwrap();
    let authority = Authority::open_read_only(owned.root.join("data")).unwrap();
    assert_eq!(
        report.native,
        Check::Observed {
            detail: authority.repository_id().unwrap()
        }
    );
    assert!(owned.root.join("bundle/manifest.json").is_file());
}

#[test]
fn native_setup_partial_native_state_is_retained_and_never_adopted_or_reinitialized() {
    let (_temp, request) = fixture();
    let owned = setup::begin(&request).unwrap();
    let mut value = receipt(&owned);
    value["progress"] = json!({"step":"initializing_native"});
    write_receipt(&owned, &value);
    let _engine = Engine::open(owned.root.join("data")).unwrap();
    let authority = Authority::open(owned.root.join("data")).unwrap();
    let before = authority.repository_id().unwrap();
    drop(authority);
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::Blocked(
            Blocker::InterruptedNativeInitialization
        ))
    ));
    assert_eq!(
        Authority::open_read_only(owned.root.join("data"))
            .unwrap()
            .repository_id()
            .unwrap(),
        before
    );
    assert!(!owned.root.join("bundle").exists());
    assert_eq!(receipt(&owned)["progress"], value["progress"]);
}

#[test]
fn native_setup_partial_bundle_and_receipt_write_are_retained() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let mut value = receipt(&owned);
    value["progress"] =
        json!({"step":"packaging_native", "native": value["progress"]["native"].clone()});
    write_receipt(&owned, &value);
    let bundle = fs::read(owned.root.join("bundle/manifest.json")).unwrap();
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::Blocked(Blocker::InterruptedPackaging))
    ));
    assert_eq!(
        fs::read(owned.root.join("bundle/manifest.json")).unwrap(),
        bundle
    );
    fs::write(
        owned.root.join(".agentscm-installation.next"),
        b"interrupted write",
    )
    .unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::Blocked(Blocker::InterruptedReceiptWrite))
    ));
    assert_eq!(
        fs::read(owned.root.join(".agentscm-installation.next")).unwrap(),
        b"interrupted write"
    );
}

#[test]
fn native_setup_foreign_receipt_stale_binary_and_changed_bundle_are_refused() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let foreign = OwnedRequest {
        root: owned.root.clone(),
        installation_id: "f".repeat(32),
    };
    assert!(matches!(
        setup::resume(&foreign),
        Err(SetupError::ForeignInstallation)
    ));
    let original = receipt(&owned);
    let mut stale = original.clone();
    stale["executable_sha256"] = json!("0".repeat(64));
    write_receipt(&owned, &stale);
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    write_receipt(&owned, &original);
    fs::write(owned.root.join("bundle/LICENSE"), b"changed").unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
}

#[test]
fn native_setup_replaced_native_repository_is_not_healed_or_accepted() {
    let (temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let other = fs::canonicalize(temp.path()).unwrap().join("foreign-data");
    Engine::open(&other).unwrap();
    let authority = Authority::open(&other).unwrap();
    let foreign_id = authority.repository_id().unwrap();
    drop(authority);
    fs::copy(
        other.join("state.sqlite"),
        owned.root.join("data/state.sqlite"),
    )
    .unwrap();
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    assert_eq!(
        Authority::open_read_only(owned.root.join("data"))
            .unwrap()
            .repository_id()
            .unwrap(),
        foreign_id
    );
}

#[test]
fn native_setup_existing_symlink_relative_overlap_and_writable_parent_are_refused() {
    let (temp, request) = fixture();
    fs::create_dir(&request.root).unwrap();
    fs::write(request.root.join("keep"), b"untouched").unwrap();
    assert!(matches!(
        setup::begin(&request),
        Err(SetupError::ExistingPath)
    ));
    assert_eq!(fs::read(request.root.join("keep")).unwrap(), b"untouched");
    let parent = fs::canonicalize(temp.path()).unwrap();
    let alias = parent.join("alias");
    symlink(&request.root, &alias).unwrap();
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: alias,
            ..request.clone()
        }),
        Err(SetupError::InvalidPath)
    ));
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: PathBuf::from("relative"),
            ..request.clone()
        }),
        Err(SetupError::InvalidPath)
    ));
    let owned = setup::begin(&FreshRequest {
        root: parent.join("owned"),
        ..request.clone()
    })
    .unwrap();
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: owned.root.join("nested"),
            ..request.clone()
        }),
        Err(SetupError::InvalidPath)
    ));
    let writable = parent.join("writable");
    fs::create_dir(&writable).unwrap();
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: writable.join("new"),
            ..request
        }),
        Err(SetupError::InvalidPath)
    ));
}

#[test]
fn native_setup_isolated_self_check_never_adds_source_or_authority_events_to_installation_data() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let report = setup::self_check(&owned).unwrap();
    assert!(matches!(report.self_check, Check::Recorded { .. }));
    assert_eq!(
        fs::read_dir(owned.root.join("data/objects"))
            .unwrap()
            .count(),
        0
    );
    let main = Authority::open_read_only(owned.root.join("data")).unwrap();
    let sample = Authority::open_read_only(owned.root.join("self-check/data")).unwrap();
    assert_ne!(
        main.repository_id().unwrap(),
        sample.repository_id().unwrap()
    );
    assert!(main.events(0, 10).unwrap().is_empty());
    assert!(main.target("main").unwrap().is_none());
    assert_eq!(
        fs::read(owned.root.join("self-check/workspace/sample.txt")).unwrap(),
        b"AgentSCM isolated setup self-check.\n"
    );
    assert!(matches!(
        setup::self_check(&owned),
        Err(SetupError::Blocked(Blocker::SelfCheckAlreadyRecorded))
    ));
}

#[test]
fn native_setup_unsupported_profiles_make_no_installation_directory() {
    let (_temp, mut request) = fixture();
    for profile in [Profile::Team, Profile::LinuxService, Profile::ModelAssisted] {
        request.profile = profile;
        assert!(matches!(
            setup::begin(&request),
            Err(SetupError::UnsupportedProfile(_))
        ));
        assert!(!request.root.exists());
    }
}

#[test]
fn native_setup_busy_and_oversized_or_public_receipts_are_refused_without_repair() {
    let (_temp, request) = fixture();
    let owned = setup::begin(&request).unwrap();
    let lock = fs::File::open(owned.root.join(".agentscm-setup.lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(setup::resume(&owned), Err(SetupError::Busy)));
    drop(lock);
    let path = owned.root.join(".agentscm-installation.json");
    fs::write(&path, vec![b'x'; 16 * 1024 + 1]).unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    assert_eq!(fs::metadata(path).unwrap().len(), 16 * 1024 + 1);
    assert!(!owned.root.join("data").exists());
}

#[test]
fn native_setup_owned_request_remains_available_after_post_claim_resume_failure() {
    let (_temp, request) = fixture();
    let owned = setup::begin(&request).unwrap();
    let id = owned.installation_id.clone();
    let lock = fs::File::open(owned.root.join(".agentscm-setup.lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(setup::resume(&owned), Err(SetupError::Busy)));
    drop(lock);
    // The typed value returned before initialization is sufficient; no private JSON read.
    assert_eq!(owned.installation_id, id);
    assert_eq!(setup::resume(&owned).unwrap().installation_id, id);
}

#[test]
fn native_setup_rejects_self_consistent_bundle_for_a_different_executable_pin() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let replacement = b"different executable bytes";
    fs::write(owned.root.join("bundle/agentscm"), replacement).unwrap();
    let path = owned.root.join("bundle/manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let member = manifest["files"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["path"] == "agentscm")
        .unwrap();
    member["sha256"] = json!(agentscm::store::digest(replacement));
    member["bytes"] = json!(replacement.len());
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(&path, &bytes).unwrap();
    let mut retained = receipt(&owned);
    retained["progress"]["manifest_sha256"] = json!(agentscm::store::digest(&bytes));
    assert_ne!(
        retained["executable_sha256"],
        json!(agentscm::store::digest(replacement))
    );
    write_receipt(&owned, &retained);
    // No updater timing or current-process image mutation: the validator sees a
    // valid inventory whose executable differs only from the retained receipt pin.
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    assert_eq!(
        fs::read(owned.root.join("bundle/agentscm")).unwrap(),
        replacement
    );
}

#[test]
fn native_setup_bundle_inventory_requires_execute_parity_and_exact_immediate_entries() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let bundle = owned.root.join("bundle");
    for (name, bad, original) in [
        ("agentscm", 0o644, 0o755),
        ("LICENSE", 0o755, 0o644),
        ("manifest.json", 0o755, 0o644),
    ] {
        fs::set_permissions(bundle.join(name), fs::Permissions::from_mode(bad)).unwrap();
        assert!(matches!(
            setup::doctor(&owned),
            Err(SetupError::ForeignInstallation)
        ));
        fs::set_permissions(bundle.join(name), fs::Permissions::from_mode(original)).unwrap();
    }
    let extra = bundle.join("extra");
    fs::write(&extra, b"not inventoried").unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    fs::remove_file(&extra).unwrap();
    fs::create_dir(&extra).unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    fs::remove_dir(&extra).unwrap();
    symlink(owned.root.join(".agentscm-installation.json"), &extra).unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
    fs::remove_file(&extra).unwrap();
    let saved = owned.root.join("saved-license");
    fs::rename(bundle.join("LICENSE"), &saved).unwrap();
    symlink(&saved, bundle.join("LICENSE")).unwrap();
    assert!(matches!(
        setup::doctor(&owned),
        Err(SetupError::ForeignInstallation)
    ));
}

#[test]
fn native_setup_non_utf8_root_or_parent_is_refused_before_any_claim() {
    let (temp, request) = fixture();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let invalid = parent.join(OsString::from_vec(b"invalid-\xff".to_vec()));
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: invalid.clone(),
            ..request.clone()
        }),
        Err(SetupError::InvalidPath)
    ));
    assert!(!invalid.exists());
    let child = invalid.join("installation");
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: child.clone(),
            ..request.clone()
        }),
        Err(SetupError::InvalidPath)
    ));
    assert!(!child.exists());
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    let represented = match fs::create_dir(&invalid) {
        Ok(()) => true,
        Err(error) => {
            // APFS may reject the filename itself. Require that exact platform
            // refusal, then still exercise nested setup refusal below.
            const { assert!(cfg!(target_os = "macos")) };
            assert_eq!(error.raw_os_error(), Some(libc::EILSEQ));
            assert!(!invalid.exists());
            false
        }
    };
    assert!(matches!(
        setup::begin(&FreshRequest {
            root: child.clone(),
            ..request
        }),
        Err(SetupError::InvalidPath)
    ));
    assert!(!child.exists());
    if represented {
        assert_eq!(fs::read_dir(&invalid).unwrap().count(), 0);
    } else {
        assert!(!invalid.exists());
    }
    assert_eq!(
        fs::read_dir(parent).unwrap().count(),
        if represented { 1 } else { 0 }
    );
}

#[test]
fn native_setup_bare_git_ancestors_are_refused_before_claim_without_source_mutation() {
    let (temp, request) = fixture();
    let bare = fs::canonicalize(temp.path()).unwrap().join("source.git");
    fs::create_dir(&bare).unwrap();
    fs::create_dir(bare.join("objects")).unwrap();
    fs::create_dir(bare.join("refs")).unwrap();
    fs::create_dir(bare.join("refs/heads")).unwrap();
    fs::create_dir(bare.join("nested")).unwrap();
    let head = b"ref: refs/heads/main\n";
    let config = b"[core]\n\trepositoryformatversion = 0\n\tbare = true\n";
    let source = b"retained source object sentinel";
    fs::write(bare.join("HEAD"), head).unwrap();
    fs::write(bare.join("config"), config).unwrap();
    fs::write(bare.join("objects/retained"), source).unwrap();
    assert!(!bare.join(".git").exists());
    // Faithful bare-repository markers exercise direct and deeper ancestor refusal.
    // No Git process, candidate repository code or real user source is involved.
    for root in [bare.join("installation"), bare.join("nested/installation")] {
        assert!(matches!(
            setup::begin(&FreshRequest {
                root: root.clone(),
                ..request.clone()
            }),
            Err(SetupError::InvalidPath)
        ));
        assert!(!root.exists());
    }
    assert_eq!(fs::read(bare.join("HEAD")).unwrap(), head);
    assert_eq!(fs::read(bare.join("config")).unwrap(), config);
    assert_eq!(fs::read(bare.join("objects/retained")).unwrap(), source);
    let mut names = fs::read_dir(&bare)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    let expected = ["HEAD", "config", "nested", "objects", "refs"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
    assert_eq!(fs::read_dir(bare.join("nested")).unwrap().count(), 0);
}

#[test]
fn native_setup_writes_external_recovery_profile_from_actual_native_and_owned_ids() {
    let (_temp, request) = fixture();
    let owned = setup::begin(&request).unwrap();
    let report = setup::resume(&owned).unwrap();
    let path = owned.root.join(setup::ISSUE_RECOVERY_CONFIG_FILE);
    assert_eq!(report.issue_recovery_config_path, path);
    assert!(matches!(
        report.issue_recovery_config,
        Check::Observed { .. }
    ));
    let native = Authority::open_read_only(owned.root.join("data"))
        .unwrap()
        .repository_id()
        .unwrap();
    let actual = IssueWorkflowRecoveryConfig::read(&path).unwrap();
    assert_eq!(actual.native_repository(), native);
    assert_eq!(actual.installation_id(), owned.installation_id);
    let expected = IssueWorkflowRecoveryConfig::new(&native, &owned.installation_id).unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!owned.root.join("data/issue-recovery.json").exists());
    assert!(!owned.root.join("data/issue-workflow").exists());
}

#[test]
fn native_setup_recovery_profile_checks_each_identifier_without_rewriting_mismatch() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let path = owned.root.join(setup::ISSUE_RECOVERY_CONFIG_FILE);
    let native = Authority::open_read_only(owned.root.join("data"))
        .unwrap()
        .repository_id()
        .unwrap();
    let other_native = if native == "f".repeat(32) {
        "e".repeat(32)
    } else {
        "f".repeat(32)
    };
    for profile in [
        IssueWorkflowRecoveryConfig::new(&other_native, &owned.installation_id).unwrap(),
        IssueWorkflowRecoveryConfig::new(&native, "different-installation").unwrap(),
    ] {
        let bytes = serde_json::to_vec(&profile).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(IssueWorkflowRecoveryConfig::read(&path).unwrap(), profile);
        assert_eq!(
            setup::doctor(&owned).unwrap().issue_recovery_config,
            Check::Blocked {
                reason: Blocker::IssueRecoveryConfigMismatch
            }
        );
        assert!(matches!(
            setup::resume(&owned),
            Err(SetupError::Blocked(Blocker::IssueRecoveryConfigMismatch))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn native_setup_recovery_profile_corrupt_extra_fields_noncanonical_private_and_missing_states_block()
 {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let path = owned.root.join(setup::ISSUE_RECOVERY_CONFIG_FILE);
    let original = fs::read(&path).unwrap();
    let mut extra: Value = serde_json::from_slice(&original).unwrap();
    extra["extra"] = json!(true);
    for bytes in [
        b"not-json".to_vec(),
        serde_json::to_vec(&extra).unwrap(),
        vec![b' '; 16 * 1024 + 1],
    ] {
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            setup::doctor(&owned).unwrap().issue_recovery_config,
            Check::Blocked {
                reason: Blocker::IssueRecoveryConfigInvalid
            }
        );
        assert!(matches!(
            setup::resume(&owned),
            Err(SetupError::Blocked(Blocker::IssueRecoveryConfigInvalid))
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let mut noncanonical = vec![b' '];
    noncanonical.extend_from_slice(&original);
    assert!(IssueWorkflowRecoveryConfig::from_json(&noncanonical).is_ok());
    fs::write(&path, &noncanonical).unwrap();
    assert_eq!(
        setup::doctor(&owned).unwrap().issue_recovery_config,
        Check::Blocked {
            reason: Blocker::IssueRecoveryConfigMismatch
        }
    );
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        setup::doctor(&owned).unwrap().issue_recovery_config,
        Check::Blocked {
            reason: Blocker::IssueRecoveryConfigInvalid
        }
    );
    fs::remove_file(&path).unwrap();
    assert_eq!(
        setup::doctor(&owned).unwrap().issue_recovery_config,
        Check::Blocked {
            reason: Blocker::IssueRecoveryConfigMissing
        }
    );
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::Blocked(Blocker::IssueRecoveryConfigMissing))
    ));
    assert!(!path.exists());
}

#[test]
fn native_setup_recovery_profile_symlink_and_uncertain_written_state_are_not_adopted() {
    let (_temp, request) = fixture();
    let report = install(&request);
    let owned = OwnedRequest {
        root: report.root,
        installation_id: report.installation_id,
    };
    let path = owned.root.join(setup::ISSUE_RECOVERY_CONFIG_FILE);
    let original = fs::read(&path).unwrap();
    let saved = owned.root.join("retained-config.json");
    fs::rename(&path, &saved).unwrap();
    symlink(&saved, &path).unwrap();
    assert_eq!(
        setup::doctor(&owned).unwrap().issue_recovery_config,
        Check::Blocked {
            reason: Blocker::IssueRecoveryConfigInvalid
        }
    );
    assert!(matches!(
        setup::resume(&owned),
        Err(SetupError::Blocked(Blocker::IssueRecoveryConfigInvalid))
    ));
    assert_eq!(fs::read(&saved).unwrap(), original);
    fs::remove_file(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    let mut retained = receipt(&owned);
    let native = retained["progress"]["native"].clone();
    for (step, reason) in [
        (
            "writing_issue_recovery_config",
            Blocker::InterruptedIssueRecoveryConfig,
        ),
        ("native_initialized", Blocker::UnexpectedIssueRecoveryConfig),
    ] {
        retained["progress"] = json!({"step":step,"native":native.clone()});
        write_receipt(&owned, &retained);
        let pending_bytes = if step == "writing_issue_recovery_config" {
            b"{\"version\":".to_vec()
        } else {
            original.clone()
        };
        fs::write(&path, &pending_bytes).unwrap();
        assert_eq!(
            setup::doctor(&owned).unwrap().issue_recovery_config,
            Check::Blocked { reason }
        );
        match setup::resume(&owned) {
            Err(SetupError::Blocked(actual)) => assert_eq!(actual, reason),
            other => panic!("expected retained profile blocker, got {other:?}"),
        }
        assert_eq!(fs::read(&path).unwrap(), pending_bytes);
        assert_eq!(receipt(&owned)["progress"], retained["progress"]);
    }
}
