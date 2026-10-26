//! Durable local workflow and skill authoring. Imported text is data until an operator commits
//! exported resources and the existing workflow compiler validates their exact native revision.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::redirect::Policy;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::pipeline_rules::{MergeTrigger, PipelineRecipe};
use crate::skill_bundle::{
    MAX_SKILL_RESOURCE_BYTES, MAX_SKILL_RESOURCES, SkillBundle, SkillResource,
};
use crate::skill_workflow::action::{ActionDefinition, ActionInput, ProductiveAction};
use crate::skill_workflow::{SkillReference, StepAction, StepDefinition};
use crate::store::digest;
use crate::{Error, Result};

const APPLICATION_ID: i64 = 0x4153_574c;
const SCHEMA_VERSION: i64 = 1;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_FILES: usize = 128;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STEPS: usize = 32;
const PREVIEW_SECONDS: i64 = 15 * 60;
const IMPORT_DEADLINE: Duration = Duration::from_secs(20);
const MAX_COMPOSED_WORKFLOWS: usize = 16;
type WorkflowRow = (
    String,
    String,
    u64,
    String,
    Option<String>,
    Option<String>,
    i64,
    bool,
);

const SCHEMA: &str = r#"
CREATE TABLE skill_versions(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, version INTEGER NOT NULL CHECK(version>0),
  generation INTEGER NOT NULL CHECK(generation>0), digest TEXT NOT NULL UNIQUE,
  entrypoint TEXT NOT NULL, files_json TEXT NOT NULL, package_json TEXT NOT NULL,
  provenance_json TEXT NOT NULL,
  created_at INTEGER NOT NULL, archived INTEGER NOT NULL CHECK(archived IN (0,1)),
  UNIQUE(name,version)) STRICT;
CREATE TABLE import_previews(
  id TEXT PRIMARY KEY, digest TEXT NOT NULL, expires_at INTEGER NOT NULL,
  skill_json TEXT NOT NULL, files_json TEXT NOT NULL, warnings_json TEXT NOT NULL,
  blocked INTEGER NOT NULL CHECK(blocked IN (0,1))) STRICT;
CREATE TABLE workflow_drafts(
  id TEXT PRIMARY KEY, name TEXT NOT NULL, summary TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation>0), steps_json TEXT NOT NULL,
  trigger_json TEXT, follow_on_json TEXT, updated_at INTEGER NOT NULL,
  archived INTEGER NOT NULL CHECK(archived IN (0,1))) STRICT;
CREATE TABLE workflow_versions(
  id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflow_drafts(id),
  version INTEGER NOT NULL CHECK(version>0), digest TEXT NOT NULL UNIQUE,
  definition_json TEXT NOT NULL, skill_versions_json TEXT NOT NULL,
  created_at INTEGER NOT NULL, archived INTEGER NOT NULL CHECK(archived IN (0,1)),
  UNIQUE(workflow_id,version)) STRICT;
CREATE TABLE workflow_attachments(
  id TEXT NOT NULL, repository TEXT NOT NULL, workflow_version_id TEXT NOT NULL REFERENCES workflow_versions(id),
  generation INTEGER NOT NULL CHECK(generation>0), state TEXT NOT NULL,
  params_json TEXT NOT NULL, source_binding_json TEXT, pipeline_rule_json TEXT,
  diagnostics_json TEXT NOT NULL, PRIMARY KEY(repository,id)) STRICT;
CREATE TABLE idempotency(
  scope TEXT NOT NULL, key TEXT NOT NULL, request_digest TEXT NOT NULL, result_id TEXT NOT NULL,
  PRIMARY KEY(scope,key)) STRICT;
"#;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Provenance {
    Markdown {
        locator: String,
    },
    Github {
        locator: String,
        commit: String,
        path: String,
    },
    NpxDescriptor {
        locator: String,
        commit: String,
        path: String,
        command: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillVersion {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub generation: u64,
    pub digest: String,
    pub entrypoint: String,
    pub files: Vec<FileSummary>,
    pub provenance: Provenance,
    pub created_at: i64,
    pub archived: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileSummary {
    pub path: String,
    pub sha256: String,
    pub size: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportSource {
    Markdown {
        name: String,
        markdown: String,
        #[serde(default)]
        resources: Vec<PackageFile>,
    },
    Github {
        repository: String,
        commit: String,
        path: String,
        #[serde(default)]
        resources: Vec<String>,
    },
    NpxDescriptor {
        command: String,
        commit: String,
        path: Option<String>,
        #[serde(default)]
        resources: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportPreviewRequest {
    pub source: ImportSource,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportPreview {
    pub preview_id: String,
    pub digest: String,
    pub expires_at: i64,
    pub skill: PreviewSkill,
    pub files: Vec<FileSummary>,
    pub warnings: Vec<String>,
    pub blocked: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreviewSkill {
    pub name: String,
    pub entrypoint: String,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcceptImport {
    pub preview_id: String,
    pub digest: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkflowStep {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub required: bool,
    #[serde(flatten)]
    pub kind: WorkflowStepKind,
}

impl<'de> Deserialize<'de> for WorkflowStep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: String,
            label: String,
            #[serde(default)]
            needs: Vec<String>,
            required: bool,
            kind: String,
            skill_version_id: Option<String>,
            runner: Option<String>,
            resource: Option<String>,
            target: Option<String>,
            workflow_version_id: Option<String>,
            inputs: Option<BTreeMap<String, String>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let unexpected = |allowed: &[&str]| {
            [
                ("skill_version_id", wire.skill_version_id.is_some()),
                ("runner", wire.runner.is_some()),
                ("resource", wire.resource.is_some()),
                ("target", wire.target.is_some()),
                ("workflow_version_id", wire.workflow_version_id.is_some()),
                ("inputs", wire.inputs.is_some()),
            ]
            .into_iter()
            .find_map(|(name, present)| (present && !allowed.contains(&name)).then_some(name))
        };
        let missing = |name: &'static str| serde::de::Error::missing_field(name);
        let kind = match wire.kind.as_str() {
            "skill" => {
                if let Some(name) = unexpected(&["skill_version_id"]) {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}` is not valid for skill step"
                    )));
                }
                WorkflowStepKind::Skill {
                    skill_version_id: wire
                        .skill_version_id
                        .ok_or_else(|| missing("skill_version_id"))?,
                }
            }
            "build" => {
                if let Some(name) = unexpected(&["skill_version_id", "runner"]) {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}` is not valid for build step"
                    )));
                }
                WorkflowStepKind::Build {
                    skill_version_id: wire
                        .skill_version_id
                        .ok_or_else(|| missing("skill_version_id"))?,
                    runner: wire.runner.ok_or_else(|| missing("runner"))?,
                }
            }
            "deploy" => {
                if let Some(name) = unexpected(&["skill_version_id", "resource", "target"]) {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}` is not valid for deploy step"
                    )));
                }
                WorkflowStepKind::Deploy {
                    skill_version_id: wire
                        .skill_version_id
                        .ok_or_else(|| missing("skill_version_id"))?,
                    resource: wire.resource.ok_or_else(|| missing("resource"))?,
                    target: wire.target.ok_or_else(|| missing("target"))?,
                }
            }
            "action" => {
                if let Some(name) = unexpected(&["skill_version_id", "resource"]) {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}` is not valid for action step"
                    )));
                }
                WorkflowStepKind::Action {
                    skill_version_id: wire
                        .skill_version_id
                        .ok_or_else(|| missing("skill_version_id"))?,
                    resource: wire.resource.ok_or_else(|| missing("resource"))?,
                }
            }
            "workflow" => {
                if let Some(name) = unexpected(&["workflow_version_id", "inputs"]) {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}` is not valid for workflow step"
                    )));
                }
                WorkflowStepKind::Workflow {
                    workflow_version_id: wire
                        .workflow_version_id
                        .ok_or_else(|| missing("workflow_version_id"))?,
                    inputs: wire.inputs.unwrap_or_default(),
                }
            }
            other => {
                return Err(serde::de::Error::unknown_variant(
                    other,
                    &["skill", "build", "deploy", "action", "workflow"],
                ));
            }
        };
        Ok(Self {
            id: wire.id,
            label: wire.label,
            needs: wire.needs,
            required: wire.required,
            kind,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowStepKind {
    Skill {
        skill_version_id: String,
    },
    Build {
        skill_version_id: String,
        runner: String,
    },
    Deploy {
        skill_version_id: String,
        resource: String,
        target: String,
    },
    Action {
        skill_version_id: String,
        resource: String,
    },
    Workflow {
        workflow_version_id: String,
        #[serde(default)]
        inputs: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    ConfirmedMerge {
        target: String,
        #[serde(default)]
        labels_all: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FollowWhen {
    Success,
    Failure,
    Always,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FollowOn {
    pub workflow_version_id: String,
    pub when: FollowWhen,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDraft {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub generation: u64,
    pub steps: Vec<WorkflowStep>,
    pub trigger: Option<Trigger>,
    pub follow_on: Option<FollowOn>,
    pub updated_at: i64,
    pub archived: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowVersion {
    pub id: String,
    pub workflow_id: String,
    pub version: u32,
    pub digest: String,
    pub definition: WorkflowDraft,
    pub skill_versions: Vec<String>,
    pub created_at: i64,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publication {
    Published {
        version: Box<WorkflowVersion>,
        diagnostics: Vec<Diagnostic>,
    },
    Rejected {
        diagnostics: Vec<Diagnostic>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkflow {
    pub name: String,
    pub summary: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkflow {
    pub expected_generation: u64,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub steps: Option<Vec<WorkflowStep>>,
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger: Option<Option<Trigger>>,
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub follow_on: Option<Option<FollowOn>>,
}

fn deserialize_nullable_patch<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishWorkflow {
    pub expected_generation: u64,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationRequest {
    pub workflow_version_id: String,
    pub repository: Option<String>,
    pub source_revision: Option<String>,
    pub event: Option<SimulationEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationEvent {
    pub target: String,
    #[serde(default)]
    pub labels: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub level: String,
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationStep {
    pub id: String,
    pub status: String,
    pub requires: Vec<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub outcome: String,
    pub steps: Vec<SimulationStep>,
    pub trigger_match: Option<bool>,
    pub diagnostics: Vec<Diagnostic>,
    pub side_effects: String,
    pub admission: String,
    pub deployment: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub repository: String,
    pub workflow_version_id: String,
    pub generation: u64,
    pub state: String,
    pub params: BTreeMap<String, Value>,
    pub source_binding: Option<Value>,
    pub pipeline_rule: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PutAttachment {
    pub expected_generation: Option<u64>,
    pub workflow_version_id: String,
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidateAttachment {
    pub expected_generation: u64,
    pub source_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationRequest {
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnableRequest {
    pub expected_generation: u64,
    pub enabled: bool,
}

pub struct Library {
    db: Connection,
}

#[derive(Default)]
struct Composition {
    steps: Vec<StepDefinition>,
    skill_ids: BTreeSet<String>,
    generated_actions: Vec<PackageFile>,
    step_origins: BTreeMap<String, String>,
    unmet: Vec<String>,
    warnings: Vec<String>,
    workflows: usize,
}

struct CompositionScope {
    prefix: String,
    external_needs: Vec<String>,
    ancestor_required: bool,
}

impl Library {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let root = data_dir.join("workflow-library");
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let mut db = Connection::open(root.join("state.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let tables: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if app == 0 && version == 0 && tables == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if app != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(Error::Invalid(
                "unsupported workflow library journal identity or schema".into(),
            ));
        }
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn skills(&self) -> Result<Vec<SkillVersion>> {
        let mut stmt = self.db.prepare("SELECT id,name,version,generation,digest,entrypoint,files_json,provenance_json,created_at,archived FROM skill_versions ORDER BY created_at DESC,id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, u64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, bool>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|r| {
                Ok(SkillVersion {
                    id: r.0,
                    name: r.1,
                    version: r.2,
                    generation: r.3,
                    digest: r.4,
                    entrypoint: r.5,
                    files: serde_json::from_str(&r.6)?,
                    provenance: serde_json::from_str(&r.7)?,
                    created_at: r.8,
                    archived: r.9,
                })
            })
            .collect()
    }

    pub fn save_preview(
        &mut self,
        skill: PreviewSkill,
        files: &[PackageFile],
        warnings: Vec<String>,
    ) -> Result<ImportPreview> {
        validate_package(files, &skill.entrypoint)?;
        validate_name(&skill.name)?;
        let canonical = serde_json::to_vec(&(skill.clone(), files))?;
        let package_digest = digest(&canonical);
        let id = new_id("imp")?;
        let expires_at = now() + PREVIEW_SECONDS;
        let summaries = summarize(files);
        let preview = ImportPreview {
            preview_id: id.clone(),
            digest: package_digest.clone(),
            expires_at,
            skill: skill.clone(),
            files: summaries,
            warnings,
            blocked: false,
        };
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM import_previews WHERE expires_at < ?1", [now()])?;
        tx.execute("INSERT INTO import_previews(id,digest,expires_at,skill_json,files_json,warnings_json,blocked) VALUES(?1,?2,?3,?4,?5,?6,0)",params![id,package_digest,expires_at,serde_json::to_string(&skill)?,serde_json::to_string(files)?,serde_json::to_string(&preview.warnings)?])?;
        tx.commit()?;
        Ok(preview)
    }

    pub fn accept_import(&mut self, request: &AcceptImport) -> Result<SkillVersion> {
        validate_key(&request.idempotency_key)?;
        let request_digest = digest(&serde_json::to_vec(request)?);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = idempotent_in(
            &tx,
            "skill_import",
            &request.idempotency_key,
            &request_digest,
        )? {
            tx.commit()?;
            let skill = self.skill(&id)?;
            if skill.archived || skill.generation != 1 {
                return Err(Error::Conflict(
                    "idempotent skill result changed after the original operation".into(),
                ));
            }
            return Ok(skill);
        }
        let row:Option<(String,i64,String,String,bool)>=tx.query_row("SELECT digest,expires_at,skill_json,files_json,blocked FROM import_previews WHERE id=?1",[&request.preview_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((retained_digest, expires_at, skill_json, files_json, blocked)) = row else {
            return Err(Error::NotFound("skill import preview".into()));
        };
        if blocked || expires_at < now() || retained_digest != request.digest {
            return Err(Error::Conflict(
                "skill import preview is blocked, stale or changed".into(),
            ));
        }
        let skill: PreviewSkill = serde_json::from_str(&skill_json)?;
        let files: Vec<PackageFile> = serde_json::from_str(&files_json)?;
        validate_package(&files, &skill.entrypoint)?;
        if let Some((existing, archived)) = tx
            .query_row(
                "SELECT id,archived FROM skill_versions WHERE digest=?1",
                [&retained_digest],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?
        {
            if archived {
                return Err(Error::Conflict(
                    "identical skill content is archived; use the retained version explicitly or import changed reviewed content".into(),
                ));
            }
            record_idempotency_in(
                &tx,
                "skill_import",
                &request.idempotency_key,
                &request_digest,
                &existing,
            )?;
            tx.commit()?;
            return self.skill(&existing);
        }
        let version = tx.query_row(
            "SELECT coalesce(max(version),0)+1 FROM skill_versions WHERE name=?1",
            [&skill.name],
            |r| r.get::<_, u32>(0),
        )?;
        let id = new_id("skl")?;
        let created_at = now();
        tx.execute(
            "INSERT INTO skill_versions(id,name,version,generation,digest,entrypoint,files_json,package_json,provenance_json,created_at,archived) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?9,0)",
            params![id,skill.name,version,retained_digest,skill.entrypoint,serde_json::to_string(&summarize(&files))?,serde_json::to_string(&files)?,serde_json::to_string(&skill.provenance)?,created_at],
        )?;
        record_idempotency_in(
            &tx,
            "skill_import",
            &request.idempotency_key,
            &request_digest,
            &id,
        )?;
        tx.commit()?;
        self.skill(&id)
    }

    pub fn archive_skill(&mut self, id: &str, expected: u64) -> Result<SkillVersion> {
        validate_id(id)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut drafts = tx.prepare("SELECT steps_json FROM workflow_drafts WHERE archived=0")?;
        let draft_rows = drafts
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(drafts);
        for steps in draft_rows {
            let steps: Vec<WorkflowStep> = serde_json::from_str(&steps)?;
            if skill_refs(&steps).iter().any(|skill| skill == id) {
                return Err(Error::Conflict(
                    "skill is referenced by an active workflow draft".into(),
                ));
            }
        }
        let mut versions = tx.prepare("SELECT skill_versions_json FROM workflow_versions")?;
        let version_rows = versions
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(versions);
        for skills in version_rows {
            let skills: Vec<String> = serde_json::from_str(&skills)?;
            if skills.iter().any(|skill| skill == id) {
                return Err(Error::Conflict(
                    "skill is referenced by an immutable workflow version".into(),
                ));
            }
        }
        if tx.execute("UPDATE skill_versions SET archived=1,generation=generation+1 WHERE id=?1 AND generation=?2 AND archived=0",params![id,expected])? != 1 {
            return Err(Error::Conflict("skill generation conflict".into()));
        }
        tx.commit()?;
        self.skill(id)
    }

    pub fn workflows(&self) -> Result<Value> {
        let mut stmt = self.db.prepare(
            "SELECT id FROM workflow_drafts WHERE archived=0 ORDER BY updated_at DESC,id",
        )?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut workflows = Vec::new();
        for id in ids {
            workflows.push(json!({"draft":self.workflow(&id)?,"versions":self.versions(&id)?}));
        }
        Ok(json!({"version":1,"workflows":workflows}))
    }

    pub fn workflow_record(&self, id: &str) -> Result<Value> {
        Ok(json!({"draft": self.workflow(id)?, "versions": self.versions(id)?}))
    }

    pub fn create_workflow(&mut self, request: &CreateWorkflow) -> Result<WorkflowDraft> {
        validate_name(&request.name)?;
        validate_text(&request.summary, 1000, "workflow summary")?;
        validate_key(&request.idempotency_key)?;
        let request_digest = digest(&serde_json::to_vec(request)?);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = idempotent_in(
            &tx,
            "workflow_create",
            &request.idempotency_key,
            &request_digest,
        )? {
            tx.commit()?;
            let workflow = self.workflow(&id)?;
            if workflow.generation != 1
                || workflow.archived
                || workflow.name != request.name
                || workflow.summary != request.summary
            {
                return Err(Error::Conflict(
                    "idempotent workflow result changed after the original operation".into(),
                ));
            }
            return Ok(workflow);
        }
        let id = new_id("wfl")?;
        let updated_at = now();
        tx.execute(
            "INSERT INTO workflow_drafts VALUES(?1,?2,?3,1,'[]',NULL,NULL,?4,0)",
            params![id, request.name, request.summary, updated_at],
        )?;
        record_idempotency_in(
            &tx,
            "workflow_create",
            &request.idempotency_key,
            &request_digest,
            &id,
        )?;
        tx.commit()?;
        self.workflow(&id)
    }

    pub fn update_workflow(&mut self, id: &str, request: &UpdateWorkflow) -> Result<WorkflowDraft> {
        let mut draft = self.workflow(id)?;
        if draft.archived || draft.generation != request.expected_generation {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        if let Some(name) = &request.name {
            validate_name(name)?;
            draft.name = name.clone();
        }
        if let Some(summary) = &request.summary {
            validate_text(summary, 1000, "workflow summary")?;
            draft.summary = summary.clone();
        }
        if let Some(steps) = &request.steps {
            validate_steps(steps)?;
            draft.steps = steps.clone();
        }
        if let Some(trigger) = &request.trigger {
            validate_trigger(trigger.as_ref())?;
            draft.trigger = trigger.clone();
        }
        if let Some(follow) = &request.follow_on {
            validate_follow_on(follow.as_ref())?;
            draft.follow_on = follow.clone();
        }
        draft.updated_at = now();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE workflow_drafts SET name=?1,summary=?2,generation=generation+1,steps_json=?3,trigger_json=?4,follow_on_json=?5,updated_at=?6 WHERE id=?7 AND generation=?8 AND archived=0",params![draft.name,draft.summary,serde_json::to_string(&draft.steps)?,draft.trigger.as_ref().map(serde_json::to_string).transpose()?,draft.follow_on.as_ref().map(serde_json::to_string).transpose()?,draft.updated_at,id,request.expected_generation])?;
        if changed != 1 {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        tx.commit()?;
        self.workflow(id)
    }

    pub fn delete_workflow(&mut self, id: &str, expected: u64) -> Result<()> {
        validate_id(id)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let versions: i64 = tx.query_row(
            "SELECT count(*) FROM workflow_versions WHERE workflow_id=?1",
            [id],
            |r| r.get(0),
        )?;
        if versions != 0 {
            return Err(Error::Conflict(
                "versioned workflows are archived, not deleted".into(),
            ));
        }
        if tx.execute(
            "DELETE FROM workflow_drafts WHERE id=?1 AND generation=?2",
            params![id, expected],
        )? != 1
        {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn archive_workflow(&mut self, id: &str, expected: u64) -> Result<WorkflowDraft> {
        validate_id(id)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE workflow_drafts SET archived=1,generation=generation+1,updated_at=?1 WHERE id=?2 AND generation=?3 AND archived=0",
            params![now(), id, expected],
        )? != 1 {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        tx.commit()?;
        self.workflow(id)
    }

    pub fn publish(&mut self, id: &str, request: &PublishWorkflow) -> Result<Publication> {
        validate_key(&request.idempotency_key)?;
        let request_digest = digest(&serde_json::to_vec(&(id, request))?);
        if let Some(version_id) = idempotent_in(
            &self.db,
            "workflow_publish",
            &request.idempotency_key,
            &request_digest,
        )? {
            let version = self.version(&version_id)?;
            let diagnostics = self.lint(&version.definition)?;
            return Ok(Publication::Published {
                version: Box::new(version),
                diagnostics,
            });
        }
        let draft = self.workflow(id)?;
        if draft.archived || draft.generation != request.expected_generation {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        let diagnostics = self.lint(&draft)?;
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "invalid_composition")
        {
            return Ok(Publication::Rejected { diagnostics });
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(version_id) = idempotent_in(
            &tx,
            "workflow_publish",
            &request.idempotency_key,
            &request_digest,
        )? {
            tx.commit()?;
            let version = self.version(&version_id)?;
            let diagnostics = self.lint(&version.definition)?;
            return Ok(Publication::Published {
                version: Box::new(version),
                diagnostics,
            });
        }
        let retained = workflow_from(&tx, id)?;
        if retained.archived
            || retained.generation != request.expected_generation
            || retained != draft
        {
            return Err(Error::Conflict("workflow generation conflict".into()));
        }
        let definition = serde_json::to_vec(&retained)?;
        let identity = digest(&definition);
        if let Some(existing) = tx
            .query_row(
                "SELECT id FROM workflow_versions WHERE digest=?1",
                [&identity],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            record_idempotency_in(
                &tx,
                "workflow_publish",
                &request.idempotency_key,
                &request_digest,
                &existing,
            )?;
            tx.commit()?;
            return Ok(Publication::Published {
                version: Box::new(self.version(&existing)?),
                diagnostics,
            });
        }
        let number = tx.query_row(
            "SELECT coalesce(max(version),0)+1 FROM workflow_versions WHERE workflow_id=?1",
            [id],
            |r| r.get::<_, u32>(0),
        )?;
        let version_id = format!("wfv_{}", &identity[..32]);
        let skills = skill_refs(&retained.steps);
        tx.execute(
            "INSERT INTO workflow_versions VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
            params![
                version_id,
                id,
                number,
                identity,
                String::from_utf8(definition)
                    .map_err(|_| Error::Invalid("workflow JSON encoding".into()))?,
                serde_json::to_string(&skills)?,
                now()
            ],
        )?;
        record_idempotency_in(
            &tx,
            "workflow_publish",
            &request.idempotency_key,
            &request_digest,
            &version_id,
        )?;
        tx.commit()?;
        Ok(Publication::Published {
            version: Box::new(self.version(&version_id)?),
            diagnostics,
        })
    }

    pub fn simulate(&self, request: &SimulationRequest) -> Result<Simulation> {
        if let Some(repository) = &request.repository {
            validate_repository(repository)?;
        }
        if let Some(revision) = &request.source_revision {
            validate_revision(revision)?;
        }
        let version = self.version(&request.workflow_version_id)?;
        let mut diagnostics = self.lint(&version.definition)?;
        let mut owner_warnings = Vec::new();
        self.note_archived_owner(&version, &mut owner_warnings)?;
        diagnostics.extend(owner_warnings.into_iter().map(|message| Diagnostic {
            level: "warning".into(),
            code: "archived_workflow_reference".into(),
            message,
            path: None,
        }));
        let trigger_match = match (&version.definition.trigger, &request.event) {
            (Some(Trigger::ConfirmedMerge { target, labels_all }), Some(event)) => {
                Some(&event.target == target && labels_all.is_subset(&event.labels))
            }
            (Some(_), None) => None,
            (None, _) => None,
        };
        let blocked = diagnostics.iter().any(|d| d.level == "error");
        let mut composition = Composition::default();
        let mut stack = BTreeSet::new();
        let _ = self.compose_definition(
            &version.id,
            &version.definition,
            CompositionScope {
                prefix: String::new(),
                external_needs: Vec::new(),
                ancestor_required: true,
            },
            &mut stack,
            &mut composition,
        );
        let mut steps = composition
            .steps
            .iter()
            .map(|step| SimulationStep {
                id: step.id.clone(),
                status: if blocked { "blocked" } else { "would_run" }.into(),
                requires: vec!["committed resources and productive action profile".into()],
                detail: "Simulation compiled the immutable action declaration; no process or external action ran.".into(),
            })
            .collect::<Vec<_>>();
        for step in &version.definition.steps {
            if matches!(
                &step.kind,
                WorkflowStepKind::Skill { .. }
                    | WorkflowStepKind::Build { .. }
                    | WorkflowStepKind::Deploy { .. }
            ) {
                steps.push(SimulationStep {
                    id: step.id.clone(),
                    status: "blocked".into(),
                    requires: match &step.kind {
                        WorkflowStepKind::Skill { .. } => vec!["explicit productive action declaration".into()],
                        WorkflowStepKind::Build { .. } => vec!["reviewed command context and runtime binding".into()],
                        WorkflowStepKind::Deploy { .. } => vec!["separate deployment executor and authority".into()],
                        WorkflowStepKind::Action { .. } | WorkflowStepKind::Workflow { .. } => {
                            unreachable!()
                        }
                    },
                    detail: "The declaration was inspected, but the existing runtime cannot execute this step as configured.".into(),
                });
            }
        }
        Ok(Simulation {
            outcome: if blocked {
                "failed"
            } else if diagnostics.is_empty() {
                "passed"
            } else {
                "passed_with_warnings"
            }
            .into(),
            steps,
            trigger_match,
            diagnostics,
            side_effects: "none".into(),
            admission: "not_assessed".into(),
            deployment: "not_attempted".into(),
        })
    }

    pub fn attachments(&self, repository: &str) -> Result<Vec<Attachment>> {
        validate_repository(repository)?;
        let mut stmt=self.db.prepare("SELECT id,workflow_version_id,generation,state,params_json,source_binding_json,pipeline_rule_json,diagnostics_json FROM workflow_attachments WHERE repository=?1 ORDER BY id")?;
        let rows = stmt
            .query_map([repository], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|r| {
                Ok(Attachment {
                    id: r.0,
                    repository: repository.into(),
                    workflow_version_id: r.1,
                    generation: r.2,
                    state: r.3,
                    params: serde_json::from_str(&r.4)?,
                    source_binding: r.5.map(|v| serde_json::from_str(&v)).transpose()?,
                    pipeline_rule: r.6.map(|v| serde_json::from_str(&v)).transpose()?,
                    diagnostics: serde_json::from_str(&r.7)?,
                })
            })
            .collect()
    }

    pub fn put_attachment(
        &mut self,
        repository: &str,
        id: &str,
        request: &PutAttachment,
    ) -> Result<Attachment> {
        validate_repository(repository)?;
        validate_id(id)?;
        validate_key(&request.idempotency_key)?;
        let request_digest = digest(&serde_json::to_vec(&(repository, id, request))?);
        let scope = format!("attachment:{repository}:{id}");
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(result_id) =
            idempotent_in(&tx, &scope, &request.idempotency_key, &request_digest)?
        {
            tx.commit()?;
            let attachment = self.attachment(repository, &result_id)?;
            let expected_generation = match request.expected_generation {
                Some(value) => value
                    .checked_add(1)
                    .ok_or_else(|| Error::Limit("attachment generation overflow".into()))?,
                None => 1,
            };
            if attachment.generation != expected_generation
                || attachment.workflow_version_id != request.workflow_version_id
                || attachment.params != request.params
                || attachment.state != "draft"
                || attachment.source_binding.is_some()
                || attachment.pipeline_rule.is_some()
                || !attachment.diagnostics.is_empty()
            {
                return Err(Error::Conflict(
                    "idempotent attachment result changed after the original operation".into(),
                ));
            }
            return Ok(attachment);
        }
        if tx
            .query_row(
                "SELECT 1 FROM workflow_versions WHERE id=?1",
                [&request.workflow_version_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_none()
        {
            return Err(Error::NotFound("workflow version".into()));
        }
        let existing: Option<u64> = tx
            .query_row(
                "SELECT generation FROM workflow_attachments WHERE repository=?1 AND id=?2",
                params![repository, id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            None if request.expected_generation.is_none() => {
                tx.execute(
                    "INSERT INTO workflow_attachments VALUES(?1,?2,?3,1,'draft',?4,NULL,NULL,'[]')",
                    params![
                        id,
                        repository,
                        request.workflow_version_id,
                        serde_json::to_string(&request.params)?
                    ],
                )?;
            }
            Some(current) if request.expected_generation == Some(current) => {
                if tx.execute("UPDATE workflow_attachments SET workflow_version_id=?1,generation=generation+1,state='draft',params_json=?2,source_binding_json=NULL,pipeline_rule_json=NULL,diagnostics_json='[]' WHERE id=?3 AND repository=?4 AND generation=?5",params![request.workflow_version_id,serde_json::to_string(&request.params)?,id,repository,current])? != 1 {return Err(Error::Conflict("attachment generation conflict".into()));}
            }
            _ => return Err(Error::Conflict("attachment generation conflict".into())),
        }
        record_idempotency_in(&tx, &scope, &request.idempotency_key, &request_digest, id)?;
        tx.commit()?;
        self.attachment(repository, id)
    }

    /// Validation is deliberately an export boundary. Exact native compilation still belongs to
    /// the existing compiler after the returned files are reviewed and committed.
    pub fn validate_attachment(
        &mut self,
        repository: &str,
        id: &str,
        request: &ValidateAttachment,
    ) -> Result<Value> {
        let current = self.attachment(repository, id)?;
        if current.generation != request.expected_generation {
            return Err(Error::Conflict("attachment generation conflict".into()));
        }
        validate_revision(&request.source_revision)?;
        let version = self.version(&current.workflow_version_id)?;
        let setup = self.export_setup(repository, id, &version, &request.source_revision)?;
        let runnable = setup["runnable"].as_bool() == Some(true);
        let warnings = setup["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|message| Diagnostic {
                level: "warning".into(),
                code: "archived_workflow_reference".into(),
                message: message.into(),
                path: None,
            })
            .collect::<Vec<_>>();
        let (state, mut diagnostics, next_actions) = if runnable {
            (
                "export_required",
                vec![Diagnostic{level:"guidance".into(),code:"commit_required".into(),message:"Review and commit the exported immutable skill resources, then fill the operator-owned manifest bindings and run the existing pipeline install command.".into(),path:None}],
                vec!["Write every exported file at its exact relative path and commit it.","Replace the manifest operator bindings with reviewed local values.","Set source_commit to the resulting full commit and run `agentscm pipeline install <manifest.json>`.","Inspect with `agentscm pipeline status`, preview a synthetic event, then enable with the returned generation."],
            )
        } else {
            let diagnostics = setup["unmet_bindings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|message| Diagnostic {
                    level: "error".into(),
                    code: "unmet_runtime_binding".into(),
                    message: message.into(),
                    path: None,
                })
                .collect::<Vec<_>>();
            ("blocked", diagnostics, Vec::new())
        };
        diagnostics.extend(warnings);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let retained:Option<(u64,String)>=tx.query_row("SELECT generation,workflow_version_id FROM workflow_attachments WHERE repository=?1 AND id=?2",params![repository,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if retained != Some((current.generation, current.workflow_version_id.clone())) {
            return Err(Error::Conflict("attachment generation conflict".into()));
        }
        if tx.execute("UPDATE workflow_attachments SET generation=generation+1,state=?1,source_binding_json=?2,diagnostics_json=?3 WHERE id=?4 AND repository=?5 AND generation=?6",params![state,serde_json::to_string(&json!({"asserted_source_revision":request.source_revision,"verified":false}))?,serde_json::to_string(&diagnostics)?,id,repository,current.generation])? != 1 {return Err(Error::Conflict("attachment generation conflict".into()));}
        tx.commit()?;
        Ok(
            json!({"attachment":self.attachment(repository,id)?,"state":state,"setup":setup,"next_actions":next_actions}),
        )
    }

    pub fn guidance(&self, repository: &str, root: &Path) -> Result<Value> {
        validate_repository(repository)?;
        let root = fs::canonicalize(root)?;
        let candidates = [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            "Makefile",
            ".github/workflows/ci.yml",
            ".github/workflows/ci.yaml",
        ];
        let mut observed = Vec::new();
        let mut suggestions = Vec::new();
        for relative in candidates {
            let path = root.join(relative);
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || meta.len() > MAX_FILE_BYTES as u64
            {
                continue;
            }
            let bytes = fs::read(&path)?;
            observed.push(json!({"path":relative,"sha256":digest(&bytes)}));
            match relative {
                "Cargo.toml" => {
                    for (kind, argv) in [
                        (
                            "lint",
                            vec![
                                "cargo",
                                "clippy",
                                "--all-targets",
                                "--locked",
                                "--",
                                "-D",
                                "warnings",
                            ],
                        ),
                        ("build", vec!["cargo", "build", "--locked"]),
                        ("test", vec!["cargo", "test", "--locked"]),
                    ] {
                        suggestions.push(json!({"kind":kind,"argv":argv,"reason":"Rust manifest observed; review repository instructions before adopting.","sources":[relative]}));
                    }
                }
                "package.json" => {
                    suggestions.push(json!({"kind":"test","argv":["npm","test"],"reason":"Node package manifest observed; scripts and package manager policy were not executed or inferred as safe.","sources":[relative]}));
                }
                "pyproject.toml" => {
                    suggestions.push(json!({"kind":"test","argv":["python","-m","pytest"],"reason":"Python project metadata observed; environment and test dependency availability remain unverified.","sources":[relative]}));
                }
                "go.mod" => {
                    suggestions.push(json!({"kind":"test","argv":["go","test","./..."],"reason":"Go module observed.","sources":[relative]}));
                }
                _ => {}
            }
        }
        Ok(
            json!({"observed_files":observed,"suggestions":suggestions,"uncovered":["Existing CI conditions, services, secrets, matrices and platform coverage were not reproduced."],"diagnostics":[]}),
        )
    }

    fn lint(&self, draft: &WorkflowDraft) -> Result<Vec<Diagnostic>> {
        let mut diagnostics = Vec::new();
        let mut composition = Composition::default();
        let mut stack = BTreeSet::new();
        if let Err(error) = self.compose_definition(
            "draft",
            draft,
            CompositionScope {
                prefix: String::new(),
                external_needs: Vec::new(),
                ancestor_required: true,
            },
            &mut stack,
            &mut composition,
        ) {
            diagnostics.push(Diagnostic {
                level: "error".into(),
                code: "invalid_composition".into(),
                message: error.to_string(),
                path: None,
            });
        }
        if diagnostics.is_empty() && !composition.steps.is_empty() {
            let root = ".agentscm/workflows/preview/v1";
            match self.composition_files(
                root,
                &composition.skill_ids,
                &composition.generated_actions,
                "# Workflow preview\n",
            ) {
                Ok(files) => {
                    let recipe = recipe_for(root, &draft.id, composition.steps.clone(), &files);
                    if let Some(message) = composed_bundle_budget(&files, root) {
                        composition.unmet.push(message);
                    } else if let Err(error) = validate_composed_bundle(&files, root, &recipe) {
                        diagnostics.push(Diagnostic {
                            level: "error".into(),
                            code: "invalid_composition".into(),
                            message: error.to_string(),
                            path: None,
                        });
                    }
                }
                Err(error) => diagnostics.push(Diagnostic {
                    level: "error".into(),
                    code: "invalid_composition".into(),
                    message: error.to_string(),
                    path: None,
                }),
            }
        }
        diagnostics.extend(composition.unmet.into_iter().map(|message| Diagnostic {
            level: "error".into(),
            code: "unmet_runtime_binding".into(),
            message,
            path: None,
        }));
        diagnostics.extend(composition.warnings.into_iter().map(|message| Diagnostic {
            level: "warning".into(),
            code: "archived_workflow_reference".into(),
            message,
            path: None,
        }));
        Ok(diagnostics)
    }

    fn compose_definition(
        &self,
        identity: &str,
        draft: &WorkflowDraft,
        scope: CompositionScope,
        stack: &mut BTreeSet<String>,
        composition: &mut Composition,
    ) -> Result<Vec<String>> {
        let CompositionScope {
            prefix,
            external_needs,
            ancestor_required,
        } = scope;
        validate_steps(&draft.steps)?;
        validate_follow_on(draft.follow_on.as_ref())?;
        if !stack.insert(identity.into()) {
            return Err(Error::Invalid("workflow version reference cycle".into()));
        }
        composition.workflows = composition
            .workflows
            .checked_add(1)
            .ok_or_else(|| Error::Limit("composed workflow count overflow".into()))?;
        if composition.workflows > MAX_COMPOSED_WORKFLOWS {
            return Err(Error::Limit(format!(
                "composition exceeds {MAX_COMPOSED_WORKFLOWS} workflow versions"
            )));
        }
        let ordered = topological_steps(&draft.steps)?;
        let local_action_ids = draft
            .steps
            .iter()
            .filter(|step| matches!(&step.kind, WorkflowStepKind::Action { .. }))
            .map(|step| (step.id.clone(), format!("{prefix}{}", step.id)))
            .collect::<BTreeMap<_, _>>();
        let mut outputs = BTreeMap::<String, Vec<String>>::new();
        for step in ordered {
            let dependencies = if step.needs.is_empty() {
                external_needs.clone()
            } else {
                step.needs
                    .iter()
                    .flat_map(|need| outputs.get(need).cloned().unwrap_or_default())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            let generated_id = format!("{prefix}{}", step.id);
            let leaves = match &step.kind {
                WorkflowStepKind::Action {
                    skill_version_id,
                    resource,
                } => {
                    composition.skill_ids.insert(skill_version_id.clone());
                    if generated_id.len() > 64 {
                        return Err(Error::Limit(format!(
                            "flattened step {identity}:{} exceeds 64 bytes as {generated_id}",
                            step.id
                        )));
                    }
                    let origin = format!("{identity}:{}", step.id);
                    if let Some(previous) = composition
                        .step_origins
                        .insert(generated_id.clone(), origin.clone())
                    {
                        return Err(Error::Invalid(format!(
                            "flattened step id {generated_id} collides between {previous} and {origin}"
                        )));
                    }
                    let (resource, generated_action) = self.composed_action(
                        identity,
                        &step.id,
                        skill_version_id,
                        resource,
                        &generated_id,
                        &local_action_ids,
                    )?;
                    composition.generated_actions.push(generated_action);
                    composition.steps.push(StepDefinition {
                        id: generated_id.clone(),
                        needs: dependencies,
                        required: ancestor_required && step.required,
                        action: StepAction::AgentAction { resource },
                    });
                    if composition.steps.len() > MAX_STEPS {
                        return Err(Error::Limit(format!(
                            "composed workflow exceeds {MAX_STEPS} steps"
                        )));
                    }
                    vec![generated_id]
                }
                WorkflowStepKind::Workflow {
                    workflow_version_id,
                    inputs,
                } => {
                    validate_id(workflow_version_id)?;
                    if !inputs.is_empty() {
                        composition.unmet.push(format!(
                            "workflow step {} has inputs, which the existing runtime cannot bind",
                            step.id
                        ));
                    }
                    let version = self.version(workflow_version_id)?;
                    self.note_archived_owner(&version, &mut composition.warnings)?;
                    self.compose_definition(
                        &version.id,
                        &version.definition,
                        CompositionScope {
                            prefix: format!("{generated_id}__"),
                            external_needs: dependencies,
                            ancestor_required: ancestor_required && step.required,
                        },
                        stack,
                        composition,
                    )?
                }
                WorkflowStepKind::Skill { skill_version_id } => {
                    self.skill(skill_version_id)?;
                    composition.skill_ids.insert(skill_version_id.clone());
                    composition.unmet.push(format!(
                        "step {} is guidance-only and has no existing runtime action binding",
                        step.id
                    ));
                    dependencies
                }
                WorkflowStepKind::Build {
                    skill_version_id, ..
                } => {
                    self.skill(skill_version_id)?;
                    composition.skill_ids.insert(skill_version_id.clone());
                    composition.unmet.push(format!(
                        "step {} needs an existing reviewed command context and recipe binding",
                        step.id
                    ));
                    dependencies
                }
                WorkflowStepKind::Deploy {
                    skill_version_id, ..
                } => {
                    self.skill(skill_version_id)?;
                    composition.skill_ids.insert(skill_version_id.clone());
                    composition.unmet.push(format!("step {} has no deployment executor; the existing runtime can only create productive artifacts",step.id));
                    dependencies
                }
            };
            outputs.insert(step.id.clone(), leaves);
        }
        let depended: BTreeSet<_> = draft
            .steps
            .iter()
            .flat_map(|step| step.needs.iter().cloned())
            .collect();
        let mut leaves = draft
            .steps
            .iter()
            .filter(|step| !depended.contains(&step.id))
            .flat_map(|step| outputs.get(&step.id).cloned().unwrap_or_default())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(follow) = &draft.follow_on {
            if !follow.inputs.is_empty() {
                composition.unmet.push(
                    "follow_on inputs are retained but the existing runtime cannot bind them"
                        .into(),
                );
            }
            match follow.when {
                FollowWhen::Success => {
                    let version = self.version(&follow.workflow_version_id)?;
                    self.note_archived_owner(&version, &mut composition.warnings)?;
                    leaves = self.compose_definition(
                        &version.id,
                        &version.definition,
                        CompositionScope {
                            prefix: format!("{prefix}follow__"),
                            external_needs: leaves,
                            ancestor_required,
                        },
                        stack,
                        composition,
                    )?;
                }
                FollowWhen::Failure | FollowWhen::Always => composition.unmet.push(
                    "the existing runtime supports only success follow_on composition".into(),
                ),
            }
        }
        stack.remove(identity);
        Ok(leaves)
    }

    fn composed_action(
        &self,
        workflow_identity: &str,
        step_id: &str,
        skill_version_id: &str,
        resource: &str,
        generated_id: &str,
        local_action_ids: &BTreeMap<String, String>,
    ) -> Result<(String, PackageFile)> {
        let skill = self.skill(skill_version_id)?;
        let root = skill
            .entrypoint
            .rsplit_once('/')
            .map_or("", |(root, _)| root);
        let source = if root.is_empty() {
            resource.to_owned()
        } else {
            format!("{root}/{resource}")
        };
        let package = self.package(skill_version_id)?;
        let action = package
            .iter()
            .find(|file| file.path == source)
            .ok_or_else(|| Error::NotFound(format!("skill resource {resource}")))?;
        if action.content.len() > 64 * 1024 {
            return Err(Error::Limit("action declaration exceeds 64 KiB".into()));
        }
        let mut definition: ActionDefinition = serde_json::from_str(&action.content)?;
        definition.validate()?;
        for input in definition.inputs.values_mut() {
            if let ActionInput::Artifact { step: producer, .. } = input {
                let original = producer.clone();
                *producer = local_action_ids.get(&original).cloned().ok_or_else(|| {
                    Error::Invalid(format!(
                        "action {workflow_identity}:{step_id} artifact producer {original} is not a productive action in the same workflow"
                    ))
                })?;
            }
        }
        if let ProductiveAction::Prompt { prompt, .. } = &mut definition.action {
            let source = if root.is_empty() {
                prompt.clone()
            } else {
                format!("{root}/{prompt}")
            };
            if !package.iter().any(|file| file.path == source) {
                return Err(Error::NotFound(format!(
                    "action {workflow_identity}:{step_id} prompt {prompt} is absent from skill {skill_version_id}"
                )));
            }
            *prompt = format!("skills/{skill_version_id}/{prompt}");
        }
        let generated_path = format!("steps/{generated_id}.json");
        Ok((
            generated_path.clone(),
            PackageFile {
                path: generated_path,
                content: serde_json::to_string(&definition)?,
            },
        ))
    }

    fn note_archived_owner(
        &self,
        version: &WorkflowVersion,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let owner = self.workflow(&version.workflow_id)?;
        if owner.archived {
            let warning = format!(
                "workflow version {} belongs to archived workflow {}",
                version.id, version.workflow_id
            );
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        Ok(())
    }

    fn composition_files(
        &self,
        root: &str,
        skill_ids: &BTreeSet<String>,
        generated_actions: &[PackageFile],
        skill_markdown: &str,
    ) -> Result<Vec<PackageFile>> {
        let mut files = vec![PackageFile {
            path: format!("{root}/SKILL.md"),
            content: skill_markdown.into(),
        }];
        for id in skill_ids {
            let skill = self.skill(id)?;
            let package_root = skill
                .entrypoint
                .rsplit_once('/')
                .map_or("", |(root, _)| root);
            for file in self.package(id)? {
                let relative = if file.path == skill.entrypoint {
                    "SKILL.md".into()
                } else if package_root.is_empty() {
                    file.path
                } else {
                    file.path
                        .strip_prefix(package_root)
                        .and_then(|path| path.strip_prefix('/'))
                        .ok_or_else(|| {
                            Error::Invalid(
                                "skill package resource is outside its entrypoint directory".into(),
                            )
                        })?
                        .to_owned()
                };
                files.push(PackageFile {
                    path: format!("{root}/skills/{}/{relative}", skill.id),
                    content: file.content,
                });
            }
        }
        files.extend(generated_actions.iter().map(|file| PackageFile {
            path: format!("{root}/{}", file.path),
            content: file.content.clone(),
        }));
        Ok(files)
    }

    fn export_files(
        &self,
        version: &WorkflowVersion,
        skill_ids: &BTreeSet<String>,
        generated_actions: &[PackageFile],
    ) -> Result<Vec<PackageFile>> {
        let root = format!(
            ".agentscm/workflows/{}/v{}",
            version.workflow_id, version.version
        );
        let mut files = self.composition_files(
            &root,
            skill_ids,
            generated_actions,
            &format!(
                "# {}\n\nImmutable workflow package {}. Review every selected resource before installation.\n",
                version.definition.name, version.digest
            ),
        )?;
        files.push(PackageFile {
            path: format!(
                ".agentscm/workflows/{}/v{}/workflow.json",
                version.workflow_id, version.version
            ),
            content: serde_json::to_string_pretty(&version.definition)?,
        });
        Ok(files)
    }

    fn export_setup(
        &self,
        repository: &str,
        attachment_id: &str,
        version: &WorkflowVersion,
        _asserted_source_revision: &str,
    ) -> Result<Value> {
        let mut composition = Composition::default();
        let mut stack = BTreeSet::new();
        self.note_archived_owner(version, &mut composition.warnings)?;
        self.compose_definition(
            &version.id,
            &version.definition,
            CompositionScope {
                prefix: String::new(),
                external_needs: Vec::new(),
                ancestor_required: true,
            },
            &mut stack,
            &mut composition,
        )?;
        let files = self.export_files(
            version,
            &composition.skill_ids,
            &composition.generated_actions,
        )?;
        let root = format!(
            ".agentscm/workflows/{}/v{}",
            version.workflow_id, version.version
        );
        let mut unmet = composition.unmet;
        let over_budget = if let Some(message) = composed_bundle_budget(&files, &root) {
            unmet.push(message);
            true
        } else {
            false
        };
        let trigger = match &version.definition.trigger {
            Some(Trigger::ConfirmedMerge { target, labels_all }) => Some(MergeTrigger {
                target: target.clone(),
                labels_all: labels_all.clone(),
            }),
            None => {
                unmet.push("confirmed_merge trigger is required for pipeline installation".into());
                None
            }
        };
        let recipe = recipe_for(&root, &version.workflow_id, composition.steps, &files);
        if !over_budget && let Err(error) = validate_composed_bundle(&files, &root, &recipe) {
            unmet.push(error.to_string());
        }
        if unmet.is_empty() {
            recipe.validate_structure(1)?;
        }
        let manifest = (unmet.is_empty())
            .then_some(trigger.as_ref())
            .flatten()
            .map(|trigger| {
                json!({
                    "repository": repository,
                    "provider": "REPLACE_WITH_github_OR_gitlab",
                    "host": "REPLACE_WITH_FORGE_HOST",
                    "provider_repository_id": 1,
                    "rule_id": attachment_id,
                    "enabled": false,
                    "trigger": trigger,
                    "recipe": recipe,
                    "profile_path": "/absolute/operator/action-profile.json",
                    "git": "/usr/bin/git",
                    "git_repository": "/absolute/repository/checkout",
                    "source_commit": "REPLACE_WITH_POST_EXPORT_COMMIT"
                })
            });
        Ok(json!({
            "runnable": unmet.is_empty(),
            "files": files,
            "pipeline_recipe": recipe,
            "pipeline_install_template": manifest,
            "library_follow_on": version.definition.follow_on,
            "source_binding": {"asserted": true, "verified": false},
            "unmet_bindings": unmet,
            "warnings": composition.warnings,
            "activation": "operator_local_cli"
        }))
    }

    fn skill(&self, id: &str) -> Result<SkillVersion> {
        self.skills()?
            .into_iter()
            .find(|v| v.id == id)
            .ok_or_else(|| Error::NotFound("skill version".into()))
    }
    fn package(&self, id: &str) -> Result<Vec<PackageFile>> {
        validate_id(id)?;
        let package: Option<String> = self
            .db
            .query_row(
                "SELECT package_json FROM skill_versions WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        package
            .map(|value| serde_json::from_str(&value).map_err(Error::from))
            .transpose()?
            .ok_or_else(|| Error::NotFound("skill version".into()))
    }
    fn workflow(&self, id: &str) -> Result<WorkflowDraft> {
        workflow_from(&self.db, id)
    }
    fn versions(&self, workflow_id: &str) -> Result<Vec<WorkflowVersion>> {
        let mut s = self.db.prepare(
            "SELECT id FROM workflow_versions WHERE workflow_id=?1 ORDER BY version DESC",
        )?;
        let ids = s
            .query_map([workflow_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.version(&id)).collect()
    }
    fn version(&self, id: &str) -> Result<WorkflowVersion> {
        validate_id(id)?;
        let r:Option<(String,u32,String,String,String,i64,bool)>=self.db.query_row("SELECT workflow_id,version,digest,definition_json,skill_versions_json,created_at,archived FROM workflow_versions WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let Some(r) = r else {
            return Err(Error::NotFound("workflow version".into()));
        };
        Ok(WorkflowVersion {
            id: id.into(),
            workflow_id: r.0,
            version: r.1,
            digest: r.2,
            definition: serde_json::from_str(&r.3)?,
            skill_versions: serde_json::from_str(&r.4)?,
            created_at: r.5,
            archived: r.6,
        })
    }
    fn attachment(&self, repository: &str, id: &str) -> Result<Attachment> {
        self.attachments(repository)?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| Error::NotFound("workflow attachment".into()))
    }
}

fn workflow_from(db: &Connection, id: &str) -> Result<WorkflowDraft> {
    validate_id(id)?;
    let row:Option<WorkflowRow>=db.query_row("SELECT name,summary,generation,steps_json,trigger_json,follow_on_json,updated_at,archived FROM workflow_drafts WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
    let Some(row) = row else {
        return Err(Error::NotFound("workflow draft".into()));
    };
    Ok(WorkflowDraft {
        id: id.into(),
        name: row.0,
        summary: row.1,
        generation: row.2,
        steps: serde_json::from_str(&row.3)?,
        trigger: row
            .4
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        follow_on: row
            .5
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        updated_at: row.6,
        archived: row.7,
    })
}

fn idempotent_in(
    db: &Connection,
    scope: &str,
    key: &str,
    request_digest: &str,
) -> Result<Option<String>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT request_digest,result_id FROM idempotency WHERE scope=?1 AND key=?2",
            params![scope, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((same, id)) if same == request_digest => Ok(Some(id)),
        Some(_) => Err(Error::Conflict(
            "idempotency key was already used with different input".into(),
        )),
        None => Ok(None),
    }
}

fn record_idempotency_in(
    db: &Connection,
    scope: &str,
    key: &str,
    request_digest: &str,
    result_id: &str,
) -> Result<()> {
    db.execute(
        "INSERT INTO idempotency VALUES(?1,?2,?3,?4)",
        params![scope, key, request_digest, result_id],
    )?;
    Ok(())
}

fn topological_steps(steps: &[WorkflowStep]) -> Result<Vec<&WorkflowStep>> {
    let indexes = steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut indegree = steps
        .iter()
        .map(|step| step.needs.len())
        .collect::<Vec<_>>();
    let mut dependents = vec![Vec::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        for dependency in &step.needs {
            let dependency = *indexes
                .get(dependency.as_str())
                .ok_or_else(|| Error::Invalid("workflow dependency is missing".into()))?;
            dependents[dependency].push(index);
        }
    }
    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(steps.len());
    while let Some(index) = ready.pop_first() {
        ordered.push(&steps[index]);
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    if ordered.len() != steps.len() {
        return Err(Error::Invalid("workflow dependency cycle".into()));
    }
    Ok(ordered)
}

fn recipe_for(
    root: &str,
    name: &str,
    steps: Vec<StepDefinition>,
    files: &[PackageFile],
) -> PipelineRecipe {
    let entrypoint = format!("{root}/SKILL.md");
    let metadata = format!("{root}/workflow.json");
    let mut resources = files
        .iter()
        .map(|file| file.path.clone())
        .filter(|path| path != &entrypoint && path != &metadata)
        .collect::<Vec<_>>();
    resources.sort();
    PipelineRecipe {
        version: 1,
        name: name.into(),
        skill: SkillReference {
            entrypoint,
            resources,
        },
        steps,
    }
}

fn composed_bundle_budget(files: &[PackageFile], root: &str) -> Option<String> {
    let prefix = format!("{root}/");
    let resources = files
        .iter()
        .filter_map(|file| {
            file.path
                .strip_prefix(&prefix)
                .filter(|relative| *relative != "workflow.json")
                .map(|relative| (relative, file.content.len()))
        })
        .collect::<Vec<_>>();
    if resources.len() > MAX_SKILL_RESOURCES {
        return Some(format!(
            "composed bundle has {} resources; the native limit is {MAX_SKILL_RESOURCES}",
            resources.len()
        ));
    }
    let bytes = resources.iter().try_fold(0usize, |total, (path, bytes)| {
        total.checked_add(path.len())?.checked_add(*bytes)
    });
    match bytes {
        None => Some("composed bundle byte count overflow".into()),
        Some(bytes) if bytes > MAX_SKILL_RESOURCE_BYTES => Some(format!(
            "composed bundle has {bytes} resource bytes including paths; the native limit is {MAX_SKILL_RESOURCE_BYTES}"
        )),
        _ => None,
    }
}

fn validate_composed_bundle(
    files: &[PackageFile],
    root: &str,
    recipe: &PipelineRecipe,
) -> Result<()> {
    recipe.validate_structure(1)?;
    let prefix = format!("{root}/");
    let resources = files
        .iter()
        .filter_map(|file| {
            file.path
                .strip_prefix(&prefix)
                .filter(|relative| *relative != "workflow.json")
                .map(|relative| SkillResource::new(relative, file.content.as_bytes()))
        })
        .collect::<Vec<_>>();
    let bundle = SkillBundle::new(resources)?;
    let mut actions = BTreeMap::<String, ActionDefinition>::new();
    for step in &recipe.steps {
        let StepAction::AgentAction { resource } = &step.action else {
            continue;
        };
        let declaration = bundle
            .resource(resource)
            .ok_or_else(|| Error::Invalid(format!("action resource is missing: {resource}")))?;
        if declaration.len() > 64 * 1024 {
            return Err(Error::Limit("action declaration exceeds 64 KiB".into()));
        }
        let definition: ActionDefinition = serde_json::from_slice(declaration)?;
        definition.validate()?;
        if definition
            .inputs
            .values()
            .any(|input| matches!(input, ActionInput::Source { .. }))
        {
            return Err(Error::Conflict(
                "action source inputs require validation after export against the resulting commit"
                    .into(),
            ));
        }
        if let ProductiveAction::Prompt { prompt, .. } = &definition.action {
            let prompt = bundle
                .resource(prompt)
                .ok_or_else(|| Error::Invalid("action prompt resource is missing".into()))?;
            let prompt = std::str::from_utf8(prompt)
                .map_err(|_| Error::Invalid("action prompt must be UTF-8".into()))?;
            if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
                return Err(Error::Limit(
                    "action prompt must contain 1..32768 bytes".into(),
                ));
            }
        }
        actions.insert(step.id.clone(), definition);
    }
    for step in &recipe.steps {
        let Some(action) = actions.get(&step.id) else {
            continue;
        };
        for input in action.inputs.values() {
            if let ActionInput::Artifact {
                step: producer,
                name,
            } = input
                && (!step.needs.contains(producer)
                    || actions
                        .get(producer)
                        .is_none_or(|action| action.output.name != *name))
            {
                return Err(Error::Invalid(
                    "artifact input must name an output of an explicit dependency".into(),
                ));
            }
        }
    }
    Ok(())
}

pub async fn preview(data_dir: PathBuf, request: ImportPreviewRequest) -> Result<ImportPreview> {
    let (skill, files, warnings) = tokio::time::timeout(IMPORT_DEADLINE, resolve(request.source))
        .await
        .map_err(|_| {
            Error::Limit("skill import exceeded the aggregate 20 second deadline".into())
        })??;
    tokio::task::spawn_blocking(move || {
        let mut library = Library::open(&data_dir)?;
        library.save_preview(skill, &files, warnings)
    })
    .await
    .map_err(|_| Error::Io(std::io::Error::other("skill preview worker failed")))?
}

async fn resolve(source: ImportSource) -> Result<(PreviewSkill, Vec<PackageFile>, Vec<String>)> {
    match source {
        ImportSource::Markdown {
            name,
            markdown,
            resources,
        } => {
            let mut files = vec![PackageFile {
                path: "SKILL.md".into(),
                content: markdown,
            }];
            files.extend(resources);
            Ok((
                PreviewSkill {
                    name,
                    entrypoint: "SKILL.md".into(),
                    provenance: Provenance::Markdown {
                        locator: "pasted_markdown".into(),
                    },
                },
                files,
                vec![],
            ))
        }
        ImportSource::Github {
            repository,
            commit,
            path,
            resources,
        } => resolve_github(repository, commit, path, resources, None).await,
        ImportSource::NpxDescriptor {
            command,
            commit,
            path,
            resources,
        } => {
            let (repository, name) = parse_npx(&command)?;
            let entrypoint = path.unwrap_or_else(|| format!("skills/{name}/SKILL.md"));
            resolve_github(repository, commit, entrypoint, resources, Some(command)).await
        }
    }
}

async fn resolve_github(
    repository: String,
    commit: String,
    path: String,
    resources: Vec<String>,
    command: Option<String>,
) -> Result<(PreviewSkill, Vec<PackageFile>, Vec<String>)> {
    validate_repository(&repository)?;
    validate_commit(&commit)?;
    validate_relative(&path)?;
    if path.rsplit('/').next() != Some("SKILL.md") {
        return Err(Error::Invalid(
            "GitHub skill path must name SKILL.md".into(),
        ));
    }
    if resources.len() + 1 > MAX_FILES {
        return Err(Error::Limit(format!(
            "skill package exceeds {MAX_FILES} files"
        )));
    }
    let mut paths = vec![path.clone()];
    paths.extend(resources);
    let mut unique = BTreeSet::new();
    for p in &paths {
        validate_relative(p)?;
        if !unique.insert(p.clone()) {
            return Err(Error::Invalid("duplicate skill resource path".into()));
        }
    }
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| Error::Invalid("GitHub import client unavailable".into()))?;
    let mut files = Vec::new();
    let mut total = 0usize;
    for remote in &paths {
        let url = raw_github_url(&repository, &commit, remote)?;
        let mut response = client
            .get(url)
            .header("accept", "text/plain")
            .send()
            .await
            .map_err(|_| Error::Conflict("pinned GitHub skill could not be read".into()))?;
        if !response.status().is_success() {
            return Err(Error::Conflict(format!(
                "pinned GitHub skill resource is unavailable: {remote}"
            )));
        }
        let declared = response
            .content_length()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        if declared > MAX_FILE_BYTES {
            return Err(Error::Limit(format!(
                "skill resource exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        let mut bytes = Vec::with_capacity(declared.min(MAX_FILE_BYTES));
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Conflict("pinned GitHub skill response could not be read".into()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_FILE_BYTES {
                return Err(Error::Limit(format!(
                    "skill resource exceeds {MAX_FILE_BYTES} bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| Error::Limit("skill package size overflow".into()))?;
        if total > MAX_PACKAGE_BYTES {
            return Err(Error::Limit(format!(
                "skill package exceeds {MAX_PACKAGE_BYTES} bytes"
            )));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| Error::Invalid("skill resources must be UTF-8 text".into()))?;
        files.push(PackageFile {
            path: remote.clone(),
            content,
        });
    }
    let name = path.split('/').rev().nth(1).unwrap_or("skill").to_owned();
    let provenance = match command {
        Some(command) => Provenance::NpxDescriptor {
            locator: repository.clone(),
            commit: commit.clone(),
            path: path.clone(),
            command,
        },
        None => Provenance::Github {
            locator: repository.clone(),
            commit: commit.clone(),
            path: path.clone(),
        },
    };
    Ok((PreviewSkill{name,entrypoint:path,provenance},files,vec!["Imported instructions are untrusted data until reviewed; no package or script was executed.".into()]))
}

fn parse_npx(command: &str) -> Result<(String, String)> {
    if command.chars().any(|c| {
        matches!(
            c,
            '\n' | '\r' | '\0' | '\'' | '"' | '`' | '$' | ';' | '|' | '&' | '>' | '<'
        )
    }) {
        return Err(Error::Invalid(
            "npx descriptor contains unsupported shell syntax".into(),
        ));
    }
    let words: Vec<&str> = command.split_ascii_whitespace().collect();
    let body = match words.as_slice() {
        ["npx", "skills", "add", repository, "--skill", name] => Some((*repository, *name)),
        ["npx", "-y", "skills", "add", repository, "--skill", name] => Some((*repository, *name)),
        _ => None,
    };
    let Some((repository, name)) = body else {
        return Err(Error::Invalid(
            "expected `npx skills add owner/repo --skill NAME`".into(),
        ));
    };
    validate_repository(repository)?;
    validate_name(name)?;
    Ok((repository.into(), name.into()))
}

fn validate_package(files: &[PackageFile], entrypoint: &str) -> Result<()> {
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(Error::Limit(format!(
            "skill package requires 1 to {MAX_FILES} files"
        )));
    }
    validate_relative(entrypoint)?;
    if entrypoint.rsplit('/').next() != Some("SKILL.md") {
        return Err(Error::Invalid(
            "skill entrypoint must end in exact SKILL.md".into(),
        ));
    }
    let package_root = entrypoint.rsplit_once('/').map_or("", |(root, _)| root);
    let mut total = 0usize;
    let mut names = BTreeSet::new();
    for file in files {
        validate_relative(&file.path)?;
        let relative = if package_root.is_empty() {
            file.path.as_str()
        } else {
            file.path
                .strip_prefix(package_root)
                .and_then(|path| path.strip_prefix('/'))
                .ok_or_else(|| {
                    Error::Invalid(
                        "skill package resource is outside its entrypoint directory".into(),
                    )
                })?
        };
        if relative.eq_ignore_ascii_case("skill.json")
            || relative.eq_ignore_ascii_case("workflow.json")
        {
            return Err(Error::Invalid(
                "skill package uses a reserved generated filename".into(),
            ));
        }
        if !names.insert(file.path.to_ascii_lowercase()) {
            return Err(Error::Invalid(
                "duplicate or case-colliding skill path".into(),
            ));
        }
        if file.content.len() > MAX_FILE_BYTES {
            return Err(Error::Limit(format!(
                "skill resource exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        total = total
            .checked_add(file.content.len())
            .ok_or_else(|| Error::Limit("skill package size overflow".into()))?;
    }
    if total > MAX_PACKAGE_BYTES {
        return Err(Error::Limit(format!(
            "skill package exceeds {MAX_PACKAGE_BYTES} bytes"
        )));
    }
    if !files.iter().any(|f| f.path == entrypoint) {
        return Err(Error::Invalid(
            "skill entrypoint is absent from package".into(),
        ));
    }
    Ok(())
}
fn summarize(files: &[PackageFile]) -> Vec<FileSummary> {
    files
        .iter()
        .map(|f| FileSummary {
            path: f.path.clone(),
            sha256: digest(f.content.as_bytes()),
            size: f.content.len(),
        })
        .collect()
}
fn skill_refs(steps: &[WorkflowStep]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for step in steps {
        match &step.kind {
            WorkflowStepKind::Skill { skill_version_id }
            | WorkflowStepKind::Build {
                skill_version_id, ..
            }
            | WorkflowStepKind::Deploy {
                skill_version_id, ..
            }
            | WorkflowStepKind::Action {
                skill_version_id, ..
            } => {
                ids.insert(skill_version_id.clone());
            }
            WorkflowStepKind::Workflow { .. } => {}
        }
    }
    ids.into_iter().collect()
}
fn validate_steps(steps: &[WorkflowStep]) -> Result<()> {
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(Error::Limit(format!(
            "workflow requires 1 to {MAX_STEPS} steps"
        )));
    }
    let ids: BTreeSet<_> = steps.iter().map(|s| s.id.as_str()).collect();
    if ids.len() != steps.len() {
        return Err(Error::Invalid("workflow step ids must be unique".into()));
    }
    for step in steps {
        validate_id(&step.id)?;
        validate_text(&step.label, 200, "step label")?;
        if step
            .needs
            .iter()
            .any(|n| !ids.contains(n.as_str()) || n == &step.id)
        {
            return Err(Error::Invalid(
                "workflow dependency is missing or self-referential".into(),
            ));
        }
        match &step.kind {
            WorkflowStepKind::Build { runner, .. } => validate_relative(runner)?,
            WorkflowStepKind::Deploy {
                resource, target, ..
            } => {
                validate_relative(resource)?;
                validate_text(target, 200, "deploy target")?
            }
            WorkflowStepKind::Action { resource, .. } => validate_relative(resource)?,
            WorkflowStepKind::Skill { .. } | WorkflowStepKind::Workflow { .. } => {}
        }
    }
    fn visit<'a>(
        id: &'a str,
        map: &BTreeMap<&'a str, &'a WorkflowStep>,
        visiting: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Result<()> {
        if done.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(Error::Invalid("workflow dependency cycle".into()));
        }
        for need in &map[id].needs {
            visit(need, map, visiting, done)?
        }
        visiting.remove(id);
        done.insert(id);
        Ok(())
    }
    let map: BTreeMap<_, _> = steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for id in ids {
        visit(id, &map, &mut visiting, &mut done)?
    }
    Ok(())
}
fn validate_trigger(trigger: Option<&Trigger>) -> Result<()> {
    if let Some(Trigger::ConfirmedMerge { target, labels_all }) = trigger {
        validate_text(target, 200, "trigger target")?;
        if labels_all.len() > 64 || labels_all.iter().any(|l| l.is_empty() || l.len() > 256) {
            return Err(Error::Limit("trigger labels exceed bounds".into()));
        }
    }
    Ok(())
}
fn validate_follow_on(follow: Option<&FollowOn>) -> Result<()> {
    if let Some(follow) = follow {
        validate_id(&follow.workflow_version_id)?;
        if follow.inputs.len() > 32
            || follow
                .inputs
                .iter()
                .map(|(key, value)| key.len().saturating_add(value.len()))
                .sum::<usize>()
                > 32 * 1024
        {
            return Err(Error::Limit("follow-on inputs exceed bounds".into()));
        }
        for (key, value) in &follow.inputs {
            validate_text(key, 100, "follow-on input name")?;
            if value.len() > 4096 {
                return Err(Error::Limit(
                    "follow-on input value exceeds 4096 bytes".into(),
                ));
            }
        }
    }
    Ok(())
}
fn validate_relative(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.contains(['\\', '?', '#', '%'])
        || !value.is_ascii()
        || Path::new(value).is_absolute()
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        || Path::new(value).components().any(|c| {
            !matches!(c, Component::Normal(_))
                || match c {
                    Component::Normal(s) => !s
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
                    _ => true,
                }
        })
    {
        return Err(Error::Invalid(
            "path must be a normalized relative path containing only safe ASCII segments".into(),
        ));
    }
    Ok(())
}
fn validate_name(value: &str) -> Result<()> {
    validate_text(value, 100, "name")?;
    if value.chars().any(char::is_control) {
        return Err(Error::Invalid("name contains control characters".into()));
    }
    Ok(())
}
fn validate_text(value: &str, max: usize, kind: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {max} bytes"
        )));
    }
    Ok(())
}
fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(Error::Invalid("invalid workflow library id".into()));
    }
    Ok(())
}
fn validate_key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(Error::Invalid("invalid idempotency key".into()));
    }
    Ok(())
}
/// The one host GitHub imports read from. The validated owner, name, commit and path go in as
/// separate path segments, so none of them can change the scheme, host or query.
fn raw_github_url(repository: &str, commit: &str, path: &str) -> Result<reqwest::Url> {
    validate_repository(repository)?;
    validate_commit(commit)?;
    validate_relative(path)?;
    let mut url = reqwest::Url::parse("https://raw.githubusercontent.com/")
        .map_err(|_| Error::Invalid("GitHub import URL unavailable".into()))?;
    url.path_segments_mut()
        .map_err(|()| Error::Invalid("GitHub import URL unavailable".into()))?
        .extend(repository.split('/'))
        .push(commit)
        .extend(path.split('/'));
    Ok(url)
}

fn validate_repository(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || p.len() > 100
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    {
        return Err(Error::Invalid("repository must be owner/name".into()));
    }
    Ok(())
}
fn validate_commit(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid(
            "GitHub import requires a full lowercase 40-hex commit".into(),
        ));
    }
    Ok(())
}
fn validate_revision(value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid(
            "source revision must be a lowercase full digest".into(),
        ));
    }
    Ok(())
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
fn new_id(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Invalid("random source unavailable".into()))?;
    Ok(format!(
        "{prefix}_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::authority::Authority;
    use crate::model::{EntryChange, EntryKind};
    use crate::skill_workflow::{WorkflowDefinition, WorkflowSource};

    #[test]
    fn raw_github_url_keeps_the_host_and_refuses_unsafe_parts() {
        let commit = "a".repeat(40);
        let url = raw_github_url("acme/widgets", &commit, "skills/x/SKILL.md").unwrap();
        assert_eq!(
            url.as_str(),
            format!("https://raw.githubusercontent.com/acme/widgets/{commit}/skills/x/SKILL.md")
        );
        for (repository, path) in [
            ("evil.example/x", "SKILL.md"),
            ("acme/widgets", "../SKILL.md"),
            ("acme/widgets", "a?b/SKILL.md"),
            ("acme/widgets@evil.example", "SKILL.md"),
        ] {
            if let Ok(url) = raw_github_url(repository, &commit, path) {
                assert_eq!(url.host_str(), Some("raw.githubusercontent.com"));
                assert!(url.query().is_none() && url.fragment().is_none());
            }
        }
        assert!(raw_github_url("acme/widgets", "main", "SKILL.md").is_err());
    }

    fn accept_package(
        library: &mut Library,
        name: &str,
        files: Vec<PackageFile>,
        key: &str,
    ) -> SkillVersion {
        let preview = library
            .save_preview(
                PreviewSkill {
                    name: name.into(),
                    entrypoint: "SKILL.md".into(),
                    provenance: Provenance::Markdown {
                        locator: "pasted_markdown".into(),
                    },
                },
                &files,
                vec![],
            )
            .unwrap();
        library
            .accept_import(&AcceptImport {
                preview_id: preview.preview_id,
                digest: preview.digest,
                idempotency_key: key.into(),
            })
            .unwrap()
    }

    fn compile_exported_setup(setup: &Value) -> crate::skill_workflow::CompiledPlan {
        let root = tempfile::tempdir().unwrap();
        let engine = crate::store::Engine::open(root.path()).unwrap();
        let authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let files: Vec<PackageFile> = serde_json::from_value(setup["files"].clone()).unwrap();
        let entries = files
            .iter()
            .map(|file| EntryChange {
                path: file.path.clone(),
                content: Some(file.content.as_bytes().to_vec()),
                kind: EntryKind::Regular,
            })
            .collect::<Vec<_>>();
        let revision = engine.checkpoint_entries(None, &entries).unwrap();
        let recipe: PipelineRecipe =
            serde_json::from_value(setup["pipeline_recipe"].clone()).unwrap();
        let definition = WorkflowDefinition {
            version: recipe.version,
            name: recipe.name,
            source: WorkflowSource {
                repository,
                revision,
                policy_epoch: 0,
            },
            skill: recipe.skill,
            steps: recipe.steps,
        };
        drop(authority);
        drop(engine);
        crate::skill_workflow::compile(root.path(), &definition).unwrap()
    }

    fn published(publication: Publication) -> (WorkflowVersion, Vec<Diagnostic>) {
        match publication {
            Publication::Published {
                version,
                diagnostics,
            } => (*version, diagnostics),
            Publication::Rejected { diagnostics } => {
                panic!("expected publication, got diagnostics: {diagnostics:?}")
            }
        }
    }

    #[test]
    fn npx_is_a_descriptor_not_shell_text() {
        assert_eq!(
            parse_npx("npx skills add owner/repo --skill review").unwrap(),
            ("owner/repo".into(), "review".into())
        );
        assert!(parse_npx("npx evil").is_err());
        assert!(parse_npx("npx skills add https://example.com/x --skill y").is_err());
        assert!(parse_npx("npx skills add owner/repo --skill x;touch").is_err());
        for path in [
            "../SKILL.md",
            "skill\\SKILL.md",
            "skill/SKILL.md?raw=1",
            "skill/%2e%2e/SKILL.md",
        ] {
            assert!(validate_relative(path).is_err(), "{path}");
        }
        for (entrypoint, files) in [
            (
                "SKILL.md",
                vec![
                    PackageFile {
                        path: "SKILL.md".into(),
                        content: "# Test".into(),
                    },
                    PackageFile {
                        path: "WORKFLOW.JSON".into(),
                        content: "{}".into(),
                    },
                ],
            ),
            (
                "skills/test/SKILL.md",
                vec![
                    PackageFile {
                        path: "skills/test/SKILL.md".into(),
                        content: "# Test".into(),
                    },
                    PackageFile {
                        path: "skills/test/WorkFlow.Json".into(),
                        content: "{}".into(),
                    },
                ],
            ),
        ] {
            assert!(validate_package(&files, entrypoint).is_err());
        }
    }

    #[test]
    fn markdown_crud_is_durable_and_generation_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        let preview = lib
            .save_preview(
                PreviewSkill {
                    name: "review".into(),
                    entrypoint: "SKILL.md".into(),
                    provenance: Provenance::Markdown {
                        locator: "pasted_markdown".into(),
                    },
                },
                &[PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Review".into(),
                }],
                vec![],
            )
            .unwrap();
        let accepted = AcceptImport {
            preview_id: preview.preview_id,
            digest: preview.digest,
            idempotency_key: "one".into(),
        };
        let skill = lib.accept_import(&accepted).unwrap();
        let same = lib
            .accept_import(&AcceptImport {
                idempotency_key: "same-content".into(),
                ..accepted.clone()
            })
            .unwrap();
        assert_eq!(same.id, skill.id);
        assert_eq!(skill.generation, 1);
        assert!(
            lib.accept_import(&AcceptImport {
                preview_id: "imp_missing".into(),
                digest: "changed".into(),
                idempotency_key: "two".into(),
            })
            .is_err()
        );
        drop(lib);
        let mut lib = Library::open(dir.path()).unwrap();
        assert_eq!(lib.skills().unwrap().len(), 1);
        assert!(lib.archive_skill(&skill.id, 2).is_err());
        assert!(lib.archive_skill(&skill.id, 1).unwrap().archived);
        assert!(lib.accept_import(&accepted).is_err());
        assert!(
            lib.accept_import(&AcceptImport {
                idempotency_key: "after-archive".into(),
                ..accepted
            })
            .is_err()
        );
    }

    #[test]
    fn workflow_null_patch_archive_and_idempotent_create_are_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        let draft = lib
            .create_workflow(&CreateWorkflow {
                name: "release flow".into(),
                summary: "Prepare a release".into(),
                idempotency_key: "create-release".into(),
            })
            .unwrap();
        let draft = lib
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "build".into(),
                        label: "Build".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Build {
                            skill_version_id: "skl_missing".into(),
                            runner: "build.json".into(),
                        },
                    }]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::new(),
                    })),
                    follow_on: Some(Some(FollowOn {
                        workflow_version_id: "wfv_missing".into(),
                        when: FollowWhen::Success,
                        inputs: BTreeMap::new(),
                    })),
                },
            )
            .unwrap();
        let clear: UpdateWorkflow = serde_json::from_value(json!({
            "expected_generation": draft.generation,
            "trigger": null,
            "follow_on": null
        }))
        .unwrap();
        assert_eq!(clear.trigger, Some(None));
        assert_eq!(clear.follow_on, Some(None));
        let cleared = lib.update_workflow(&draft.id, &clear).unwrap();
        assert_eq!(cleared.trigger, None);
        assert_eq!(cleared.follow_on, None);
        assert!(
            lib.create_workflow(&CreateWorkflow {
                name: "release flow".into(),
                summary: "Prepare a release".into(),
                idempotency_key: "create-release".into(),
            })
            .is_err()
        );
        let archived = lib.archive_workflow(&draft.id, cleared.generation).unwrap();
        assert!(archived.archived);
        assert!(
            lib.archive_workflow(&draft.id, archived.generation)
                .is_err()
        );
        assert!(
            lib.update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: archived.generation,
                    name: None,
                    summary: Some("changed".into()),
                    steps: None,
                    trigger: None,
                    follow_on: None,
                }
            )
            .is_err()
        );
        assert_eq!(lib.workflows().unwrap()["workflows"], json!([]));

        drop(lib);
        let path = dir.path().to_owned();
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut library = Library::open(&path).unwrap();
                    barrier.wait();
                    library
                        .create_workflow(&CreateWorkflow {
                            name: "one result".into(),
                            summary: "Same retried request".into(),
                            idempotency_key: "raced-create".into(),
                        })
                        .unwrap()
                        .id
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn simulation_never_creates_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        let draft = lib
            .create_workflow(&CreateWorkflow {
                name: "release".into(),
                summary: "Draft release notes".into(),
                idempotency_key: "create".into(),
            })
            .unwrap();
        let draft = lib
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "draft".into(),
                        label: "Draft".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Skill {
                            skill_version_id: "missing".into(),
                        },
                    }]),
                    trigger: None,
                    follow_on: None,
                },
            )
            .unwrap();
        assert!(matches!(
            lib.publish(
                &draft.id,
                &PublishWorkflow {
                    expected_generation: draft.generation,
                    idempotency_key: "publish".into()
                }
            )
            .unwrap(),
            Publication::Rejected { diagnostics }
                if diagnostics.iter().any(|diagnostic| diagnostic.message.contains("skill version"))
        ));
        assert!(!dir.path().join("skill-workflows/state.sqlite").exists());
    }

    #[test]
    fn composed_actions_preserve_provenance_and_compile_with_native_engine() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let imported = vec![
            PackageFile {
                path: "SKILL.md".into(),
                content: "# Produce and consume".into(),
            },
            PackageFile {
                path: "produce.json".into(),
                content: r#"{"version":1,"inputs":{},"output":{"name":"draft","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"shared.md","required_tools":[]}}"#.into(),
            },
            PackageFile {
                path: "consume.json".into(),
                content: r#"{"version":1,"inputs":{"draft":{"kind":"artifact","step":"produce","name":"draft"}},"output":{"name":"summary","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"shared.md","required_tools":[]}}"#.into(),
            },
            PackageFile {
                path: "shared.md".into(),
                content: "Use the supplied draft without changing its meaning.".into(),
            },
        ];
        let skill = accept_package(
            &mut library,
            "producer-consumer",
            imported.clone(),
            "import-pc",
        );
        let child = library
            .create_workflow(&CreateWorkflow {
                name: "child".into(),
                summary: "Produce then consume".into(),
                idempotency_key: "create-child-native".into(),
            })
            .unwrap();
        let child = library
            .update_workflow(
                &child.id,
                &UpdateWorkflow {
                    expected_generation: child.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![
                        WorkflowStep {
                            id: "produce".into(),
                            label: "Produce".into(),
                            needs: vec![],
                            required: true,
                            kind: WorkflowStepKind::Action {
                                skill_version_id: skill.id.clone(),
                                resource: "produce.json".into(),
                            },
                        },
                        WorkflowStep {
                            id: "consume".into(),
                            label: "Consume".into(),
                            needs: vec!["produce".into()],
                            required: false,
                            kind: WorkflowStepKind::Action {
                                skill_version_id: skill.id.clone(),
                                resource: "consume.json".into(),
                            },
                        },
                    ]),
                    trigger: None,
                    follow_on: None,
                },
            )
            .unwrap();
        let (child_version, diagnostics) = published(
            library
                .publish(
                    &child.id,
                    &PublishWorkflow {
                        expected_generation: child.generation,
                        idempotency_key: "publish-child-native".into(),
                    },
                )
                .unwrap(),
        );
        assert!(diagnostics.is_empty());
        let collision = WorkflowDraft {
            id: "wf_collision".into(),
            name: "collision".into(),
            summary: "Flattened identifiers must remain unique".into(),
            generation: 1,
            steps: vec![
                WorkflowStep {
                    id: "nested".into(),
                    label: "Nested".into(),
                    needs: vec![],
                    required: true,
                    kind: WorkflowStepKind::Workflow {
                        workflow_version_id: child_version.id.clone(),
                        inputs: BTreeMap::new(),
                    },
                },
                WorkflowStep {
                    id: "nested__produce".into(),
                    label: "Conflicting action".into(),
                    needs: vec![],
                    required: true,
                    kind: WorkflowStepKind::Action {
                        skill_version_id: skill.id.clone(),
                        resource: "produce.json".into(),
                    },
                },
            ],
            trigger: None,
            follow_on: None,
            updated_at: now(),
            archived: false,
        };
        let diagnostics = library.lint(&collision).unwrap();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid_composition"
                && diagnostic
                    .message
                    .contains("flattened step id nested__produce collides")
        }));

        let parent = library
            .create_workflow(&CreateWorkflow {
                name: "parent".into(),
                summary: "Nested and follow-on composition".into(),
                idempotency_key: "create-parent-native".into(),
            })
            .unwrap();
        let parent = library
            .update_workflow(
                &parent.id,
                &UpdateWorkflow {
                    expected_generation: parent.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "nested".into(),
                        label: "Optional nested workflow".into(),
                        needs: vec![],
                        required: false,
                        kind: WorkflowStepKind::Workflow {
                            workflow_version_id: child_version.id.clone(),
                            inputs: BTreeMap::new(),
                        },
                    }]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::from(["major-feature".into()]),
                    })),
                    follow_on: Some(Some(FollowOn {
                        workflow_version_id: child_version.id,
                        when: FollowWhen::Success,
                        inputs: BTreeMap::new(),
                    })),
                },
            )
            .unwrap();
        let (parent_version, diagnostics) = published(
            library
                .publish(
                    &parent.id,
                    &PublishWorkflow {
                        expected_generation: parent.generation,
                        idempotency_key: "publish-parent-native".into(),
                    },
                )
                .unwrap(),
        );
        assert!(diagnostics.is_empty());
        library
            .archive_workflow(&child.id, child.generation)
            .unwrap();
        let attachment = library
            .put_attachment(
                "acme/widgets",
                "native-composition",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: parent_version.id,
                    params: BTreeMap::new(),
                    idempotency_key: "attach-parent-native".into(),
                },
            )
            .unwrap();
        let response = library
            .validate_attachment(
                "acme/widgets",
                "native-composition",
                &ValidateAttachment {
                    expected_generation: attachment.generation,
                    source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                },
            )
            .unwrap();
        let setup = &response["setup"];
        assert_eq!(setup["runnable"], true);
        assert!(setup["warnings"].as_array().unwrap().iter().any(|warning| {
            warning
                .as_str()
                .unwrap()
                .contains("belongs to archived workflow")
        }));
        assert!(
            response["attachment"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == "archived_workflow_reference")
        );
        let plan = compile_exported_setup(setup);
        let required = plan
            .definition()
            .steps
            .iter()
            .map(|step| (step.id.as_str(), step.required))
            .collect::<Vec<_>>();
        assert_eq!(
            required,
            vec![
                ("nested__produce", false),
                ("nested__consume", false),
                ("follow__produce", true),
                ("follow__consume", false),
            ]
        );
        for (consumer, producer) in [
            ("nested__consume", "nested__produce"),
            ("follow__consume", "follow__produce"),
        ] {
            assert_eq!(
                plan.actions[consumer].definition.inputs["draft"],
                ActionInput::Artifact {
                    step: producer.into(),
                    name: "draft".into(),
                }
            );
            assert_eq!(
                plan.actions[consumer].prompt.as_deref(),
                Some("Use the supplied draft without changing its meaning.")
            );
        }
        let exported: Vec<PackageFile> = serde_json::from_value(setup["files"].clone()).unwrap();
        for source in imported {
            let expected = format!(
                "/skills/{}/{}",
                skill.id,
                if source.path == "SKILL.md" {
                    "SKILL.md"
                } else {
                    &source.path
                }
            );
            let copy = exported
                .iter()
                .find(|file| file.path.ends_with(&expected))
                .unwrap();
            assert_eq!(copy.content.as_bytes(), source.content.as_bytes());
        }
    }

    #[test]
    fn composition_rejects_cross_scope_inputs_and_blocks_oversized_native_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::open(dir.path()).unwrap();
        let provider = accept_package(
            &mut library,
            "prompt-provider",
            vec![
                PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Provider".into(),
                },
                PackageFile {
                    path: "shared.md".into(),
                    content: "This prompt belongs only to the provider skill.".into(),
                },
            ],
            "import-provider",
        );
        let consumer = accept_package(
            &mut library,
            "bad-consumer",
            vec![
                PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Consumer".into(),
                },
                PackageFile {
                    path: "consume.json".into(),
                    content: r#"{"version":1,"inputs":{"draft":{"kind":"artifact","step":"guidance","name":"draft"}},"output":{"name":"summary","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"shared.md","required_tools":[]}}"#.into(),
                },
            ],
            "import-consumer",
        );
        let invalid = WorkflowDraft {
            id: "wf_invalid".into(),
            name: "invalid provenance".into(),
            summary: "A different skill cannot supply this action's prompt or producer".into(),
            generation: 1,
            steps: vec![
                WorkflowStep {
                    id: "guidance".into(),
                    label: "Guidance is not an action producer".into(),
                    needs: vec![],
                    required: true,
                    kind: WorkflowStepKind::Skill {
                        skill_version_id: provider.id,
                    },
                },
                WorkflowStep {
                    id: "consume".into(),
                    label: "Consume".into(),
                    needs: vec!["guidance".into()],
                    required: true,
                    kind: WorkflowStepKind::Action {
                        skill_version_id: consumer.id,
                        resource: "consume.json".into(),
                    },
                },
            ],
            trigger: None,
            follow_on: None,
            updated_at: now(),
            archived: false,
        };
        let diagnostics = library.lint(&invalid).unwrap();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid_composition"
                && diagnostic.message.contains(
                    "artifact producer guidance is not a productive action in the same workflow",
                )
        }));
        let missing_prompt = accept_package(
            &mut library,
            "missing-prompt",
            vec![
                PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Missing prompt".into(),
                },
                PackageFile {
                    path: "action.json".into(),
                    content: r#"{"version":1,"inputs":{},"output":{"name":"draft","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"shared.md","required_tools":[]}}"#.into(),
                },
            ],
            "import-missing-prompt",
        );
        let missing_prompt_draft = WorkflowDraft {
            id: "wf_missing_prompt".into(),
            name: "missing own prompt".into(),
            summary: "Another selected skill has the same path".into(),
            generation: 1,
            steps: vec![
                invalid.steps[0].clone(),
                WorkflowStep {
                    id: "write".into(),
                    label: "Write".into(),
                    needs: vec!["guidance".into()],
                    required: true,
                    kind: WorkflowStepKind::Action {
                        skill_version_id: missing_prompt.id,
                        resource: "action.json".into(),
                    },
                },
            ],
            trigger: None,
            follow_on: None,
            updated_at: now(),
            archived: false,
        };
        let diagnostics = library.lint(&missing_prompt_draft).unwrap();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid_composition"
                && diagnostic
                    .message
                    .contains("prompt shared.md is absent from skill")
        }));
        let source_input = accept_package(
            &mut library,
            "source-input",
            vec![
                PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Source input".into(),
                },
                PackageFile {
                    path: "action.json".into(),
                    content: r#"{"version":1,"inputs":{"source":{"kind":"source","path":"src/lib.rs"}},"output":{"name":"draft","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"prompt.md","required_tools":[]}}"#.into(),
                },
                PackageFile {
                    path: "prompt.md".into(),
                    content: "Review the source.".into(),
                },
            ],
            "import-source-input",
        );
        let source_draft = WorkflowDraft {
            id: "wf_source_input".into(),
            name: "source input".into(),
            summary: "Source must bind to the exported commit".into(),
            generation: 1,
            steps: vec![WorkflowStep {
                id: "review".into(),
                label: "Review".into(),
                needs: vec![],
                required: true,
                kind: WorkflowStepKind::Action {
                    skill_version_id: source_input.id,
                    resource: "action.json".into(),
                },
            }],
            trigger: None,
            follow_on: None,
            updated_at: now(),
            archived: false,
        };
        let diagnostics = library.lint(&source_draft).unwrap();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid_composition"
                && diagnostic
                    .message
                    .contains("source inputs require validation after export")
        }));

        let mut large_package = vec![
            PackageFile {
                path: "SKILL.md".into(),
                content: "# Large package".into(),
            },
            PackageFile {
                path: "action.json".into(),
                content: r#"{"version":1,"inputs":{},"output":{"name":"draft","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"prompt.md","required_tools":[]}}"#.into(),
            },
            PackageFile {
                path: "prompt.md".into(),
                content: "Produce a draft.".into(),
            },
        ];
        large_package.extend((0..62).map(|index| PackageFile {
            path: format!("references/{index}.md"),
            content: format!("reference {index}"),
        }));
        let large = accept_package(&mut library, "large", large_package, "import-large");
        let draft = library
            .create_workflow(&CreateWorkflow {
                name: "large workflow".into(),
                summary: "Exceeds the exact native resource ceiling".into(),
                idempotency_key: "create-large".into(),
            })
            .unwrap();
        let draft = library
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "produce".into(),
                        label: "Produce".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Action {
                            skill_version_id: large.id,
                            resource: "action.json".into(),
                        },
                    }]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::new(),
                    })),
                    follow_on: None,
                },
            )
            .unwrap();
        let (version, diagnostics) = published(
            library
                .publish(
                    &draft.id,
                    &PublishWorkflow {
                        expected_generation: draft.generation,
                        idempotency_key: "publish-large".into(),
                    },
                )
                .unwrap(),
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "unmet_runtime_binding"
                && diagnostic.message.contains("native limit is 64")
        }));
        let attachment = library
            .put_attachment(
                "acme/widgets",
                "large",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: version.id,
                    params: BTreeMap::new(),
                    idempotency_key: "attach-large".into(),
                },
            )
            .unwrap();
        let response = library
            .validate_attachment(
                "acme/widgets",
                "large",
                &ValidateAttachment {
                    expected_generation: attachment.generation,
                    source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                },
            )
            .unwrap();
        assert_eq!(response["state"], "blocked");
        assert_eq!(response["setup"]["runnable"], false);
        assert_eq!(response["setup"]["pipeline_install_template"], Value::Null);
        assert_eq!(response["next_actions"], json!([]));
        assert!(
            response["setup"]["unmet_bindings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message.as_str().unwrap().contains("resources"))
        );
    }

    #[test]
    fn published_action_exports_an_existing_pipeline_recipe_and_keeps_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = Library::open(dir.path()).unwrap();
        let preview = lib.save_preview(
            PreviewSkill{name:"release".into(),entrypoint:"SKILL.md".into(),provenance:Provenance::Markdown{locator:"pasted_markdown".into()}},
            &[
                PackageFile{path:"SKILL.md".into(),content:"# Release".into()},
                PackageFile{path:"draft.json".into(),content:r#"{"version":1,"inputs":{},"output":{"name":"draft","media_type":"text/markdown","max_bytes":16384},"action":{"kind":"prompt","prompt":"draft.md","required_tools":[]}}"#.into()},
                PackageFile{path:"draft.md".into(),content:"Write release notes.".into()},
            ], vec![]).unwrap();
        let skill = lib
            .accept_import(&AcceptImport {
                preview_id: preview.preview_id,
                digest: preview.digest,
                idempotency_key: "import".into(),
            })
            .unwrap();
        let draft = lib
            .create_workflow(&CreateWorkflow {
                name: "release-notes".into(),
                summary: "Draft release notes".into(),
                idempotency_key: "create".into(),
            })
            .unwrap();
        let draft = lib
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "draft".into(),
                        label: "Draft notes".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Action {
                            skill_version_id: skill.id.clone(),
                            resource: "draft.json".into(),
                        },
                    }]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::from(["major-feature".into()]),
                    })),
                    follow_on: None,
                },
            )
            .unwrap();
        let (version, _) = published(
            lib.publish(
                &draft.id,
                &PublishWorkflow {
                    expected_generation: draft.generation,
                    idempotency_key: "publish".into(),
                },
            )
            .unwrap(),
        );
        assert!(lib.archive_skill(&skill.id, skill.generation).is_err());
        let edited = lib
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: Some("Edited draft".into()),
                    steps: None,
                    trigger: None,
                    follow_on: None,
                },
            )
            .unwrap();
        assert_eq!(
            lib.version(&version.id).unwrap().definition.summary,
            "Draft release notes"
        );
        assert!(
            lib.update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: Some("stale".into()),
                    steps: None,
                    trigger: None,
                    follow_on: None
                }
            )
            .is_err()
        );
        assert_eq!(edited.generation, draft.generation + 1);
        let attachment = lib
            .put_attachment(
                "acme/widgets",
                "release",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: version.id.clone(),
                    params: BTreeMap::new(),
                    idempotency_key: "attach".into(),
                },
            )
            .unwrap();
        let retry = lib
            .put_attachment(
                "acme/widgets",
                "release",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: version.id.clone(),
                    params: BTreeMap::new(),
                    idempotency_key: "attach".into(),
                },
            )
            .unwrap();
        assert_eq!(retry.generation, attachment.generation);
        let other_repository = lib
            .put_attachment(
                "acme/other",
                "release",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: version.id.clone(),
                    params: BTreeMap::new(),
                    idempotency_key: "attach-other".into(),
                },
            )
            .unwrap();
        assert_eq!(other_repository.repository, "acme/other");
        let updated_attachment = lib
            .put_attachment(
                "acme/widgets",
                "release",
                &PutAttachment {
                    expected_generation: Some(attachment.generation),
                    workflow_version_id: version.id.clone(),
                    params: BTreeMap::from([("mode".into(), json!("review"))]),
                    idempotency_key: "attach-update".into(),
                },
            )
            .unwrap();
        assert!(
            lib.put_attachment(
                "acme/widgets",
                "release",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: version.id.clone(),
                    params: BTreeMap::new(),
                    idempotency_key: "attach".into(),
                },
            )
            .is_err()
        );
        let value = lib
            .validate_attachment(
                "acme/widgets",
                "release",
                &ValidateAttachment {
                    expected_generation: updated_attachment.generation,
                    source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                },
            )
            .unwrap();
        assert_eq!(value["setup"]["runnable"], true);
        let recipe: PipelineRecipe =
            serde_json::from_value(value["setup"]["pipeline_recipe"].clone()).unwrap();
        recipe.validate_structure(1).unwrap();
        let action: crate::skill_workflow::action::ActionDefinition = serde_json::from_str(
            &lib.package(&skill.id)
                .unwrap()
                .into_iter()
                .find(|file| file.path == "draft.json")
                .unwrap()
                .content,
        )
        .unwrap();
        action.validate().unwrap();
        assert!(
            value["setup"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"].as_str().unwrap().ends_with("/draft.json"))
        );

        let build = lib
            .create_workflow(&CreateWorkflow {
                name: "build declaration".into(),
                summary: "Declaration without a command binding".into(),
                idempotency_key: "create-build".into(),
            })
            .unwrap();
        let build = lib
            .update_workflow(
                &build.id,
                &UpdateWorkflow {
                    expected_generation: build.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "build".into(),
                        label: "Build".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Build {
                            skill_version_id: skill.id.clone(),
                            runner: "draft.json".into(),
                        },
                    }]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::new(),
                    })),
                    follow_on: None,
                },
            )
            .unwrap();
        let (build_version, diagnostics) = published(
            lib.publish(
                &build.id,
                &PublishWorkflow {
                    expected_generation: build.generation,
                    idempotency_key: "publish-build".into(),
                },
            )
            .unwrap(),
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.level == "error")
        );
        let blocked_attachment = lib
            .put_attachment(
                "acme/widgets",
                "blocked-build",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: build_version.id,
                    params: BTreeMap::new(),
                    idempotency_key: "attach-build".into(),
                },
            )
            .unwrap();
        let blocked = lib
            .validate_attachment(
                "acme/widgets",
                "blocked-build",
                &ValidateAttachment {
                    expected_generation: blocked_attachment.generation,
                    source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                },
            )
            .unwrap();
        assert_eq!(blocked["state"], "blocked");
        assert_eq!(blocked["setup"]["runnable"], false);
        assert_eq!(blocked["setup"]["pipeline_install_template"], Value::Null);
        assert_eq!(blocked["next_actions"], json!([]));
        assert_eq!(
            blocked["attachment"]["diagnostics"][0]["code"],
            "unmet_runtime_binding"
        );

        let parent = lib
            .create_workflow(&CreateWorkflow {
                name: "major feature follow-up".into(),
                summary: "Compose a published workflow and success follow-on".into(),
                idempotency_key: "create-parent".into(),
            })
            .unwrap();
        let parent = lib
            .update_workflow(
                &parent.id,
                &UpdateWorkflow {
                    expected_generation: parent.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![
                        WorkflowStep {
                            id: "prepare".into(),
                            label: "Prepare".into(),
                            needs: vec![],
                            required: true,
                            kind: WorkflowStepKind::Action {
                                skill_version_id: skill.id.clone(),
                                resource: "draft.json".into(),
                            },
                        },
                        WorkflowStep {
                            id: "reuse".into(),
                            label: "Reuse".into(),
                            needs: vec!["prepare".into()],
                            required: true,
                            kind: WorkflowStepKind::Workflow {
                                workflow_version_id: version.id.clone(),
                                inputs: BTreeMap::new(),
                            },
                        },
                    ]),
                    trigger: Some(Some(Trigger::ConfirmedMerge {
                        target: "main".into(),
                        labels_all: BTreeSet::from(["major-feature".into()]),
                    })),
                    follow_on: Some(Some(FollowOn {
                        workflow_version_id: version.id.clone(),
                        when: FollowWhen::Success,
                        inputs: BTreeMap::new(),
                    })),
                },
            )
            .unwrap();
        let (parent_version, diagnostics) = published(
            lib.publish(
                &parent.id,
                &PublishWorkflow {
                    expected_generation: parent.generation,
                    idempotency_key: "publish-parent".into(),
                },
            )
            .unwrap(),
        );
        assert!(diagnostics.is_empty());
        let simulation = lib
            .simulate(&SimulationRequest {
                workflow_version_id: parent_version.id.clone(),
                repository: Some("acme/widgets".into()),
                source_revision: Some("0123456789abcdef0123456789abcdef01234567".into()),
                event: None,
            })
            .unwrap();
        assert_eq!(simulation.outcome, "passed");
        assert_eq!(
            simulation
                .steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            vec!["prepare", "reuse__draft", "follow__draft"]
        );
        let parent_attachment = lib
            .put_attachment(
                "acme/widgets",
                "major-feature",
                &PutAttachment {
                    expected_generation: None,
                    workflow_version_id: parent_version.id,
                    params: BTreeMap::new(),
                    idempotency_key: "attach-parent".into(),
                },
            )
            .unwrap();
        let setup = lib
            .validate_attachment(
                "acme/widgets",
                "major-feature",
                &ValidateAttachment {
                    expected_generation: parent_attachment.generation,
                    source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                },
            )
            .unwrap();
        assert_eq!(setup["setup"]["runnable"], true);
        assert_eq!(
            setup["setup"]["pipeline_recipe"]["steps"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
}
