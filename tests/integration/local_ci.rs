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

struct MemoryReader {
    files: BTreeMap<String, Vec<u8>>,
}

impl SourceReader for MemoryReader {
    fn verify_source(&self, source: &SourceIdentity) -> agentscm::Result<()> {
        source.validate()
    }

    fn read_file(&self, _: &SourceIdentity, path: &str, _: usize) -> agentscm::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| agentscm::Error::Invalid("fixture file missing".into()))
    }
}

fn trusted_fixture(mut requirements: Requirements) -> (TrustedRequirements, MemoryReader) {
    requirements.workflows = vec![
        WorkflowBinding::from_bytes(
            "ci/script.sh".into(),
            WorkflowFormat::Commands,
            b"synthetic script\n",
        )
        .unwrap(),
    ];
    let bytes = requirements.canonical_bytes().unwrap();
    use sha2::{Digest, Sha256};
    let binding = ConfigBinding {
        source: source('d'),
        path: "ci/requirements.json".into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    let reader = MemoryReader {
        files: BTreeMap::from([
            ("ci/requirements.json".into(), bytes),
            ("ci/script.sh".into(), b"synthetic script\n".to_vec()),
        ]),
    };
    (TrustedRequirements::load(binding, &reader).unwrap(), reader)
}

fn inputs() -> PlanInputs {
    PlanInputs {
        subject: plan().subject,
        event_inputs: plan().event_inputs,
        omissions: vec![],
        qualified_capabilities: BTreeMap::from([
            ("command/v1".into(), ()),
            ("mobile-simulator/v1".into(), ()),
        ])
        .into_keys()
        .collect(),
    }
}

#[test]
fn resolution_requires_qualified_capabilities_resolved_dependencies_and_exact_omissions() {
    let (trusted, reader) = trusted_fixture(requirements());
    let mut input = inputs();
    input.qualified_capabilities.remove("mobile-simulator/v1");
    assert!(trusted.resolve(input, &reader).is_err());
    let mut input = inputs();
    input.omissions.push(PermittedOmission {
        job: "mobile".into(),
        cell: "ios".into(),
        reason: "untrusted skip".into(),
    });
    assert!(trusted.resolve(input, &reader).is_err());
    for state in [
        DependencyState::Unknown {},
        DependencyState::Declared {},
        DependencyState::Observed {
            revision: Some("a".repeat(64)),
            evidence_digest: "b".repeat(64),
        },
        DependencyState::Unavailable {
            reason: "offline".into(),
        },
    ] {
        let mut req = requirements();
        req.cells[0].dependencies[0].state = state;
        let (trusted, reader) = trusted_fixture(req);
        assert!(trusted.resolve(inputs(), &reader).is_err());
    }
    let mut req = requirements();
    let omission = PermittedOmission {
        job: "mobile".into(),
        cell: "ios".into(),
        reason: "explicit hosted coverage".into(),
    };
    req.permitted_omissions.push(omission.clone());
    req.cells[0].dependencies[0].state = DependencyState::Unknown {};
    let (trusted, reader) = trusted_fixture(req);
    let mut input = inputs();
    input.omissions.push(omission);
    let resolved = trusted.resolve(input, &reader).unwrap();
    assert_eq!(resolved.plan().cells.len(), 2);
    assert_eq!(resolved.plan().omissions.len(), 1);
}

#[test]
fn trusted_loading_rejects_substitution_and_oversized_reader_bytes() {
    let (trusted, mut reader) = trusted_fixture(requirements());
    let mut binding = trusted.binding().clone();
    binding.sha256 = "a".repeat(64);
    assert!(TrustedRequirements::load(binding, &reader).is_err());
    let mut input = inputs();
    input.subject = PlanSubject::PrHead {
        head: SourceIdentity::Native {
            repository: "example/project".into(),
            revision: "a".repeat(64),
        },
        base: SourceIdentity::Native {
            repository: "example/project".into(),
            revision: "b".repeat(64),
        },
    };
    assert!(trusted.resolve(input, &reader).is_err());
    let mut input = inputs();
    if let PlanSubject::PrHead {
        head: SourceIdentity::Git { subject },
        base: SourceIdentity::Git { subject: base },
    } = &mut input.subject
    {
        subject.repository = "other/project".into();
        base.repository = "other/project".into();
    }
    assert!(trusted.resolve(input, &reader).is_err());
    reader
        .files
        .insert("ci/script.sh".into(), vec![b'x'; MAX_WORKFLOW_BYTES + 1]);
    assert!(trusted.resolve(inputs(), &reader).is_err());
    reader.files.insert(
        "ci/requirements.json".into(),
        vec![b'x'; MAX_DOCUMENT_BYTES + 1],
    );
    assert!(TrustedRequirements::load(trusted.binding().clone(), &reader).is_err());
}

#[test]
fn adapter_descriptors_preserve_the_entire_resolved_inventory() {
    let (trusted, mut reader) = trusted_fixture(requirements());
    let command = trusted.resolve(inputs(), &reader).unwrap();
    assert!(command.adapter_input(AdapterKind::Command).is_ok());
    assert!(command.adapter_input(AdapterKind::Actions).is_err());
    let mut req = trusted.requirements().clone();
    req.workflows.push(
        WorkflowBinding::from_bytes(
            "ci/actions.yml".into(),
            WorkflowFormat::Actions,
            b"synthetic Actions input\n",
        )
        .unwrap(),
    );
    let bytes = req.canonical_bytes().unwrap();
    use sha2::{Digest, Sha256};
    let mut binding = trusted.binding().clone();
    binding.sha256 = format!("{:x}", Sha256::digest(&bytes));
    reader.files.insert(binding.path.clone(), bytes);
    reader.files.insert(
        "ci/actions.yml".into(),
        b"synthetic Actions input\n".to_vec(),
    );
    let trusted = TrustedRequirements::load(binding, &reader).unwrap();
    let resolved = trusted.resolve(inputs(), &reader).unwrap();
    let command = resolved.adapter_input(AdapterKind::Command).unwrap();
    let actions = resolved.adapter_input(AdapterKind::Actions).unwrap();
    assert_eq!(command.cells(), actions.cells());
    assert_eq!(command.omissions(), actions.omissions());
    assert_eq!(
        command.plan().digest().unwrap(),
        actions.plan().digest().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn git_loader_uses_trusted_configuration_and_exact_workflow_bytes() {
    use std::{fs, path::Path, process::Command};
    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn identity(repo: &Path) -> SourceIdentity {
        SourceIdentity::Git {
            subject: GitValidationSubject {
                repository: "example/project".into(),
                commit: git(repo, &["rev-parse", "HEAD"]),
                tree: git(repo, &["rev-parse", "HEAD^{tree}"]),
            },
        }
    }
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, &["init", "--quiet"]);
    fs::create_dir(root.join("ci")).unwrap();
    let mut req = requirements();
    req.workflows = vec![
        WorkflowBinding::from_bytes(
            "ci/script.sh".into(),
            WorkflowFormat::Commands,
            b"echo synthetic\n",
        )
        .unwrap(),
    ];
    req.cells[0].command.reviewed_script = Some("ci/script.sh".into());
    let bytes = req.canonical_bytes().unwrap();
    fs::write(root.join("ci/requirements.json"), &bytes).unwrap();
    fs::write(root.join("ci/script.sh"), b"echo synthetic\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "fixture base"]);
    let base = identity(root);
    use sha2::{Digest, Sha256};
    let binding = ConfigBinding {
        source: base.clone(),
        path: "ci/requirements.json".into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    let reader = GitSourceReader::new(root, "example/project".into()).unwrap();
    let trusted = TrustedRequirements::load(binding.clone(), &reader).unwrap();
    req.cells.pop();
    req.cells[0].services.clear();
    req.policy_epoch += 1;
    fs::write(
        root.join("ci/requirements.json"),
        req.canonical_bytes().unwrap(),
    )
    .unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "--quiet", "-m", "candidate weakens config"],
    );
    let candidate = identity(root);
    let mut input = inputs();
    input.subject = PlanSubject::PrHead {
        head: candidate.clone(),
        base: base.clone(),
    };
    let resolved = trusted.resolve(input.clone(), &reader).unwrap();
    assert_eq!(resolved.plan().cells.len(), 2);
    assert_eq!(resolved.plan().cells[0].services.len(), 1);
    assert_eq!(resolved.plan().policy_epoch, 3);
    let mut false_tree = candidate.clone();
    if let SourceIdentity::Git { subject } = &mut false_tree {
        subject.tree = "a".repeat(40);
    }
    assert!(reader.verify_source(&false_tree).is_err());
    fs::write(root.join("ci/script.sh"), b"echo candidate changed\n").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "--quiet", "-m", "candidate changes workflow"],
    );
    let changed = identity(root);
    input.subject = PlanSubject::PrHead {
        head: changed.clone(),
        base: base.clone(),
    };
    assert!(trusted.resolve(input, &reader).is_err());
    if let (
        SourceIdentity::Git { subject: original },
        SourceIdentity::Git {
            subject: replacement,
        },
    ) = (&candidate, &changed)
    {
        git(root, &["replace", &original.commit, &replacement.commit]);
    }
    assert!(reader.verify_source(&candidate).is_ok());
    let mut input = inputs();
    input.subject = PlanSubject::PrHead {
        head: candidate,
        base: base.clone(),
    };
    assert!(trusted.resolve(input, &reader).is_ok());
    // A reviewed new trusted revision can activate changed requirements, but must explicitly
    // update the workflow digest as well; the candidate cannot do this to its old plan.
    let fresh = TrustedRequirements::load(binding, &reader).unwrap();
    assert_eq!(fresh.requirements().policy_epoch, 3);
    req.workflows[0] = WorkflowBinding::from_bytes(
        "ci/script.sh".into(),
        WorkflowFormat::Commands,
        b"echo candidate changed\n",
    )
    .unwrap();
    let reviewed_bytes = req.canonical_bytes().unwrap();
    fs::write(root.join("ci/requirements.json"), &reviewed_bytes).unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["commit", "--quiet", "-m", "reviewed configuration fixture"],
    );
    let reviewed = identity(root);
    let activated = TrustedRequirements::load(
        ConfigBinding {
            source: reviewed.clone(),
            path: "ci/requirements.json".into(),
            sha256: format!("{:x}", Sha256::digest(&reviewed_bytes)),
        },
        &reader,
    )
    .unwrap();
    let mut input = inputs();
    input.subject = PlanSubject::PrHead {
        head: reviewed,
        base,
    };
    let activated_plan = activated.resolve(input, &reader).unwrap();
    assert_eq!(activated_plan.plan().policy_epoch, 4);
    assert_eq!(activated_plan.plan().cells.len(), 1);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("script.sh", root.join("ci/link.sh")).unwrap();
        fs::write(root.join("ci/literal[1].sh"), b"literal bytes").unwrap();
        fs::write(root.join("ci/large.bin"), vec![b'x'; 4096]).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "symlink fixture"]);
        assert!(
            reader
                .read_file(&identity(root), "ci/link.sh", 1024)
                .is_err()
        );
        assert_eq!(
            reader
                .read_file(&identity(root), "ci/literal[1].sh", 1024)
                .unwrap(),
            b"literal bytes"
        );
        assert!(
            reader
                .read_file(&identity(root), "ci/large.bin", 1024)
                .is_err()
        );
    }
}

#[test]
fn native_sources_use_the_same_resolver_without_git_adoption() {
    let (git_trusted, reader) = trusted_fixture(requirements());
    let native = |c: char| SourceIdentity::Native {
        repository: "example/project".into(),
        revision: c.to_string().repeat(64),
    };
    let mut binding = git_trusted.binding().clone();
    binding.source = native('c');
    let trusted = TrustedRequirements::load(binding, &reader).unwrap();
    let mut input = inputs();
    input.subject = PlanSubject::PrHead {
        head: native('a'),
        base: native('b'),
    };
    assert!(trusted.resolve(input, &reader).is_ok());
}

fn coverage(plan: &ImmutablePlan) -> CoverageReport {
    CoverageReport {
        version: VERSION,
        plan_sha256: plan.digest().unwrap(),
        cells: plan
            .cells
            .iter()
            .filter(|cell| {
                !plan
                    .omissions
                    .iter()
                    .any(|o| o.job == cell.job && o.cell == cell.id)
            })
            .map(|cell| CellObservation {
                job: cell.job.clone(),
                cell: cell.id.clone(),
                outcome: ObservationOutcome::Success,
                services: cell
                    .services
                    .iter()
                    .map(|s| ServiceObservation {
                        id: s.id.clone(),
                        outcome: ObservationOutcome::Success,
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[test]
fn coverage_inventory_and_outcomes_are_independent_of_execution_assurance() {
    use agentscm::evidence::EvidenceOutcome;
    let (trusted, reader) = trusted_fixture(requirements());
    let plan = trusted.resolve(inputs(), &reader).unwrap().plan().clone();
    let report = coverage(&plan);
    let assess = |report: &CoverageReport| {
        verify_coverage(trusted.binding().clone(), inputs(), &reader, &plan, report).unwrap()
    };
    let good = assess(&report);
    assert!(good.inventory_complete);
    assert_eq!(good.outcome, EvidenceOutcome::Pass);
    let mut missing = report.clone();
    missing.cells.pop();
    let result = assess(&missing);
    assert!(!result.inventory_complete);
    assert_eq!(result.outcome, EvidenceOutcome::Incomplete);
    let mut missing_service = report.clone();
    missing_service.cells[0].services.clear();
    assert!(!assess(&missing_service).inventory_complete);
    let mut extra = report.clone();
    extra.cells[0].cell = "unexpected".into();
    assert!(!assess(&extra).inventory_complete);
    let mut extra_service = report.clone();
    extra_service.cells[0].services[0].id = "unexpected".into();
    assert!(!assess(&extra_service).inventory_complete);
    for state in [
        ObservationOutcome::Skipped,
        ObservationOutcome::Cancelled,
        ObservationOutcome::Unavailable,
        ObservationOutcome::Incomplete,
        ObservationOutcome::Unknown,
    ] {
        let mut changed = report.clone();
        changed.cells[0].outcome = state;
        assert!(assess(&changed).inventory_complete);
        assert_eq!(assess(&changed).outcome, EvidenceOutcome::Incomplete);
    }
    let mut failed = report.clone();
    failed.cells[0].outcome = ObservationOutcome::Failed;
    assert!(assess(&failed).inventory_complete);
    assert_eq!(assess(&failed).outcome, EvidenceOutcome::Fail);
    for state in [
        ObservationOutcome::Failed,
        ObservationOutcome::Skipped,
        ObservationOutcome::Cancelled,
        ObservationOutcome::Unavailable,
        ObservationOutcome::Incomplete,
        ObservationOutcome::Unknown,
    ] {
        let mut changed = report.clone();
        changed.cells[0].services[0].outcome = state;
        assert_eq!(assess(&changed).outcome, EvidenceOutcome::Incomplete);
    }
}

#[test]
fn coverage_strict_parsing_canonicalization_and_duplicate_rejection() {
    let report = coverage(&plan());
    let bytes = report.canonical_bytes().unwrap();
    assert_eq!(
        CoverageReport::from_slice(&bytes)
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        bytes
    );
    let mut reordered = report.clone();
    reordered.cells.reverse();
    assert_eq!(report.digest().unwrap(), reordered.digest().unwrap());
    let mut duplicate = report.clone();
    duplicate.cells.push(duplicate.cells[0].clone());
    assert!(duplicate.validate().is_err());
    let mut duplicate = report.clone();
    let service = duplicate.cells[0].services[0].clone();
    duplicate.cells[0].services.push(service);
    assert!(duplicate.validate().is_err());
    let text = String::from_utf8(bytes).unwrap();
    for invalid in [
        text.replacen("\"version\":1", "\"version\":2", 1),
        text.replacen("\"version\":1", "\"version\":1,\"extra\":true", 1),
        text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1),
        format!("{text} {{}}"),
        "{".into(),
    ] {
        assert!(CoverageReport::from_slice(invalid.as_bytes()).is_err());
    }
    assert!(CoverageReport::from_slice(&vec![b' '; MAX_DOCUMENT_BYTES + 1]).is_err());
    assert_eq!(
        report.digest().unwrap(),
        "9a9d1723217cf6fdddde3b9264ca0dd89e5f41bb61b24080eb58a7b978231b92"
    );
}

#[test]
fn verifier_recomputes_current_subject_policy_and_omissions() {
    let (trusted, reader) = trusted_fixture(requirements());
    let original = trusted.resolve(inputs(), &reader).unwrap().plan().clone();
    let report = coverage(&original);
    let mut mutations = Vec::new();
    let mut changed = original.clone();
    changed.policy_epoch += 1;
    mutations.push(changed);
    let mut changed = original.clone();
    changed.requirements_sha256 = "a".repeat(64);
    mutations.push(changed);
    let mut changed = original.clone();
    changed.configuration.sha256 = "b".repeat(64);
    mutations.push(changed);
    let mut changed = original.clone();
    changed.subject = PlanSubject::PrHead {
        head: source('a'),
        base: source('f'),
    };
    mutations.push(changed);
    let mut changed = original.clone();
    changed.subject = PlanSubject::MergeResult {
        head: source('a'),
        base: source('c'),
        result: source('a'),
    };
    mutations.push(changed);
    let mut changed = original.clone();
    changed.cells.pop();
    mutations.push(changed);
    let mut changed = original.clone();
    changed.omissions.push(PermittedOmission {
        job: "mobile".into(),
        cell: "ios".into(),
        reason: "skip".into(),
    });
    mutations.push(changed);
    for changed in mutations {
        // Even a self-consistent report for the altered plan cannot set its own expected inputs.
        let report = coverage(&changed);
        assert!(
            verify_coverage(
                trusted.binding().clone(),
                inputs(),
                &reader,
                &changed,
                &report
            )
            .is_err()
        );
    }
    let mut stale = inputs();
    stale.event_inputs.insert("event".into(), "push".into());
    assert!(
        verify_coverage(
            trusted.binding().clone(),
            stale,
            &reader,
            &original,
            &report
        )
        .is_err()
    );
    let mut wrong_report = report;
    wrong_report.plan_sha256 = "f".repeat(64);
    assert!(
        verify_coverage(
            trusted.binding().clone(),
            inputs(),
            &reader,
            &original,
            &wrong_report
        )
        .is_err()
    );
    let mut req = requirements();
    let omission = PermittedOmission {
        job: "mobile".into(),
        cell: "ios".into(),
        reason: "reviewed hosted omission".into(),
    };
    req.permitted_omissions.push(omission.clone());
    let (trusted, reader) = trusted_fixture(req);
    let mut input = inputs();
    input.omissions.push(omission);
    let plan = trusted
        .resolve(input.clone(), &reader)
        .unwrap()
        .plan()
        .clone();
    let mut report = coverage(&plan);
    let result = verify_coverage(
        trusted.binding().clone(),
        input.clone(),
        &reader,
        &plan,
        &report,
    )
    .unwrap();
    assert_eq!(result.expected_cells, 1);
    assert_eq!(result.outcome, agentscm::evidence::EvidenceOutcome::Pass);
    report.cells.push(CellObservation {
        job: "mobile".into(),
        cell: "ios".into(),
        outcome: ObservationOutcome::Success,
        services: vec![],
    });
    assert!(
        !verify_coverage(trusted.binding().clone(), input, &reader, &plan, &report)
            .unwrap()
            .inventory_complete
    );
}

#[test]
fn portable_evidence_mapping_commits_plan_without_promoting_legacy_assurance() {
    use agentscm::context::ChangeId;
    use sha2::{Digest, Sha256};
    let original = plan();
    let change = ChangeId {
        repository: "example/project".into(),
        id: "pr-1".into(),
    };
    let subject = original.evidence_subject(change.clone(), 1).unwrap();
    assert_eq!(subject.context_digest, original.digest().unwrap());
    assert_eq!(subject.recipe_sha256, original.requirements_sha256);
    assert_eq!(LOCAL_CI_CHECK_KIND, "command/v1");
    assert_ne!(
        LOCAL_CI_REQUIREMENT_ID,
        agentscm::local_validation::REQUIREMENT_ID
    );
    let artifact = original.evidence_artifact().unwrap();
    assert_eq!(
        artifact.record.sha256,
        format!("{:x}", Sha256::digest(&artifact.bytes))
    );
    assert_ne!(artifact.record.sha256, original.digest().unwrap());
    assert_eq!(artifact.bytes, original.canonical_bytes().unwrap());
    // Existing portable evidence validation accepts this mapping; a fixture basis stays a
    // fixture and this does not sign, authenticate or promote its execution claims.
    use agentscm::evidence::{
        ArtifactAvailability, ArtifactRecord, CoverageClaim, EvidenceOutcome, EvidencePayload,
        ExecutionBasis, OutputCommitment, RecoveryState, TerminalState,
    };
    let record_sha256 = format!("{:x}", Sha256::digest(b"synthetic record"));
    let empty = format!("{:x}", Sha256::digest(b""));
    EvidencePayload {
        version: 1,
        requirement_id: LOCAL_CI_REQUIREMENT_ID.into(),
        check_kind: LOCAL_CI_CHECK_KIND.into(),
        subject: subject.clone(),
        installed_policy_sha256: original.requirements_sha256.clone(),
        installed_policy_epoch: original.policy_epoch,
        issuer_registry_sha256: "a".repeat(64),
        dependency_snapshot_sha256: None,
        attempt_id: "fixture-attempt".into(),
        run_generation: 1,
        request_sha256: original.digest().unwrap(),
        limits_sha256: "b".repeat(64),
        terminal_record_sha256: record_sha256.clone(),
        issuer_id: "fixture".into(),
        key_id: "c".repeat(64),
        issued_at_ms: 1,
        expires_at_ms: 2,
        terminal_state: TerminalState::Success,
        recovery_state: RecoveryState::NotRequired,
        outcome: EvidenceOutcome::Pass,
        execution_basis: ExecutionBasis::Fixture,
        qualification_claim: None,
        coverage: CoverageClaim {
            scope: vec!["synthetic-cells".into()],
            expected_count: 2,
            observed_count: 2,
            inventory_complete: true,
            omissions: vec![],
        },
        dependencies: vec![],
        artifacts: vec![
            artifact.record,
            ArtifactRecord {
                artifact_id: "record".into(),
                media_type: "application/json".into(),
                sha256: record_sha256,
                bytes: 16,
                claimed_availability: ArtifactAvailability::Full,
                expires_at_ms: None,
            },
        ],
        output: OutputCommitment {
            stdout_sha256: empty.clone(),
            stdout_bytes: 0,
            stderr_sha256: empty,
            stderr_bytes: 0,
            truncated: false,
        },
    }
    .validate()
    .unwrap();
    let mut changed = original.clone();
    changed.subject = PlanSubject::PrHead {
        head: source('f'),
        base: source('c'),
    };
    assert_ne!(
        subject.revision,
        changed
            .evidence_subject(change.clone(), 1)
            .unwrap()
            .revision
    );
    let mut merge = original.clone();
    merge.subject = PlanSubject::MergeResult {
        head: source('a'),
        base: source('c'),
        result: source('a'),
    };
    assert_ne!(
        subject.context_digest,
        merge
            .evidence_subject(change.clone(), 1)
            .unwrap()
            .context_digest
    );
    assert!(original.evidence_subject(change.clone(), 0).is_err());
    let wrong = ChangeId {
        repository: "other/project".into(),
        id: "pr-1".into(),
    };
    assert!(original.evidence_subject(wrong, 1).is_err());
    let mut reordered = original.clone();
    reordered.cells.reverse();
    assert_eq!(subject, reordered.evidence_subject(change, 1).unwrap());
}
