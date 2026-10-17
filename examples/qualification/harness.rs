use std::fs;
use std::path::Path;
use std::time::Instant;

use agentscm::history::history_page;
use agentscm::model::FileChange;
use agentscm::store::{Engine, validate_id};
use agentscm::{Error, Result};

use super::contracts::{Inventory, PAGE_SIZE, Phase, Report, Workload};

pub fn path(index: usize) -> String {
    format!("files/f-{index:07}.bin")
}

fn index(path: &str, files: usize) -> Result<usize> {
    let parsed = path
        .strip_prefix("files/f-")
        .and_then(|p| p.strip_suffix(".bin"))
        .and_then(|p| p.parse::<usize>().ok());
    match parsed {
        Some(i) if i < files && self::path(i) == path => Ok(i),
        _ => Err(Error::Invalid("unexpected manifest path".into())),
    }
}

pub fn inventory_bounded(
    root: &Path,
    deadline: Option<Instant>,
    max_bytes: u64,
) -> Result<Inventory> {
    check_deadline(deadline)?;
    let mut result = Inventory::default();
    #[cfg(unix)]
    let mut temporary_inodes = std::collections::BTreeMap::new();
    #[cfg(unix)]
    {
        result.allocated_bytes = Some(0);
    }
    for shard in fs::read_dir(root.join("objects"))? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            return Err(Error::Invalid("unexpected object shard type".into()));
        }
        let prefix = shard.file_name().to_string_lossy().into_owned();
        for object in fs::read_dir(shard.path())? {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::Limit(
                    "inventory reached supervisor wall deadline".into(),
                ));
            }
            let object = object?;
            let name = object.file_name().to_string_lossy().into_owned();
            let kind = match object.file_type() {
                Ok(kind) => kind,
                Err(error)
                    if name.starts_with(".tmp-")
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => return Err(Error::Io(error)),
            };
            if !kind.is_file() {
                return Err(Error::Invalid("unexpected object type".into()));
            }
            // An interrupted native publication may leave a temporary file.
            if !name.starts_with(".tmp-") {
                validate_id(&format!("{prefix}{name}"))?;
                result.objects += 1;
            }
            let metadata = match object.metadata() {
                Ok(metadata) => metadata,
                Err(error)
                    if name.starts_with(".tmp-")
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => return Err(Error::Io(error)),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let inode = (metadata.dev(), metadata.ino());
                let length = metadata.len();
                let blocks = metadata.blocks() * 512;
                if let Some(previous) = temporary_inodes.get_mut(&inode) {
                    reconcile_temporary(&mut result, previous, length, blocks);
                } else if !name.starts_with(".tmp-") || metadata.nlink() == 1 {
                    result.encoded_bytes += length;
                    *result
                        .allocated_bytes
                        .as_mut()
                        .expect("Unix allocation counter") += blocks;
                    if name.starts_with(".tmp-") {
                        if temporary_inodes.len() == 65_536 {
                            return Err(Error::Limit(
                                "inventory exceeds temporary inode observation bound".into(),
                            ));
                        }
                        temporary_inodes.insert(inode, (length, blocks));
                    }
                }
            }
            #[cfg(not(unix))]
            {
                result.encoded_bytes += metadata.len();
            }
            if result
                .encoded_bytes
                .max(result.allocated_bytes.unwrap_or(0))
                > max_bytes
            {
                return Err(Error::Limit(
                    "sampled retained-storage guard reached".into(),
                ));
            }
        }
    }
    check_deadline(deadline)?;
    Ok(result)
}

fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(Error::Limit(
            "inventory reached supervisor wall deadline".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn reconcile_temporary(
    result: &mut Inventory,
    previous: &mut (u64, u64),
    length: u64,
    blocks: u64,
) {
    result.encoded_bytes += length.saturating_sub(previous.0);
    *result
        .allocated_bytes
        .as_mut()
        .expect("Unix allocation counter") += blocks.saturating_sub(previous.1);
    previous.0 = previous.0.max(length);
    previous.1 = previous.1.max(blocks);
}

fn progress(run: &Path, report: &Report) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    fs::write(run.join("progress.tmp"), bytes)?;
    fs::rename(run.join("progress.tmp"), run.join("progress.json"))?;
    Ok(())
}

pub fn write_report(run: &Path, filename: &str, report: &Report) -> Result<()> {
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(run.join(filename))?;
    use std::io::Write;
    output.write_all(&serde_json::to_vec_pretty(report)?)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    Ok(())
}

fn guard(started: Instant, workload: &Workload) -> Result<()> {
    if started.elapsed().as_secs() >= workload.max_wall_seconds {
        return Err(Error::Limit("worker wall-time guard reached".into()));
    }
    Ok(())
}

fn phase(report: &mut Report, name: &str, started: Instant) {
    report.phases.push(Phase {
        name: name.into(),
        elapsed_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
    });
}

fn completed(report: &mut Report, operation: &str) {
    let result = report
        .operation_results
        .iter_mut()
        .find(|row| row.operation == operation)
        .expect("fixed operation matrix");
    result.status = "completed".into();
}

pub fn run(run: &Path, report: &mut Report) -> Result<()> {
    let workload = report.workload.clone();
    workload.validate()?;
    let root = run.join("store");
    let engine = Engine::open(&root)?;
    let started = Instant::now();
    let stage = Instant::now();
    let mut head = None;
    let mut checkpoint_ids = Vec::new();
    for first in (0..workload.files).step_by(workload.batch_files) {
        let end = (first + workload.batch_files).min(workload.files);
        let changes: Vec<_> = (first..end)
            .map(|i| FileChange {
                path: path(i),
                content: Some(workload.bytes(i, 0)),
            })
            .collect();
        report.calls_attempted.checkpoint += 1;
        head = Some(engine.checkpoint(head.as_deref(), &changes)?);
        checkpoint_ids.push(head.as_ref().expect("created revision").clone());
        report.completed_files = end;
        report.head = head.clone();
        progress(run, report)?;
        guard(started, &workload)?;
    }
    phase(report, "create_source_including_monitoring", stage);
    completed(report, "source_creation");
    guard(started, &workload)?;
    let stage = Instant::now();
    let mut head = head.ok_or_else(|| Error::Invalid("empty workload".into()))?;
    let initial = head.clone();
    let mut last_parent = head.clone();
    for edit in 1..=workload.history_edits {
        last_parent = head.clone();
        let i = (edit - 1) % workload.files;
        report.calls_attempted.checkpoint += 1;
        head = engine.checkpoint(
            Some(&head),
            &[FileChange {
                path: path(i),
                content: Some(workload.bytes(i, edit)),
            }],
        )?;
        checkpoint_ids.push(head.clone());
        report.completed_history_edits = edit;
        report.head = Some(head.clone());
        progress(run, report)?;
        guard(started, &workload)?;
    }
    phase(report, "small_edits_including_monitoring", stage);
    completed(report, "history_edits");
    guard(started, &workload)?;

    // Opening a new Engine does not reset the operating system cache.
    drop(engine);
    let engine = Engine::open(&root)?;
    let stage = Instant::now();
    report.calls_attempted.read_file += 1;
    if engine.read_file(&head, &path(0))? != workload.bytes(0, workload.last_edit(0)) {
        return Err(Error::Invalid("initial reopened-reader mismatch".into()));
    }
    phase(report, "initial_reopened_read_os_cache_uncontrolled", stage);
    let stage = Instant::now();
    let edited_index = (workload.history_edits - 1) % workload.files;
    let mut edited_blob = None;
    let mut seen = vec![false; workload.files];
    let mut cursor = None;
    loop {
        report.calls_attempted.list_files += 1;
        let page = engine.list_files(&head, cursor.as_deref(), PAGE_SIZE)?;
        for entry in page.entries {
            let i = index(&entry.path, workload.files)?;
            if std::mem::replace(&mut seen[i], true) {
                return Err(Error::Invalid("manifest repeats a path".into()));
            }
            validate_id(&entry.blob)?;
            if i == edited_index {
                edited_blob = Some(entry.blob.clone());
            }
            report.calls_attempted.read_file += 1;
            if engine.read_file(&head, &entry.path)? != workload.bytes(i, workload.last_edit(i)) {
                return Err(Error::Invalid("source content mismatch".into()));
            }
            report.validated_files += 1;
        }
        next(&cursor, &page.next)?;
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    if seen.iter().any(|present| !present) {
        return Err(Error::Invalid("manifest omits files".into()));
    }
    phase(report, "validate_full_source_os_cache_uncontrolled", stage);
    progress(run, report)?;
    guard(started, &workload)?;

    let stage = Instant::now();
    let mut cursor = None;

    loop {
        report.calls_attempted.history_page += 1;
        let page = history_page(&engine, &head, cursor.as_deref(), PAGE_SIZE)?;
        if !page.needed_objects.is_empty() {
            return Err(Error::Invalid("history incomplete".into()));
        }
        for entry in page.entries {
            let position = checkpoint_ids
                .len()
                .checked_sub(report.validated_revisions + 1)
                .ok_or_else(|| Error::Invalid("history contains extra revisions".into()))?;
            validate_history_entry(&entry, &checkpoint_ids, position)?;
            report.validated_revisions += 1;
        }
        next(&cursor, &page.next)?;
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    if report.validated_revisions != checkpoint_ids.len() {
        return Err(Error::Invalid("history count mismatch".into()));
    }
    phase(report, "validate_full_history", stage);
    completed(report, "history_validation");

    let stage = Instant::now();
    let edited_path = path(edited_index);
    report.calls_attempted.read_file += 1;
    if engine.read_file(&last_parent, &edited_path)?
        != workload.bytes(
            edited_index,
            workload.history_edits.saturating_sub(workload.files),
        )
    {
        return Err(Error::Invalid("last-parent edited content mismatch".into()));
    }
    let before_blob = file_blob(&engine, &last_parent, &edited_path, report)?;
    let after_blob =
        edited_blob.ok_or_else(|| Error::Invalid("edited path not validated".into()))?;
    let mut entries = 0;
    let mut cursor = None;
    loop {
        report.calls_attempted.diff_page += 1;
        let page = engine.diff_page(&last_parent, &head, cursor.as_deref(), PAGE_SIZE)?;
        if !page.needed_objects.is_empty() {
            return Err(Error::Invalid("diff incomplete".into()));
        }
        for entry in page.entries {
            entries += 1;
            validate_diff_entry(&entry, &edited_path, &before_blob, &after_blob)?;
        }
        next(&cursor, &page.next)?;
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    if entries != 1 {
        return Err(Error::Invalid("small-edit diff count mismatch".into()));
    }
    phase(report, "validate_small_edit_diff", stage);
    completed(report, "diff_validation");

    let stage = Instant::now();
    for repetition in 0..workload.read_repetitions {
        let i = repetition % workload.files;
        report.calls_attempted.read_file += 1;
        if engine.read_file(&head, &path(i))? != workload.bytes(i, workload.last_edit(i)) {
            return Err(Error::Invalid("repeated source read mismatch".into()));
        }
    }
    // Check old immutable source survives later edits, including repeated data.
    report.calls_attempted.read_file += 1;
    if engine.read_file(&initial, &path(0))? != workload.bytes(0, 0) {
        return Err(Error::Invalid("retained source changed".into()));
    }
    phase(
        report,
        "repeated_reads_after_validation_os_cache_uncontrolled",
        stage,
    );
    guard(started, &workload)?;
    report.retained = Some(inventory_bounded(
        &root,
        Some(started + std::time::Duration::from_secs(workload.max_wall_seconds)),
        workload.max_retained_bytes,
    )?);
    completed(report, "source_validation");
    report.status = "completed".into();
    progress(run, report)?;
    Ok(())
}

fn validate_history_entry(
    entry: &agentscm::history::HistoryEntry,
    ids: &[String],
    position: usize,
) -> Result<()> {
    let parents = &ids[position.saturating_sub(1)..position];
    if entry.id != ids[position]
        || entry.revision.version != 1
        || entry.revision.parents.as_slice() != parents
    {
        return Err(Error::Invalid(
            "history differs from checkpoint-returned IDs/parents".into(),
        ));
    }
    Ok(())
}

fn validate_diff_entry(
    entry: &agentscm::model::DiffEntry,
    path: &str,
    before: &str,
    after: &str,
) -> Result<()> {
    if entry.path != path
        || entry.before.as_deref() != Some(before)
        || entry.after.as_deref() != Some(after)
        || before == after
    {
        return Err(Error::Invalid("small-edit diff identity mismatch".into()));
    }
    Ok(())
}

fn file_blob(engine: &Engine, revision: &str, path: &str, report: &mut Report) -> Result<String> {
    let mut cursor = None;
    loop {
        report.calls_attempted.list_files += 1;
        let page = engine.list_files(revision, cursor.as_deref(), PAGE_SIZE)?;
        if let Some(entry) = page.entries.iter().find(|entry| entry.path == path) {
            return Ok(entry.blob.clone());
        }
        next(&cursor, &page.next)?;
        cursor = page.next;
        if cursor.is_none() {
            return Err(Error::Invalid("parent manifest omits edited path".into()));
        }
    }
}

fn next(previous: &Option<String>, current: &Option<String>) -> Result<()> {
    if current.is_some() && current == previous {
        return Err(Error::Invalid("page cursor failed to advance".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::contracts::{BuildIdentity, Calls};
    use super::*;

    pub fn report() -> Report {
        Report {
            version: 1,
            status: "running".into(),
            diagnostic: None,
            workload: serde_json::from_str(include_str!(
                "../../fixtures/qualification/small-unique.json"
            ))
            .unwrap(),
            declared_build: BuildIdentity {
                source_commit: "a".repeat(40),
                source_tree: "b".repeat(40),
                source_clean: false,
                rustc_verbose: "test fixture".into(),
                profile: "test".into(),
                flags: "none".into(),
                host: "test".into(),
            },
            executable_sha256: "c".repeat(64),
            cargo_lock_sha256: "d".repeat(64),
            toolchain_file_sha256: "e".repeat(64),
            source_profile: "agentscm-source-v1".into(),
            cache_setup: "new store; OS cache uncontrolled".into(),
            completed_files: 0,
            completed_history_edits: 0,
            validated_files: 0,
            validated_revisions: 0,
            head: None,
            calls_attempted: Calls::default(),
            phases: Vec::new(),
            operation_results: super::super::contracts::operation_matrix(),
            retained: None,
            process_cpu_seconds: None,
            process_peak_rss_bytes: None,
            provider_requests: None,
            supervised_elapsed_ns: None,
            supervisor_inventory_passes: None,
            supervisor_inventory_wall_ns: None,
        }
    }

    #[test]
    fn full_native_correctness_and_retained_source_for_both_modes() {
        for mode in [
            super::super::contracts::Content::Unique,
            super::super::contracts::Content::Repeated,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut report = report();
            report.workload.content = mode;
            run(directory.path(), &mut report).unwrap();
            assert_eq!(report.status, "completed");
            assert_eq!(report.validated_files, report.workload.files);
            assert!(report.retained.unwrap().objects > report.workload.files as u64);
        }
    }

    #[test]
    fn wrong_diff_ids_and_self_consistent_wrong_history_are_rejected() {
        let before = "a".repeat(64);
        let after = "b".repeat(64);
        let mut entry = agentscm::model::DiffEntry {
            path: path(0),
            before: Some(before.clone()),
            after: Some(after.clone()),
        };
        validate_diff_entry(&entry, &path(0), &before, &after).unwrap();
        std::mem::swap(&mut entry.before, &mut entry.after);
        assert!(validate_diff_entry(&entry, &path(0), &before, &after).is_err());
        entry.before = Some("c".repeat(64));
        assert!(validate_diff_entry(&entry, &path(0), &before, &after).is_err());
        let ids = [before, after];
        let mut entry = agentscm::history::HistoryEntry {
            id: ids[1].clone(),
            revision: agentscm::model::Revision {
                version: 1,
                parents: vec![ids[0].clone()],
                tree: "c".repeat(64),
            },
        };
        validate_history_entry(&entry, &ids, 1).unwrap();
        entry.id = ids[0].clone();
        entry.revision.parents.clear();
        assert!(validate_history_entry(&entry, &ids, 1).is_err());
    }

    #[test]
    fn completed_and_progress_evidence_cannot_change_observed_identity() {
        let directory = tempfile::tempdir().unwrap();
        let initial = report();
        let mut result = report();
        run(directory.path(), &mut result).unwrap();
        result.validate_completed(&initial).unwrap();
        let serialized = serde_json::to_vec(&result).unwrap();
        for field in [
            "completed_files",
            "phases",
            "operation_results",
            "process_cpu_seconds",
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&serialized).unwrap();
            value[field] = match field {
                "completed_files" => serde_json::json!(0),
                "phases" => serde_json::json!([]),
                "operation_results" => serde_json::json!([]),
                _ => serde_json::json!(1.0),
            };
            let modified: Report = serde_json::from_value(value).unwrap();
            assert!(
                modified.validate_completed(&initial).is_err(),
                "accepted mutated {field}"
            );
        }
        for phase_count in [3, 4, 5, 6, 7] {
            let mut value: serde_json::Value = serde_json::from_slice(&serialized).unwrap();
            value["phases"]
                .as_array_mut()
                .unwrap()
                .truncate(phase_count);
            for (i, threshold) in [1, 2, 7, 5, 6].into_iter().enumerate() {
                value["operation_results"][i]["status"] =
                    serde_json::json!(if phase_count >= threshold {
                        "completed"
                    } else {
                        "not_run"
                    });
            }
            value["calls_attempted"]["read_file"] = serde_json::json!(0);
            value["calls_attempted"]["list_files"] = serde_json::json!(0);
            value["validated_files"] = serde_json::json!(0);
            value["validated_revisions"] = serde_json::json!(0);
            let wrong: Report = serde_json::from_value(value).unwrap();
            assert!(
                wrong.validate_progress(&initial).is_err(),
                "accepted contradictory phase {phase_count}"
            );
        }
        for phase_count in [2, 3, 5, 6] {
            let mut value: serde_json::Value = serde_json::from_slice(&serialized).unwrap();
            value["phases"]
                .as_array_mut()
                .unwrap()
                .truncate(phase_count);
            for (i, threshold) in [1, 2, 7, 5, 6].into_iter().enumerate() {
                value["operation_results"][i]["status"] =
                    serde_json::json!(if phase_count >= threshold {
                        "completed"
                    } else {
                        "not_run"
                    });
            }
            let wrong: Report = serde_json::from_value(value).unwrap();
            assert!(
                wrong.validate_progress(&initial).is_err(),
                "accepted unreachable persisted phase {phase_count}"
            );
        }
        let mut large_initial = report();
        large_initial.workload = serde_json::from_str(include_str!(
            "../../fixtures/qualification/source-1000000-unique.json"
        ))
        .unwrap();
        let workload = &large_initial.workload;
        let revisions = workload.files.div_ceil(workload.batch_files) + workload.history_edits;
        let mut value: serde_json::Value = serde_json::from_slice(&serialized).unwrap();
        value["workload"] = serde_json::to_value(workload).unwrap();
        value["completed_files"] = serde_json::json!(workload.files);
        value["completed_history_edits"] = serde_json::json!(workload.history_edits);
        value["validated_files"] = serde_json::json!(workload.files);
        value["validated_revisions"] = serde_json::json!(revisions);
        value["calls_attempted"]["checkpoint"] = serde_json::json!(revisions);
        value["calls_attempted"]["read_file"] =
            serde_json::json!(workload.files + workload.read_repetitions + 3);
        value["calls_attempted"]["list_files"] =
            serde_json::json!(workload.files.div_ceil(PAGE_SIZE) + 1);
        value["calls_attempted"]["history_page"] = serde_json::json!(revisions.div_ceil(PAGE_SIZE));
        let synthetic: Report = serde_json::from_value(value.clone()).unwrap();
        synthetic.validate_completed(&large_initial).unwrap();
        for field in ["list_files", "history_page"] {
            let mut wrong = value.clone();
            wrong["calls_attempted"][field] = serde_json::json!(1);
            let wrong: Report = serde_json::from_value(wrong).unwrap();
            assert!(
                wrong.validate_progress(&large_initial).is_err(),
                "accepted too few {field} pages"
            );
        }
        result.executable_sha256 = "0".repeat(64);
        assert!(result.validate_completed(&initial).is_err());
        assert!(result.validate_progress(&initial).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn publication_temporary_hardlink_is_not_double_counted() {
        let directory = tempfile::tempdir().unwrap();
        let shard = directory.path().join("objects/aa");
        fs::create_dir_all(&shard).unwrap();
        let object = shard.join("b".repeat(62));
        fs::write(&object, b"object").unwrap();
        let before = inventory_bounded(directory.path(), None, u64::MAX).unwrap();
        fs::hard_link(&object, shard.join(".tmp-fixture")).unwrap();
        let after = inventory_bounded(directory.path(), None, u64::MAX).unwrap();
        assert_eq!(before, after);
        assert!(inventory_bounded(directory.path(), Some(Instant::now()), u64::MAX).is_err());
        let mut sampled = Inventory {
            objects: 0,
            encoded_bytes: 0,
            allocated_bytes: Some(4096),
        };
        let mut previous = (0, 4096);
        reconcile_temporary(&mut sampled, &mut previous, 6, 4096);
        assert_eq!(sampled.encoded_bytes, 6);
        assert_eq!(sampled.allocated_bytes, Some(4096));
        reconcile_temporary(&mut sampled, &mut previous, 6, 4096);
        assert_eq!(sampled.encoded_bytes, 6);
        assert_eq!(sampled.allocated_bytes, Some(4096));
        let empty = tempfile::tempdir().unwrap();
        fs::create_dir(empty.path().join("objects")).unwrap();
        assert!(inventory_bounded(empty.path(), Some(Instant::now()), u64::MAX).is_err());
    }

    #[test]
    fn inconsistent_completed_progress_rows_are_rejected() {
        let initial = report();
        let mut wrong = report();
        for row in &mut wrong.operation_results[..5] {
            row.status = "completed".into();
        }
        assert!(wrong.validate_progress(&initial).is_err());
        let mut wrong = report();
        wrong.completed_files = wrong.workload.files;
        assert!(wrong.validate_progress(&initial).is_err());
        let mut wrong = report();
        wrong.head = Some("0".repeat(64));
        assert!(wrong.validate_progress(&initial).is_err());
    }

    #[test]
    fn storage_guard_and_cursor_failure_are_errors() {
        let directory = tempfile::tempdir().unwrap();
        let mut report = report();
        report.workload.max_retained_bytes =
            (report.workload.files * report.workload.file_bytes) as u64;
        assert!(matches!(
            run(directory.path(), &mut report),
            Err(Error::Limit(_))
        ));
        assert_ne!(report.status, "completed");
        assert!(next(&Some("same".into()), &Some("same".into())).is_err());
    }
}
