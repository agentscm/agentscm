//! Bounded qualification driver; all source operations use public native APIs.
mod qualification {
    pub mod contracts;
    pub mod harness;
}

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentscm::store::digest;
use agentscm::{Error, Result};
use clap::Parser;
use qualification::contracts::{BuildIdentity, Calls, Report, Workload};
use qualification::harness::{inventory_bounded, write_report};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

#[derive(Parser)]
struct Args {
    /// New directory under an existing, trusted, disposable parent.
    #[arg(long)]
    run_dir: PathBuf,
    #[arg(long)]
    workload: PathBuf,
    #[arg(long)]
    build_identity: PathBuf,
    #[arg(long, hide = true)]
    worker: bool,
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Limit("JSON input exceeds 64 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn executable_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn verify_observed_identity(report: &Report, executable: &Path) -> Result<()> {
    if report.executable_sha256 != executable_digest(executable)?
        || report.cargo_lock_sha256 != digest(include_bytes!("../Cargo.lock"))
        || report.toolchain_file_sha256 != digest(include_bytes!("../rust-toolchain.toml"))
    {
        return Err(Error::Invalid(
            "worker observed binary/build hashes differ from initial".into(),
        ));
    }
    Ok(())
}

fn initial_report(
    workload: Workload,
    declared_build: BuildIdentity,
    executable: &Path,
) -> Result<Report> {
    Ok(Report {
        version: 1, status: "running".into(), diagnostic: None, workload, declared_build,
        executable_sha256: executable_digest(executable)?,
        cargo_lock_sha256: digest(include_bytes!("../Cargo.lock")),
        toolchain_file_sha256: digest(include_bytes!("../rust-toolchain.toml")),
        source_profile: "agentscm-source-v1".into(),
        cache_setup: "fresh owned store; initial and repeated reader; OS cache uncontrolled; no application read cache".into(),
        completed_files: 0, completed_history_edits: 0, validated_files: 0, validated_revisions: 0,
        head: None, calls_attempted: Calls::default(), phases: Vec::new(),
        operation_results: qualification::contracts::operation_matrix(), retained: None,
        process_cpu_seconds: None, process_peak_rss_bytes: None, provider_requests: None,
        supervised_elapsed_ns: None, supervisor_inventory_passes: None, supervisor_inventory_wall_ns: None,
    })
}

fn execute(args: Args) -> Result<bool> {
    let workload: Workload = read_json(&args.workload)?;
    let identity: BuildIdentity = read_json(&args.build_identity)?;
    workload.validate()?;
    identity.validate()?;
    if args.worker {
        let expected = ["initial.json", "workload.json", "build-identity.json"];
        for entry in fs::read_dir(&args.run_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || !expected.iter().any(|name| entry.file_name() == *name)
            {
                return Err(Error::Invalid(
                    "worker directory contains unexpected data".into(),
                ));
            }
        }
        fs::create_dir(args.run_dir.join("store"))?;
        let mut report: Report = read_json(&args.run_dir.join("initial.json"))?;
        report.workload.validate()?;
        report.declared_build.validate()?;
        report.validate_hashes()?;
        if let Err(error) = verify_observed_identity(&report, &std::env::current_exe()?) {
            report.status = "failed".into();
            report.diagnostic = Some(error.to_string());
            write_report(&args.run_dir, "worker-result.json", &report)?;
            return Ok(false);
        }
        if report.workload != workload || report.declared_build != identity {
            return Err(Error::Invalid(
                "worker inputs differ from initial receipt".into(),
            ));
        }
        if let Err(error) = qualification::harness::run(&args.run_dir, &mut report) {
            report.status = "failed".into();
            report.diagnostic = Some(error.to_string());
        }
        let completed = report.status == "completed";
        write_report(&args.run_dir, "worker-result.json", &report)?;
        return Ok(completed);
    }

    // Canonicalize only the existing parent; never reuse or remove a fixture.
    let name = args
        .run_dir
        .file_name()
        .ok_or_else(|| Error::Invalid("run directory needs a new name".into()))?;
    let parent = args
        .run_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let run = parent.canonicalize()?.join(name);
    let executable = std::env::current_exe()?;
    let mut report = initial_report(workload, identity, &executable)?;
    report.validate_hashes()?;
    fs::create_dir(&run)?;
    fs::write(
        run.join("initial.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    fs::write(
        run.join("workload.json"),
        serde_json::to_vec_pretty(&report.workload)?,
    )?;
    fs::write(
        run.join("build-identity.json"),
        serde_json::to_vec_pretty(&report.declared_build)?,
    )?;
    let child = Command::new(&executable)
        .arg("--worker")
        .arg("--run-dir")
        .arg(&run)
        .arg("--workload")
        .arg(run.join("workload.json"))
        .arg("--build-identity")
        .arg(run.join("build-identity.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            report.status = "incomplete".into();
            report.diagnostic = Some(format!("worker launch failed: {error}"));
            write_report(&run, "result.json", &report)?;
            return Ok(false);
        }
    };
    supervise(&mut child, &run, report)
}

fn supervise(child: &mut std::process::Child, run: &Path, mut report: Report) -> Result<bool> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(report.workload.max_wall_seconds);
    let mut next_inventory = started;
    let mut inventory_passes = 0;
    let mut inventory_wall_ns = 0;
    let mut stop_reason = None;
    let mut exited = None;
    let supervision: Result<()> = (|| {
        loop {
            if Instant::now() >= deadline {
                stop_reason = Some("supervisor wall-time guard reached".to_owned());
                break;
            }
            if let Some(status) = child.try_wait()? {
                exited = Some(status);
                break;
            }
            if Instant::now() >= next_inventory && run.join("store/objects").exists() {
                let scan_started = Instant::now();
                let scan = inventory_bounded(
                    &run.join("store"),
                    Some(deadline),
                    report.workload.max_retained_bytes,
                );
                let elapsed = scan_started.elapsed();
                inventory_passes += 1;
                inventory_wall_ns += elapsed.as_nanos().min(u64::MAX as u128) as u64;
                scan?;
                // Avoid spending most of a large run repeatedly rescanning objects.
                next_inventory = Instant::now()
                    + elapsed
                        .saturating_mul(4)
                        .clamp(Duration::from_secs(1), Duration::from_secs(30));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })();
    if let Err(error) = supervision {
        stop_reason = Some(format!("supervision failed: {error}"));
    }
    if stop_reason.is_some() {
        // This fixed native worker never spawns descendants or external tools.
        if let Err(error) = child.kill()
            && child.try_wait()?.is_none()
        {
            return Err(Error::Io(error));
        }
        child.wait()?;
    }
    if stop_reason.is_none() && exited.is_some_and(|status| status.success()) {
        let validation: Result<Report> = (|| {
            let result: Report = read_json(&run.join("worker-result.json"))?;
            result.validate_completed(&report)?;
            let scan_started = Instant::now();
            let retained = inventory_bounded(
                &run.join("store"),
                Some(deadline),
                report.workload.max_retained_bytes,
            );
            inventory_passes += 1;
            inventory_wall_ns += scan_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            if result.retained.as_ref() != Some(&retained?) {
                return Err(Error::Invalid(
                    "worker inventory differs from stable final inventory".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(Error::Limit(
                    "supervisor completion deadline reached".into(),
                ));
            }
            Ok(result)
        })();
        match validation {
            Ok(result) => {
                report = result;
            }
            Err(error) => {
                stop_reason = Some(format!("worker completion rejected: {error}"));
            }
        }
    } else if stop_reason.is_none() {
        stop_reason = Some("worker exited unsuccessfully".into());
    }
    if stop_reason.is_some() {
        if let Ok(partial) = read_json::<Report>(&run.join("progress.json"))
            && partial.validate_progress(&report).is_ok()
        {
            report = partial;
        }
        report.status = "incomplete".into();
        report.diagnostic = stop_reason;
        // Incomplete progress cannot supply final measured storage inventory.
        report.retained = None;
    }
    report.supervised_elapsed_ns = Some(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
    report.supervisor_inventory_passes = Some(inventory_passes);
    report.supervisor_inventory_wall_ns = Some(inventory_wall_ns);
    let completed = report.status == "completed";
    write_report(run, "result.json", &report)?;
    Ok(completed)
}

fn main() {
    match execute(Args::parse()) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        initial_report(
            serde_json::from_slice(include_bytes!(
                "../fixtures/qualification/small-unique.json"
            ))
            .unwrap(),
            serde_json::from_slice(include_bytes!(
                "../fixtures/qualification/build-identity-example.json"
            ))
            .unwrap(),
            &std::env::current_exe().unwrap(),
        )
        .unwrap()
    }

    fn fixture_child(run: &Path, mode: &str) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::supervised_child_fixture", "--ignored"])
            .env("AGENTSCM_QUALIFY_TEST_RUN", run)
            .env("AGENTSCM_QUALIFY_TEST_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    #[ignore = "subprocess fixture; invoked only by the bounded supervisor tests"]
    fn supervised_child_fixture() {
        let run = PathBuf::from(
            std::env::var_os("AGENTSCM_QUALIFY_TEST_RUN").expect("test fixture directory"),
        );
        let mode = std::env::var("AGENTSCM_QUALIFY_TEST_MODE").unwrap();
        if mode == "missing" {
            return;
        }
        if mode == "nonzero" {
            std::process::exit(2);
        }
        if mode == "success" {
            let mut report = report();
            qualification::harness::run(&run, &mut report).unwrap();
            write_report(&run, "worker-result.json", &report).unwrap();
            return;
        }
        fs::write(
            run.join("worker-result.json"),
            b"malformed retained worker result",
        )
        .unwrap();
        if mode == "timeout" {
            std::thread::sleep(Duration::from_secs(10));
        }
    }

    #[test]
    fn supervisor_kills_waits_and_preserves_incomplete_evidence() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("keep"), b"retained").unwrap();
        let mut initial = report();
        initial.workload.max_wall_seconds = 1;
        let mut child = fixture_child(directory.path(), "timeout");
        assert!(!supervise(&mut child, directory.path(), initial).unwrap());
        assert!(child.try_wait().unwrap().is_some());
        let result: Report = read_json(&directory.path().join("result.json")).unwrap();
        assert_eq!(result.status, "incomplete");
        assert_eq!(
            fs::read(directory.path().join("keep")).unwrap(),
            b"retained"
        );
        assert_eq!(
            fs::read(directory.path().join("worker-result.json")).unwrap(),
            b"malformed retained worker result"
        );
    }

    #[test]
    fn supervisor_missing_malformed_and_nonzero_results_remain_incomplete() {
        for mode in ["missing", "malformed", "nonzero"] {
            let directory = tempfile::tempdir().unwrap();
            let mut child = fixture_child(directory.path(), mode);
            assert!(!supervise(&mut child, directory.path(), report()).unwrap());
            assert!(child.try_wait().unwrap().is_some());
            let result: Report = read_json(&directory.path().join("result.json")).unwrap();
            assert_eq!(result.status, "incomplete");
            assert!(result.diagnostic.is_some());
        }
    }

    #[test]
    fn supervisor_accepts_success_and_preserves_existing_final_file() {
        for existing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            if existing {
                fs::write(directory.path().join("result.json"), b"keep").unwrap();
            }
            let mut child = fixture_child(directory.path(), "success");
            let result = supervise(&mut child, directory.path(), report());
            assert!(child.try_wait().unwrap().is_some());
            let worker: Report = read_json(&directory.path().join("worker-result.json")).unwrap();
            assert_eq!(worker.status, "completed");
            if existing {
                assert!(result.is_err());
                assert_eq!(
                    fs::read(directory.path().join("result.json")).unwrap(),
                    b"keep"
                );
            } else {
                assert!(result.unwrap());
                let result: Report = read_json(&directory.path().join("result.json")).unwrap();
                assert_eq!(result.status, "completed");
                assert!(result.supervised_elapsed_ns.is_some());
                assert!(result.supervisor_inventory_passes.unwrap() >= 1);
                assert!(result.supervisor_inventory_wall_ns.is_some());
            }
        }
    }

    #[test]
    fn direct_worker_cannot_write_a_final_supervised_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let initial = report();
        let workload = directory.path().join("workload.json");
        let identity = directory.path().join("build-identity.json");
        fs::write(&workload, serde_json::to_vec(&initial.workload).unwrap()).unwrap();
        fs::write(
            &identity,
            serde_json::to_vec(&initial.declared_build).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join("initial.json"),
            serde_json::to_vec(&initial).unwrap(),
        )
        .unwrap();
        assert!(
            execute(Args {
                run_dir: directory.path().to_path_buf(),
                workload,
                build_identity: identity,
                worker: true
            })
            .unwrap()
        );
        assert!(directory.path().join("worker-result.json").is_file());
        assert!(!directory.path().join("result.json").exists());
    }

    #[test]
    fn child_observed_build_identity_mismatch_is_rejected() {
        let executable = std::env::current_exe().unwrap();
        let mut expected = report();
        verify_observed_identity(&expected, &executable).unwrap();
        expected.executable_sha256 = "0".repeat(64);
        assert!(verify_observed_identity(&expected, &executable).is_err());
        expected = report();
        expected.cargo_lock_sha256 = "0".repeat(64);
        assert!(verify_observed_identity(&expected, &executable).is_err());
    }

    #[test]
    fn supervisor_storage_stop_and_mismatched_progress_are_guarded() {
        let directory = tempfile::tempdir().unwrap();
        let objects = directory.path().join("store/objects/aa");
        fs::create_dir_all(&objects).unwrap();
        fs::write(objects.join("b".repeat(62)), [0; 1024]).unwrap();
        let mut initial = report();
        initial.workload.max_retained_bytes = 256;
        let mut wrong = report();
        wrong.workload.max_retained_bytes = 256;
        wrong.completed_files = 8;
        wrong.retained = Some(qualification::contracts::Inventory {
            objects: 1,
            encoded_bytes: 1,
            allocated_bytes: Some(1),
        });
        fs::write(
            directory.path().join("progress.json"),
            serde_json::to_vec(&wrong).unwrap(),
        )
        .unwrap();
        let mut child = fixture_child(directory.path(), "timeout");
        assert!(!supervise(&mut child, directory.path(), initial).unwrap());
        assert!(child.try_wait().unwrap().is_some());
        let result: Report = read_json(&directory.path().join("result.json")).unwrap();
        assert_eq!(result.status, "incomplete");
        assert_eq!(result.completed_files, 0);
        assert!(result.retained.is_none());
        assert!(result.diagnostic.unwrap().contains("storage"));
    }

    #[test]
    fn refuses_existing_destination_and_oversized_input() {
        let parent = tempfile::tempdir().unwrap();
        let run = parent.path().join("already-exists");
        fs::create_dir(&run).unwrap();
        fs::write(run.join("keep"), b"keep").unwrap();
        let workload = parent.path().join("workload.json");
        fs::write(
            &workload,
            include_bytes!("../fixtures/qualification/small-unique.json"),
        )
        .unwrap();
        let identity = parent.path().join("identity.json");
        fs::write(
            &identity,
            include_bytes!("../fixtures/qualification/build-identity-example.json"),
        )
        .unwrap();
        assert!(
            execute(Args {
                run_dir: run.clone(),
                workload: workload.clone(),
                build_identity: identity,
                worker: false
            })
            .is_err()
        );
        assert_eq!(fs::read(run.join("keep")).unwrap(), b"keep");
        fs::write(&workload, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(read_json::<Workload>(&workload).is_err());
    }
}
