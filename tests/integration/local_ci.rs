use std::collections::BTreeMap;

use agentscm::context::{DependencyClaim, DependencyState};
use agentscm::local_ci::*;
use agentscm::local_validation::GitValidationSubject;

fn source(commit: char) -> SourceIdentity {
    SourceIdentity::Git {
        subject: GitValidationSubject {
            repository: "example/project".into(),
            commit: commit.to_string().repeat(40),
            tree: "b".repeat(40),
        },
    }
}

fn resources() -> ResourceRequirements {
    ResourceRequirements {
        timeout_secs: 7200,
        cpu_millis: 6000,
        memory_bytes: 8 << 30,
        disk_bytes: 32 << 30,
    }
}

fn cell(id: &str) -> RequiredCell {
    RequiredCell {
        job: "mobile".into(),
        id: id.into(),
        matrix: BTreeMap::from([("platform".into(), id.into())]),
        command: CommandSpec {
            argv: vec!["bun".into(), "run".into(), format!("e2e:{id}")],
            working_directory: ".".into(),
            environment: BTreeMap::from([("CI".into(), "1".into())]),
            reviewed_script: None,
            resources: resources(),
        },
        runner: RunnerProfile {
            id: "mobile-host".into(),
            sha256: "c".repeat(64),
            platform: id.into(),
            architecture: "arm64".into(),
            toolchain_sha256: "d".repeat(64),
        },
        services: vec![ServiceRequirement {
            id: "database".into(),
            sha256: "e".repeat(64),
            environment: BTreeMap::new(),
            resources: resources(),
        }],
        dependencies: vec![DependencyClaim {
            identity: "lockfile".into(),
            state: DependencyState::Resolved {
                revision: "f".repeat(64),
            },
        }],
        required_capabilities: vec!["command/v1".into(), "mobile-simulator/v1".into()],
    }
}

pub(super) fn requirements() -> Requirements {
    Requirements {
        version: VERSION,
        repository: "example/project".into(),
        policy_epoch: 3,
        workflows: vec![
            WorkflowBinding::from_bytes(
                "ci/commands.json".into(),
                WorkflowFormat::Commands,
                b"synthetic workflow\n",
            )
            .unwrap(),
        ],
        cells: vec![cell("ios"), cell("android")],
        permitted_omissions: vec![],
    }
}

pub(super) fn plan() -> ImmutablePlan {
    let requirements = requirements();
    ImmutablePlan {
        version: VERSION,
        subject: PlanSubject::PrHead {
            head: source('a'),
            base: source('c'),
        },
        configuration: ConfigBinding {
            source: source('d'),
            path: "ci/requirements.json".into(),
            sha256: "1".repeat(64),
        },
        policy_epoch: requirements.policy_epoch,
        requirements_sha256: requirements.digest().unwrap(),
        workflows: requirements.workflows,
        event_inputs: BTreeMap::from([("event".into(), "pull_request".into())]),
        cells: requirements.cells,
        omissions: vec![],
    }
}

#[test]
fn canonical_round_trip_and_frozen_digests() {
    let requirements = requirements();
    assert_eq!(
        Requirements::from_slice(&requirements.canonical_bytes().unwrap())
            .unwrap()
            .digest()
            .unwrap(),
        requirements.digest().unwrap()
    );
    let plan = plan();
    assert_eq!(
        ImmutablePlan::from_slice(&plan.canonical_bytes().unwrap())
            .unwrap()
            .digest()
            .unwrap(),
        plan.digest().unwrap()
    );
    assert_eq!(
        requirements.digest().unwrap(),
        "76dc1cd863bc9c0a64aaadb5e3782bd15448fe83a5326f9842c17bdc994cf887"
    );
    assert_eq!(
        plan.digest().unwrap(),
        "187ba06afb79af032ea5d3841e598c28db22a5c2405cd5f23077e1d20ab37ec9"
    );
}

#[test]
fn set_order_is_canonical_but_command_and_workflow_bytes_are_exact() {
    let original = requirements();
    let mut reordered = original.clone();
    reordered.cells.reverse();
    reordered.cells[0].required_capabilities.reverse();
    assert_eq!(original.digest().unwrap(), reordered.digest().unwrap());
    reordered.cells[0].command.argv.reverse();
    assert_ne!(original.digest().unwrap(), reordered.digest().unwrap());
    let a = WorkflowBinding::from_bytes("script.sh".into(), WorkflowFormat::Commands, b"echo ok\n")
        .unwrap();
    let b = WorkflowBinding::from_bytes("script.sh".into(), WorkflowFormat::Commands, b"echo ok")
        .unwrap();
    assert_ne!(a.sha256, b.sha256);
}

#[test]
fn exact_subject_policy_inputs_and_platform_change_plan_digest() {
    let original = plan();
    let mut variants = vec![];
    let mut changed = original.clone();
    changed.policy_epoch += 1;
    variants.push(changed);
    let mut changed = original.clone();
    changed.requirements_sha256 = "2".repeat(64);
    variants.push(changed);
    let mut changed = original.clone();
    changed.configuration.sha256 = "2".repeat(64);
    variants.push(changed);
    let mut changed = original.clone();
    changed.configuration.source = source('e');
    variants.push(changed);
    let mut changed = original.clone();
    changed.subject = PlanSubject::PrHead {
        head: source('f'),
        base: source('c'),
    };
    variants.push(changed);
    let mut changed = original.clone();
    changed.subject = PlanSubject::PrHead {
        head: source('a'),
        base: source('f'),
    };
    variants.push(changed);
    let mut changed = original.clone();
    changed.subject = PlanSubject::MergeResult {
        head: source('a'),
        base: source('c'),
        result: source('f'),
    };
    variants.push(changed);
    let mut changed = original.clone();
    changed.event_inputs.insert("event".into(), "push".into());
    variants.push(changed);
    let mut changed = original.clone();
    changed.cells[0].runner.platform = "linux".into();
    variants.push(changed);
    let mut changed = original.clone();
    changed.cells[0].dependencies[0].state = DependencyState::Resolved {
        revision: "2".repeat(64),
    };
    variants.push(changed);
    for changed in variants {
        assert_ne!(original.digest().unwrap(), changed.digest().unwrap());
    }
}

#[test]
fn rejects_unknown_versions_fields_duplicate_keys_and_trailing_json() {
    let bytes = String::from_utf8(plan().canonical_bytes().unwrap()).unwrap();
    for invalid in [
        bytes.replacen("\"version\":1", "\"version\":2", 1),
        bytes.replacen("\"version\":1", "\"version\":1,\"surprise\":true", 1),
        bytes.replacen(
            "\"event\":\"pull_request\"",
            "\"event\":\"pull_request\",\"event\":\"push\"",
            1,
        ),
        bytes.replacen("\"version\":1", "\"version\":1,\"version\":1", 1),
        format!("{bytes} {{}}"),
        "{".into(),
    ] {
        assert!(ImmutablePlan::from_slice(invalid.as_bytes()).is_err());
    }
    let mut native = plan();
    native.subject = PlanSubject::PrHead {
        head: SourceIdentity::Native {
            repository: "example/project".into(),
            revision: "a".repeat(64),
        },
        base: SourceIdentity::Native {
            repository: "example/project".into(),
            revision: "b".repeat(64),
        },
    };
    native.configuration.source = SourceIdentity::Native {
        repository: "example/project".into(),
        revision: "c".repeat(64),
    };
    let bytes = String::from_utf8(native.canonical_bytes().unwrap()).unwrap();
    assert!(
        ImmutablePlan::from_slice(
            bytes
                .replacen("\"kind\":\"native\"", "\"kind\":\"native\",\"extra\":1", 1)
                .as_bytes()
        )
        .is_err()
    );
}

#[test]
fn rejects_oversized_documents_fields_and_inventories() {
    assert!(Requirements::from_slice(&vec![b' '; MAX_DOCUMENT_BYTES + 1]).is_err());
    let mut value = requirements();
    value.cells[0].command.argv[0] = "x".repeat(2049);
    assert!(value.validate().is_err());
    let mut value = requirements();
    value.cells = (0..MAX_CELLS + 1)
        .map(|i| cell(&format!("cell-{i}")))
        .collect();
    assert!(value.validate().is_err());
    let mut value = requirements();
    value.cells[0].matrix = (0..33).map(|i| (format!("key-{i}"), "v".into())).collect();
    assert!(value.validate().is_err());
    assert!(
        WorkflowBinding::from_bytes(
            "script.sh".into(),
            WorkflowFormat::Commands,
            &vec![b'x'; MAX_WORKFLOW_BYTES + 1]
        )
        .is_err()
    );
}

#[test]
fn rejects_path_escape_duplicate_identity_and_unbound_script() {
    for invalid in [
        "../outside",
        "/outside",
        "a/../b",
        "C:\\outside",
        "a//b",
        "a/./b",
        "-option",
        "a\nb",
        ".git/config",
        "a/.AgentSCM/data",
    ] {
        let mut value = requirements();
        value.cells[0].command.working_directory = invalid.into();
        assert!(value.validate().is_err(), "{invalid}");
    }
    let mut value = requirements();
    value.cells.push(value.cells[0].clone());
    assert!(value.validate().is_err());
    let mut value = requirements();
    let service = value.cells[0].services[0].clone();
    value.cells[0].services.push(service);
    assert!(value.validate().is_err());
    let mut value = requirements();
    value.cells[0].command.reviewed_script = Some("unbound.sh".into());
    assert!(value.validate().is_err());
    let mut value = requirements();
    value.permitted_omissions.push(PermittedOmission {
        job: "mobile".into(),
        cell: "missing".into(),
        reason: "hosted only".into(),
    });
    assert!(value.validate().is_err());
}

#[test]
fn rejects_cross_repository_and_mixed_source_subjects() {
    let mut value = plan();
    value.configuration.source = SourceIdentity::Native {
        repository: "other/project".into(),
        revision: "a".repeat(64),
    };
    assert!(value.validate().is_err());
    let mut value = plan();
    value.subject = PlanSubject::PrHead {
        head: source('a'),
        base: SourceIdentity::Native {
            repository: "example/project".into(),
            revision: "a".repeat(64),
        },
    };
    assert!(value.validate().is_err());
    let mut value = plan();
    let mut other = source('a');
    if let SourceIdentity::Git { subject } = &mut other {
        subject.repository = "other/project".into();
    }
    value.subject = PlanSubject::MergeResult {
        head: source('a'),
        base: source('b'),
        result: other,
    };
    assert!(value.validate().is_err());
}
