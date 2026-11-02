//! Completeness of declared observations, independently of their execution assurance.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::evidence::{EvidenceOutcome, validate_identifier};
use crate::{Error, Result};

use super::{
    ConfigBinding, ImmutablePlan, MAX_CELLS, MAX_ITEMS, PlanInputs, SourceReader,
    TrustedRequirements, bounded_bytes, digest, domain_digest, json, version,
};

/// A worker can report these states without proving they happened. Only success or explicit
/// failure describe completed execution; skipped and uncertain states cannot satisfy coverage.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationOutcome {
    Success,
    Failed,
    Skipped,
    Cancelled,
    Unavailable,
    Incomplete,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceObservation {
    pub id: String,
    pub outcome: ObservationOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellObservation {
    pub job: String,
    pub cell: String,
    pub outcome: ObservationOutcome,
    pub services: Vec<ServiceObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoverageReport {
    pub version: u32,
    pub plan_sha256: String,
    pub cells: Vec<CellObservation>,
}

impl CoverageReport {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let report: Self = json::from_slice(bytes)?;
        report.validate()?;
        Ok(report)
    }

    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        digest("coverage plan digest", &self.plan_sha256)?;
        if self.cells.len() > MAX_CELLS {
            return Err(Error::Limit("coverage cell count exceeds limit".into()));
        }
        let mut keys = BTreeSet::new();
        for cell in &self.cells {
            validate_identifier("observed job", &cell.job)?;
            validate_identifier("observed cell", &cell.cell)?;
            if !keys.insert((&cell.job, &cell.cell)) || cell.services.len() > MAX_ITEMS {
                return Err(Error::Invalid(
                    "duplicate cell or excess observed services".into(),
                ));
            }
            let mut services = BTreeSet::new();
            for service in &cell.services {
                validate_identifier("observed service", &service.id)?;
                if !services.insert(&service.id) {
                    return Err(Error::Invalid("duplicate observed service".into()));
                }
            }
        }
        bounded_bytes(self).map(|_| ())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value = self.clone();
        value
            .cells
            .sort_by(|a, b| (&a.job, &a.cell).cmp(&(&b.job, &b.cell)));
        for cell in &mut value.cells {
            cell.services.sort_by(|a, b| a.id.cmp(&b.id));
        }
        bounded_bytes(&value)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(b"agentscm.local-ci.coverage.v1\0", &self.canonical_bytes()?)
    }
}

/// Structural inventory and declared outcomes only. This result does not authenticate a worker,
/// establish freshness, prove execution or authorize admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageAssessment {
    pub inventory_complete: bool,
    pub outcome: EvidenceOutcome,
    pub expected_cells: usize,
    pub observed_cells: usize,
    pub issues: Vec<String>,
}

/// Reload current trusted configuration and recompute the plan from independent controller
/// observations. `binding` and `inputs` must never be populated from the submitted plan/report.
pub fn verify_coverage(
    binding: ConfigBinding,
    inputs: PlanInputs,
    reader: &impl SourceReader,
    submitted: &ImmutablePlan,
    report: &CoverageReport,
) -> Result<CoverageAssessment> {
    let expected = TrustedRequirements::load(binding, reader)?.resolve(inputs, reader)?;
    submitted.validate()?;
    report.validate()?;
    if expected.plan().canonical_bytes()? != submitted.canonical_bytes()?
        || report.plan_sha256 != expected.plan().digest()?
    {
        return Err(Error::Invalid(
            "submitted plan or coverage does not match current trusted plan".into(),
        ));
    }
    let plan = expected.plan();
    let omitted: BTreeSet<_> = plan
        .omissions
        .iter()
        .map(|o| (o.job.as_str(), o.cell.as_str()))
        .collect();
    let required: BTreeMap<_, _> = plan
        .cells
        .iter()
        .filter(|c| !omitted.contains(&c.key()))
        .map(|c| (c.key(), c))
        .collect();
    let observed: BTreeMap<_, _> = report
        .cells
        .iter()
        .map(|c| ((c.job.as_str(), c.cell.as_str()), c))
        .collect();
    let mut issues = Vec::new();
    let mut inventory_complete = true;
    let mut incomplete = false;
    let mut failed = false;
    for key in observed.keys() {
        if !required.contains_key(key) {
            inventory_complete = false;
            issues.push(format!("unexpected or omitted cell: {}/{}", key.0, key.1));
        }
    }
    for (key, cell) in &required {
        let Some(result) = observed.get(key) else {
            inventory_complete = false;
            issues.push(format!("missing cell: {}/{}", key.0, key.1));
            continue;
        };
        let expected_services: BTreeSet<_> = cell.services.iter().map(|s| s.id.as_str()).collect();
        let observed_services: BTreeSet<_> =
            result.services.iter().map(|s| s.id.as_str()).collect();
        if expected_services != observed_services {
            inventory_complete = false;
            issues.push(format!("service inventory differs: {}/{}", key.0, key.1));
        }
        match result.outcome {
            ObservationOutcome::Success => {}
            ObservationOutcome::Failed => failed = true,
            _ => {
                incomplete = true;
                issues.push(format!("cell did not complete: {}/{}", key.0, key.1));
            }
        }
        // A service failure prevents the required execution environment, even if the command
        // itself reported success or failure.
        if result
            .services
            .iter()
            .any(|s| s.outcome != ObservationOutcome::Success)
        {
            incomplete = true;
            issues.push(format!(
                "required services not successful: {}/{}",
                key.0, key.1
            ));
        }
    }
    let outcome = if !inventory_complete || incomplete {
        EvidenceOutcome::Incomplete
    } else if failed {
        EvidenceOutcome::Fail
    } else {
        EvidenceOutcome::Pass
    };
    Ok(CoverageAssessment {
        inventory_complete,
        outcome,
        expected_cells: required.len(),
        observed_cells: observed.keys().filter(|k| required.contains_key(k)).count(),
        issues,
    })
}
