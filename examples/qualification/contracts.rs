use serde::{Deserialize, Serialize};

use agentscm::{Error, Result, store::validate_id};

pub const PAGE_SIZE: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Repeated,
    Unique,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub version: u32,
    pub files: usize,
    pub content: Content,
    pub file_bytes: usize,
    pub batch_files: usize,
    pub history_edits: usize,
    pub read_repetitions: usize,
    pub max_wall_seconds: u64,
    pub max_retained_bytes: u64,
}

impl Workload {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !(1..=1_000_000).contains(&self.files)
            || !(16..=4_096).contains(&self.file_bytes)
            || !(1..=1_024).contains(&self.batch_files)
            || !(1..=10_000).contains(&self.history_edits)
            || !(1..=10_000).contains(&self.read_repetitions)
            || !(1..=3_600).contains(&self.max_wall_seconds)
            || !(1..=64 * 1024 * 1024 * 1024).contains(&self.max_retained_bytes)
        {
            return Err(Error::Limit("workload outside harness v1 bounds".into()));
        }
        if (self.files as u64) * (self.file_bytes as u64) > self.max_retained_bytes {
            return Err(Error::Limit("logical source exceeds storage budget".into()));
        }
        if self.files.div_ceil(self.batch_files) + self.history_edits
            > agentscm::history::MAX_HISTORY_REVISIONS
        {
            return Err(Error::Limit(
                "requested chain exceeds native history bound".into(),
            ));
        }
        Ok(())
    }

    pub fn last_edit(&self, index: usize) -> usize {
        if self.history_edits <= index {
            0
        } else {
            (self.history_edits - 1 - index) / self.files * self.files + index + 1
        }
    }

    pub fn bytes(&self, index: usize, edit: usize) -> Vec<u8> {
        let index = match self.content {
            Content::Repeated => 0,
            Content::Unique => index as u64,
        };
        let mut bytes = vec![b'x'; self.file_bytes];
        bytes[..8].copy_from_slice(&index.to_le_bytes());
        bytes[8..16].copy_from_slice(&(edit as u64).to_le_bytes());
        bytes
    }
}

/// Supplied build provenance, kept separate from the observed executable digest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    pub source_commit: String,
    pub source_tree: String,
    pub source_clean: bool,
    pub rustc_verbose: String,
    pub profile: String,
    pub flags: String,
    pub host: String,
}

impl BuildIdentity {
    pub fn validate(&self) -> Result<()> {
        for id in [&self.source_commit, &self.source_tree] {
            if id.len() != 40
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Invalid(
                    "expected lowercase Git SHA-1 build identity".into(),
                ));
            }
        }
        for value in [&self.rustc_verbose, &self.profile, &self.flags, &self.host] {
            if value.is_empty() || value.len() > 4_096 {
                return Err(Error::Invalid("missing or oversized build identity".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub objects: u64,
    pub encoded_bytes: u64,
    pub allocated_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Calls {
    pub checkpoint: u64,
    pub read_file: u64,
    pub list_files: u64,
    pub diff_page: u64,
    pub history_page: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub name: String,
    pub elapsed_ns: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    pub operation: String,
    pub status: String,
    pub scope: String,
}

pub fn operation_matrix() -> Vec<OperationResult> {
    [
        ("source_creation", "batched public Engine checkpoints"),
        ("history_edits", "single-file public Engine checkpoints"),
        (
            "source_validation",
            "all final paths and bytes plus retained old content",
        ),
        ("history_validation", "complete generated R1 chain"),
        ("diff_validation", "complete last-edit diff only"),
        (
            "external_encoded_reader_adapter",
            "not exercised: separate EncodedObjectReader adapter; Engine-backed reader only",
        ),
        (
            "complete_bundle_publication",
            "not exercised: bundle closure/native publication qualification",
        ),
        (
            "physical_workspace",
            "not exercised: real filesystem materialization",
        ),
        (
            "sparse_workspace",
            "not exercised: partial draft/materialization",
        ),
        (
            "team_admission",
            "not exercised: authenticated authority/policy/CAS",
        ),
        (
            "parallel_clients",
            "not exercised: concurrent readers/writers",
        ),
        (
            "recovery",
            "not exercised: interrupted source/authority/provider recovery",
        ),
        (
            "object_providers",
            "not exercised: remote adapter behavior/requests/billing",
        ),
    ]
    .into_iter()
    .map(|(operation, scope)| OperationResult {
        operation: operation.into(),
        status: "not_run".into(),
        scope: scope.into(),
    })
    .collect()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub version: u32,
    pub status: String,
    pub diagnostic: Option<String>,
    pub workload: Workload,
    pub declared_build: BuildIdentity,
    pub executable_sha256: String,
    pub cargo_lock_sha256: String,
    pub toolchain_file_sha256: String,
    pub source_profile: String,
    pub cache_setup: String,
    pub completed_files: usize,
    pub completed_history_edits: usize,
    pub validated_files: usize,
    pub validated_revisions: usize,
    pub head: Option<String>,
    pub calls_attempted: Calls,
    pub phases: Vec<Phase>,
    pub operation_results: Vec<OperationResult>,
    pub retained: Option<Inventory>,
    pub process_cpu_seconds: Option<f64>,
    pub process_peak_rss_bytes: Option<u64>,
    pub provider_requests: Option<u64>,
    pub supervised_elapsed_ns: Option<u64>,
    pub supervisor_inventory_passes: Option<u64>,
    pub supervisor_inventory_wall_ns: Option<u64>,
}

impl Report {
    pub fn validate_progress(&self, initial: &Self) -> Result<()> {
        self.validate_hashes()?;
        if self.version != 1
            || self.workload != initial.workload
            || self.declared_build != initial.declared_build
            || self.executable_sha256 != initial.executable_sha256
            || self.cargo_lock_sha256 != initial.cargo_lock_sha256
            || self.toolchain_file_sha256 != initial.toolchain_file_sha256
            || self.source_profile != initial.source_profile
            || self.cache_setup != initial.cache_setup
            || self.completed_files > self.workload.files
            || self.completed_history_edits > self.workload.history_edits
            || self.validated_files > self.completed_files
            || self.validated_revisions
                > self.workload.files.div_ceil(self.workload.batch_files)
                    + self.workload.history_edits
            || self.calls_attempted.checkpoint
                > (self.workload.files.div_ceil(self.workload.batch_files)
                    + self.workload.history_edits) as u64
            || self.calls_attempted.read_file
                > (self.workload.files + self.workload.read_repetitions + 3) as u64
            || self.calls_attempted.history_page
                > (self.workload.files.div_ceil(self.workload.batch_files)
                    + self.workload.history_edits
                    + 1) as u64
            || self.calls_attempted.diff_page > (self.workload.files * 128 + 1) as u64
            || self.calls_attempted.list_files > (self.workload.files * 256 + 2) as u64
            || self.process_cpu_seconds.is_some()
            || self.process_peak_rss_bytes.is_some()
            || self.provider_requests.is_some()
            || self.supervised_elapsed_ns.is_some()
            || self.supervisor_inventory_passes.is_some()
            || self.supervisor_inventory_wall_ns.is_some()
            || !["running", "failed", "completed"].contains(&self.status.as_str())
        {
            return Err(Error::Invalid(
                "worker result differs from supervised workload/evidence".into(),
            ));
        }
        if let Some(head) = &self.head {
            validate_id(head)?;
        }
        if self.head.is_some() != (self.completed_files > 0) {
            return Err(Error::Invalid(
                "worker head/file progress inconsistent".into(),
            ));
        }
        let phases = [
            "create_source_including_monitoring",
            "small_edits_including_monitoring",
            "initial_reopened_read_os_cache_uncontrolled",
            "validate_full_source_os_cache_uncontrolled",
            "validate_full_history",
            "validate_small_edit_diff",
            "repeated_reads_after_validation_os_cache_uncontrolled",
        ];
        if self.phases.len() > phases.len()
            || self
                .phases
                .iter()
                .zip(phases)
                .any(|(phase, name)| phase.name != name)
        {
            return Err(Error::Invalid("worker progress phases inconsistent".into()));
        }
        let matrix = operation_matrix();
        if self.operation_results.len() != matrix.len()
            || self
                .operation_results
                .iter()
                .zip(&matrix)
                .enumerate()
                .any(|(i, (row, expected))| {
                    row.operation != expected.operation
                        || row.scope != expected.scope
                        || if i < 5 {
                            !["not_run", "completed"].contains(&row.status.as_str())
                        } else {
                            row.status != "not_run"
                        }
                })
        {
            return Err(Error::Invalid(
                "worker operation matrix inconsistent".into(),
            ));
        }
        let full_source = self.completed_files == self.workload.files && self.head.is_some();
        let full_edits = full_source && self.completed_history_edits == self.workload.history_edits;
        let expected_revisions =
            self.workload.files.div_ceil(self.workload.batch_files) + self.workload.history_edits;
        let phase_thresholds = [1, 2, 7, 5, 6];
        let persisted_checkpoints =
            self.completed_files.div_ceil(self.workload.batch_files) + self.completed_history_edits;
        let phase_count = self.phases.len();
        let legal_state = match phase_count {
            0 => {
                self.completed_history_edits == 0
                    && self.calls_attempted.read_file == 0
                    && self.calls_attempted.list_files == 0
                    && self.calls_attempted.history_page == 0
                    && self.calls_attempted.diff_page == 0
                    && self.validated_files == 0
                    && self.validated_revisions == 0
                    && self.retained.is_none()
            }
            1 => {
                full_source
                    && self.completed_history_edits > 0
                    && self.calls_attempted.read_file == 0
                    && self.calls_attempted.list_files == 0
                    && self.calls_attempted.history_page == 0
                    && self.calls_attempted.diff_page == 0
                    && self.validated_files == 0
                    && self.validated_revisions == 0
                    && self.retained.is_none()
            }
            4 => {
                full_edits
                    && self.validated_files == self.workload.files
                    && self.calls_attempted.read_file == (self.workload.files + 1) as u64
                    && self.calls_attempted.list_files
                        >= self.workload.files.div_ceil(PAGE_SIZE) as u64
                    && self.validated_revisions == 0
                    && self.calls_attempted.history_page == 0
                    && self.calls_attempted.diff_page == 0
                    && self.retained.is_none()
            }
            7 => {
                full_edits
                    && self.validated_files == self.workload.files
                    && self.validated_revisions == expected_revisions
                    && self.calls_attempted.read_file
                        == (self.workload.files + self.workload.read_repetitions + 3) as u64
                    && self.calls_attempted.list_files
                        >= (self.workload.files.div_ceil(PAGE_SIZE) + 1) as u64
                    && self.calls_attempted.history_page
                        >= expected_revisions.div_ceil(PAGE_SIZE) as u64
                    && self.calls_attempted.diff_page > 0
                    && self.retained.is_some()
            }
            _ => false,
        };
        if !legal_state
            || self.completed_files == 0
            || self.calls_attempted.checkpoint != persisted_checkpoints as u64
            || self.operation_results[..5]
                .iter()
                .zip(phase_thresholds)
                .any(|(row, phase)| (row.status == "completed") != (phase_count >= phase))
            || (self.completed_files < self.workload.files
                && !self
                    .completed_files
                    .is_multiple_of(self.workload.batch_files))
            || (phase_count == 7) != (self.status == "completed")
            || self.diagnostic.is_some()
        {
            return Err(Error::Invalid(
                "worker progress is not a reachable persisted state".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_completed(&self, initial: &Self) -> Result<()> {
        self.validate_progress(initial)?;
        if self.status != "completed"
            || self.diagnostic.is_some()
            || self.head.is_none()
            || self.completed_files != self.workload.files
            || self.completed_history_edits != self.workload.history_edits
            || self.validated_files != self.workload.files
            || self.validated_revisions
                != self.workload.files.div_ceil(self.workload.batch_files)
                    + self.workload.history_edits
            || self.calls_attempted.checkpoint != self.validated_revisions as u64
            || self.calls_attempted.read_file
                != (self.workload.files + self.workload.read_repetitions + 3) as u64
            || self.calls_attempted.list_files == 0
            || self.calls_attempted.history_page == 0
            || self.calls_attempted.diff_page == 0
            || self
                .retained
                .as_ref()
                .is_none_or(|retained| retained.objects == 0 || retained.encoded_bytes == 0)
            || self.operation_results[..5]
                .iter()
                .any(|row| row.status != "completed")
        {
            return Err(Error::Invalid(
                "worker completion counts/evidence inconsistent".into(),
            ));
        }
        let expected = [
            "create_source_including_monitoring",
            "small_edits_including_monitoring",
            "initial_reopened_read_os_cache_uncontrolled",
            "validate_full_source_os_cache_uncontrolled",
            "validate_full_history",
            "validate_small_edit_diff",
            "repeated_reads_after_validation_os_cache_uncontrolled",
        ];
        if self
            .phases
            .iter()
            .map(|phase| phase.name.as_str())
            .collect::<Vec<_>>()
            != expected
        {
            return Err(Error::Invalid("worker phases incomplete".into()));
        }
        Ok(())
    }

    pub fn validate_hashes(&self) -> Result<()> {
        for hash in [
            &self.executable_sha256,
            &self.cargo_lock_sha256,
            &self.toolchain_file_sha256,
        ] {
            validate_id(hash)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn small() -> Workload {
        serde_json::from_str(include_str!(
            "../../fixtures/qualification/small-unique.json"
        ))
        .unwrap()
    }

    #[test]
    fn workload_rejects_unknown_fields_and_unbounded_work() {
        let mut workload = small();
        workload.files = 1_000_001;
        assert!(workload.validate().is_err());
        workload.files = 0;
        assert!(workload.validate().is_err());
        workload = small();
        workload.history_edits = 0;
        assert!(workload.validate().is_err());
        let json = include_str!("../../fixtures/qualification/small-unique.json");
        assert!(
            serde_json::from_str::<Workload>(
                &json.replace("\"version\": 1", "\"version\": 1, \"typo\": 2")
            )
            .is_err()
        );
    }

    #[test]
    fn native_history_boundary_is_exact() {
        let mut workload = small();
        workload.files = 99_999;
        workload.batch_files = 1;
        workload.history_edits = 1;
        workload.validate().unwrap();
        workload.files += 1;
        assert!(matches!(workload.validate(), Err(Error::Limit(_))));
    }

    #[test]
    fn dataset_modes_and_edit_wrap_are_distinct() {
        let mut workload = small();
        assert_ne!(workload.bytes(0, 0), workload.bytes(1, 0));
        workload.content = Content::Repeated;
        assert_eq!(workload.bytes(0, 0), workload.bytes(1, 0));
        workload.files = 3;
        workload.history_edits = 5;
        assert_eq!(
            (0..3).map(|i| workload.last_edit(i)).collect::<Vec<_>>(),
            [4, 5, 3]
        );
        assert_ne!(workload.bytes(0, 0), workload.bytes(0, 1));
    }
}
