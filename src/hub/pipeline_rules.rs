//! Operator-local repository rules feed the existing productive workflow journal.
//! Forge hints require a live read; labels describe observation time, not merge time.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::connection::ConnectedRepository;
use super::forge_api::ForgeApi;
use crate::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeTrigger {
    pub target: String,
    #[serde(default)]
    pub labels_all: BTreeSet<String>,
}

/// Strict graph template. The observer supplies exact source; no native target is changed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipelineRecipe {
    pub version: u32,
    pub name: String,
    pub skill: SkillReference,
    pub steps: Vec<StepDefinition>,
}
impl PipelineRecipe {
    pub fn digest(&self) -> Result<String> {
        Ok(crate::store::digest(&serde_json::to_vec(self)?))
    }
    fn definition(&self, source: WorkflowSource) -> WorkflowDefinition {
        WorkflowDefinition {
            version: self.version,
            name: self.name.clone(),
            source,
            skill: self.skill.clone(),
            steps: self.steps.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PipelineRule {
    pub repository: String,
    pub provider: String,
    pub host: String,
    pub provider_repository_id: i64,
    pub rule_id: String,
    pub generation: i64,
    pub enabled: bool,
    pub trigger: MergeTrigger,
    pub recipe: PipelineRecipe,
    pub recipe_digest: String,
    pub skill_digest: String,
    pub profile_id: String,
    pub profile_digest: String,
    /// An explicit operator file, outside candidate source. Removal revokes future delivery.
    pub profile_path: PathBuf,
    /// This initial authority ceiling permits productive local actions only.
    pub authority_ceiling: String,
    pub git: PathBuf,
    pub git_identity: crate::execution::executable::ExecutableIdentity,
    pub git_repository: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedMerge {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub provider_repository_id: i64,
    pub proposal_number: u64,
    pub accepted_commit: String,
    pub target: String,
    pub labels: BTreeSet<String>,
    pub observed_at: i64,
    pub delivery_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatchPreview {
    pub matches: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingDispatch {
    pub dispatch_id: String,
    pub run_id: String,
    pub event: ConfirmedMerge,
    /// Frozen winning configuration; later edits cannot change this run.
    pub rule: PipelineRule,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchView {
    /// SQLite insertion sequence for bounded descending status pagination.
    pub sequence: i64,
    pub dispatch: PendingDispatch,
    pub state: String,
    pub claim_generation: i64,
    pub reason: Option<String>,
}
#[derive(Clone, Debug)]
pub struct DispatchClaim {
    pub dispatch: PendingDispatch,
    pub generation: i64,
}

/// Re-read a numbered proposal using the configured provider and stable repository.
/// GitLab fast-forward proof is accepted only while the target tip still equals its head.
pub async fn confirm_merge(
    api: ForgeApi<'_>,
    token: &str,
    repository: &ConnectedRepository,
    number: i64,
    observed_at: i64,
    delivery_id: Option<String>,
) -> Result<Option<ConfirmedMerge>> {
    if number < 1 {
        return Err(Error::Invalid("proposal number must be positive".into()));
    }
    verify_repository(api, token, repository)
        .await
        .map_err(observation_error)?;
    confirm_in_repository(api, token, repository, number, observed_at, delivery_id)
        .await
        .map_err(observation_error)
}

enum ObservationError {
    Forge(super::github::GhError),
    Invalid(Error),
}
impl From<super::github::GhError> for ObservationError {
    fn from(e: super::github::GhError) -> Self {
        Self::Forge(e)
    }
}
impl From<Error> for ObservationError {
    fn from(e: Error) -> Self {
        Self::Invalid(e)
    }
}
fn observation_error(error: ObservationError) -> Error {
    match error {
        ObservationError::Invalid(e) => e,
        ObservationError::Forge(error) => Error::Conflict(match error {
            super::github::GhError::Auth(_) => {
                "forge authentication failed; no event accepted".into()
            }
            super::github::GhError::Limited(at) => {
                format!("forge rate limited until {at}; no event accepted")
            }
            super::github::GhError::Failed(_) => "forge read failed; no event accepted".into(),
        }),
    }
}
async fn verify_repository(
    api: ForgeApi<'_>,
    token: &str,
    repository: &ConnectedRepository,
) -> std::result::Result<(), ObservationError> {
    let Some(info) = api.repo(token, &repository.full_name).await? else {
        return Err(Error::NotFound("configured forge repository".into()).into());
    };
    if info.repository_id != repository.repository_id || info.full_name != repository.full_name {
        return Err(Error::Conflict("configured forge repository identity changed".into()).into());
    }
    Ok(())
}
async fn confirm_in_repository(
    api: ForgeApi<'_>,
    token: &str,
    repository: &ConnectedRepository,
    number: i64,
    observed_at: i64,
    delivery_id: Option<String>,
) -> std::result::Result<Option<ConfirmedMerge>, ObservationError> {
    if number < 1 {
        return Err(Error::Invalid("proposal number must be positive".into()).into());
    }
    let provider = match api {
        ForgeApi::Github(_) => "github",
        ForgeApi::Gitlab(_) => "gitlab",
    };
    let Some(pull) = api.pull(token, repository, number).await? else {
        return Ok(None);
    };
    let merged = pull.get("merged").and_then(|v| v.as_bool()) == Some(true);
    if pull.get("number").and_then(|v| v.as_i64()) != Some(number) {
        return Err(Error::Conflict("forge proposal identity differs".into()).into());
    }
    let target_identity_matches = if provider == "github" {
        pull.pointer("/base/repo/id").and_then(|v| v.as_i64()) == Some(repository.repository_id)
            && pull
                .pointer("/base/repo/full_name")
                .and_then(|v| v.as_str())
                == Some(repository.full_name.as_str())
    } else {
        pull.get("target_project_id").and_then(|v| v.as_i64()) == Some(repository.repository_id)
    };
    if !target_identity_matches {
        return Err(Error::Conflict(
            "forge proposal target repository differs or is missing".into(),
        )
        .into());
    }
    if !merged {
        return Ok(None);
    }
    let target = pull
        .pointer("/base/ref")
        .or_else(|| pull.get("target_branch"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Invalid("merged proposal has no target".into()))?
        .to_owned();
    let mut accepted = pull
        .get("merge_commit_sha")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if provider == "gitlab" && accepted.is_none() {
        accepted = pull
            .get("squash_commit_sha")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if accepted.is_none() {
            let head = pull
                .pointer("/head/sha")
                .and_then(|v| v.as_str())
                .filter(|v| is_oid(v));
            let tip = api.ref_sha(token, repository, &target).await?;
            if head.is_some() && tip.as_deref() == head {
                accepted = head.map(str::to_owned);
            }
        }
    }
    let accepted_commit = accepted.filter(|v| is_oid(v)).ok_or_else(|| Error::Conflict(
        "merged proposal has no verifiable accepted commit (fast-forward target may have advanced)".into()))?;
    let labels = pull
        .get("labels")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v.as_str()
                .or_else(|| v.get("name").and_then(|n| n.as_str()))
        })
        .map(str::to_owned)
        .collect();
    let event = ConfirmedMerge {
        provider: provider.into(),
        host: repository.host.clone(),
        repository: repository.full_name.clone(),
        provider_repository_id: repository.repository_id,
        proposal_number: number as u64,
        accepted_commit,
        target,
        labels,
        observed_at,
        delivery_id,
    };
    validate_event(&event)?;
    Ok(Some(event))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollBlocked {
    pub kind: String,
    pub reason: String,
    /// Validated provider retry time in Unix seconds, at most one day ahead.
    pub retry_at: Option<i64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PollReport {
    pub observations: Vec<serde_json::Value>,
    pub skipped_confirmed: usize,
    pub observed_at: i64,
    pub unread_pages: Option<u32>,
    pub checkpoint_advanced: bool,
    pub blocked: Option<PollBlocked>,
    pub coverage: &'static str,
}
impl PollReport {
    fn block(&mut self, error: ObservationError) {
        use super::github::GhError;
        if self.blocked.is_some() && matches!(&error, ObservationError::Invalid(_)) {
            return;
        }
        self.blocked = Some(match error {
            ObservationError::Forge(GhError::Auth(_)) => PollBlocked {
                kind: "authentication".into(),
                reason:
                    "forge authentication failed; restart observation after fixing the credential"
                        .into(),
                retry_at: None,
            },
            ObservationError::Forge(GhError::Limited(at)) => PollBlocked {
                kind: "rate_limited".into(),
                reason: "forge rate limited; successful checkpoint unchanged".into(),
                retry_at: if at > self.observed_at / 1000 && at <= self.observed_at / 1000 + 86400 {
                    Some(at)
                } else {
                    None
                },
            },
            ObservationError::Forge(GhError::Failed(_)) => PollBlocked {
                kind: "unavailable".into(),
                reason: "forge read failed; successful checkpoint unchanged".into(),
                retry_at: None,
            },
            ObservationError::Invalid(error) => PollBlocked {
                kind: "unverifiable".into(),
                reason: error.to_string(),
                retry_at: None,
            },
        });
    }
}

/// Listings are discovery hints only. A hint may suppress a read only when its exact
/// semantic event was already confirmed. Unconfirmed proposals are always re-read;
/// local observation timestamps never exclude changed/reopened proposals.
pub async fn poll_merges(
    api: ForgeApi<'_>,
    token: &str,
    repository: &ConnectedRepository,
    journal: &mut RuleJournal,
    observed_at: i64,
) -> Result<PollReport> {
    let provider = match api {
        ForgeApi::Github(_) => "github",
        ForgeApi::Gitlab(_) => "gitlab",
    };
    journal.assert_repository(provider, &repository.host, repository.repository_id)?;
    let mut report = PollReport {
        observations: vec![],
        skipped_confirmed: 0,
        observed_at,
        unread_pages: None,
        checkpoint_advanced: false,
        blocked: None,
        coverage: "newest 100 updated proposals; known confirmed events are skipped, unconfirmed candidates are re-read; labels are observation-time",
    };
    if let Err(error) = verify_repository(api, token, repository).await {
        report.block(error);
        return Ok(report);
    }
    let listing = match api.recent_closed_pulls(token, repository).await {
        Ok(value) => value,
        Err(error) => {
            report.block(error.into());
            return Ok(report);
        }
    };
    report.unread_pages = Some(listing.unread);
    for (listing_index, candidate) in listing.items.iter().take(100).enumerate() {
        let Some(number) = candidate
            .get("number")
            .and_then(|v| v.as_i64())
            .filter(|v| *v > 0)
        else {
            let error = Error::Invalid("forge list has no proposal number".into());
            report.observations.push(serde_json::json!({"listing_index":listing_index,"accepted":[],"blocked":error.to_string()}));
            report.block(error.into());
            continue;
        };
        // A fallback head may only skip an already-confirmed GitLab fast-forward;
        // it can never authorize the initial event without live target-tip proof.
        let commit = candidate
            .get("merge_commit_sha")
            .and_then(|v| v.as_str())
            .filter(|v| is_oid(v))
            .or_else(|| {
                candidate
                    .get("squash_commit_sha")
                    .and_then(|v| v.as_str())
                    .filter(|v| is_oid(v))
            })
            .or_else(|| {
                if provider == "gitlab"
                    && candidate.get("merged").and_then(|v| v.as_bool()) == Some(true)
                {
                    candidate
                        .pointer("/head/sha")
                        .and_then(|v| v.as_str())
                        .filter(|v| is_oid(v))
                } else {
                    None
                }
            });
        if let Some(commit) = commit
            && journal.confirmed_hint(provider, repository, number, commit)?
        {
            report.skipped_confirmed += 1;
            continue;
        }
        match confirm_in_repository(api,token,repository,number,observed_at,None).await {
            Ok(Some(event))=>{
                let accepted=journal.accept(&event)?.into_iter().map(|d|serde_json::json!({"dispatch_id":d.dispatch_id,"run_id":d.run_id})).collect::<Vec<_>>();
                report.observations.push(serde_json::json!({"proposal":number,"event":event,"accepted":accepted}));
            }
            Ok(None)=>report.observations.push(serde_json::json!({"proposal":number,"accepted":[],"reason":"proposal not confirmed merged"})),
            Err(ObservationError::Invalid(error))=>{
                report.observations.push(serde_json::json!({"proposal":number,"accepted":[],"blocked":error.to_string()}));
                report.block(error.into());
            }
            Err(error)=>{report.block(error);return Ok(report);}
        }
    }
    if report.blocked.is_none() {
        journal.record_poll(
            &format!(
                "{provider}://{}/{}#{}",
                repository.host, repository.full_name, repository.repository_id
            ),
            observed_at,
            listing.unread,
        )?;
        report.checkpoint_advanced = true;
    }
    Ok(report)
}

pub fn preview(rule: &PipelineRule, event: &ConfirmedMerge) -> MatchPreview {
    let mut reasons = Vec::new();
    if rule.repository != event.repository
        || rule.provider != event.provider
        || rule.host != event.host
        || rule.provider_repository_id != event.provider_repository_id
    {
        reasons.push("repository identity differs".into());
    }
    if !rule.enabled {
        reasons.push("rule is disabled".into());
    }
    if rule.trigger.target != event.target {
        reasons.push("target branch differs".into());
    }
    let missing: Vec<_> = rule
        .trigger
        .labels_all
        .difference(&event.labels)
        .cloned()
        .collect();
    if !missing.is_empty() {
        reasons.push(format!(
            "missing observation-time labels: {}",
            missing.join(", ")
        ));
    }
    MatchPreview {
        matches: reasons.is_empty(),
        reasons,
    }
}

/// SQLite outbox. Claims are generation-fenced and intentionally have no automatic lease
/// expiry: an interrupted process may have performed effects. Explicit recovery adopts
/// the same workflow run, whose attempt journal decides whether execution is uncertain.
const SCHEMA:&str="CREATE TABLE pipeline_binding(singleton INTEGER PRIMARY KEY CHECK(singleton=1),native_repository TEXT NOT NULL,provider TEXT NOT NULL,host TEXT NOT NULL,provider_repository_id INTEGER NOT NULL CHECK(provider_repository_id>0)) STRICT;
            CREATE TABLE pipeline_rules(rule_id TEXT PRIMARY KEY,repository TEXT NOT NULL,generation INTEGER NOT NULL CHECK(generation>0),body TEXT NOT NULL) STRICT;
            CREATE TABLE pipeline_events(semantic_key TEXT PRIMARY KEY,body TEXT NOT NULL) STRICT;
            CREATE TABLE pipeline_observations(semantic_key TEXT PRIMARY KEY,event_key TEXT NOT NULL REFERENCES pipeline_events(semantic_key),rule_id TEXT NOT NULL REFERENCES pipeline_rules(rule_id),rule_generation INTEGER NOT NULL CHECK(rule_generation>0),reasons TEXT NOT NULL) STRICT;
            CREATE TABLE pipeline_polls(repository TEXT PRIMARY KEY,observed_at INTEGER NOT NULL CHECK(observed_at>=0),unread_pages INTEGER NOT NULL CHECK(unread_pages>=0)) STRICT;
            CREATE TABLE pipeline_dispatches(dispatch_id TEXT PRIMARY KEY,run_id TEXT NOT NULL UNIQUE,semantic_key TEXT NOT NULL UNIQUE REFERENCES pipeline_observations(semantic_key),event_key TEXT NOT NULL REFERENCES pipeline_events(semantic_key),rule_id TEXT NOT NULL REFERENCES pipeline_rules(rule_id),body TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('pending','claimed','complete','blocked')),claim_generation INTEGER NOT NULL DEFAULT 0 CHECK(claim_generation>=0),error TEXT) STRICT;";
const APPLICATION_ID: i64 = 0x4153_5052;
const SCHEMA_VERSION: i64 = 1;
pub struct RuleJournal {
    db: Connection,
    native_repository: String,
    _access: crate::local_recovery::AccessGuard,
}
impl RuleJournal {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| Error::Invalid("pipeline journal needs operator root".into()))?;
        if path.file_name() != Some(std::ffi::OsStr::new("state.sqlite"))
            || directory.file_name() != Some(std::ffi::OsStr::new("pipeline-rules"))
        {
            return Err(Error::Invalid(
                "pipeline journal must live at pipeline-rules/state.sqlite".into(),
            ));
        }
        let root = directory
            .parent()
            .ok_or_else(|| Error::Invalid("pipeline journal needs native root".into()))?;
        let access = crate::local_recovery::AccessGuard::shared_existing(root)?;
        let native_repository =
            crate::authority::Authority::open_read_only(root)?.repository_id()?;
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if app == 0 && version == 0 && tables == 0 {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
        validate_recovery(&tx, &native_repository)?;
        tx.commit()?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        Ok(Self {
            db,
            native_repository,
            _access: access,
        })
    }
    /// Reserve one stable forge/native binding before any source import. A failed first
    /// installation deliberately leaves this empty root bound; it is never rebound silently.
    pub fn bind_repository(&mut self, rule: &PipelineRule) -> Result<()> {
        validate_rule(rule)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO pipeline_binding VALUES(1,?1,?2,?3,?4)",
            params![
                self.native_repository,
                rule.provider,
                rule.host,
                rule.provider_repository_id
            ],
        )?;
        ensure_binding(
            &tx,
            &self.native_repository,
            &rule.provider,
            &rule.host,
            rule.provider_repository_id,
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn install(&mut self, rule: &PipelineRule, expected_generation: Option<i64>) -> Result<()> {
        validate_rule(rule)?;
        ensure_binding(
            &self.db,
            &self.native_repository,
            &rule.provider,
            &rule.host,
            rule.provider_repository_id,
        )?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT body FROM pipeline_rules WHERE rule_id=?1",
                params![rule.rule_id],
                |r| r.get(0),
            )
            .optional()?;
        let old: Option<PipelineRule> = old.map(|v| serde_json::from_str(&v)).transpose()?;
        if old.as_ref().map(|v| v.generation) != expected_generation
            || rule.generation
                != expected_generation
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| Error::Limit("rule generation exhausted".into()))?
        {
            return Err(Error::Conflict("pipeline rule generation conflict".into()));
        }
        if old.as_ref().is_some_and(|v| {
            v.provider != rule.provider
                || v.host != rule.host
                || v.provider_repository_id != rule.provider_repository_id
        }) {
            return Err(Error::Conflict(
                "stable rule cannot change repository identity".into(),
            ));
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM pipeline_rules", [], |r| r.get(0))?;
        if old.is_none() && count >= 256 {
            return Err(Error::Limit("local rule limit is 256".into()));
        }
        tx.execute("INSERT INTO pipeline_rules(repository,rule_id,generation,body) VALUES(?1,?2,?3,?4)
          ON CONFLICT(rule_id) DO UPDATE SET repository=excluded.repository,generation=excluded.generation,body=excluded.body",
          params![rule.repository,rule.rule_id,rule.generation,serde_json::to_string(rule)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn assert_repository(&self, provider: &str, host: &str, id: i64) -> Result<()> {
        ensure_binding(&self.db, &self.native_repository, provider, host, id)
    }
    /// A retained observation checkpoint, not a claim of complete provider history.
    pub fn record_poll(
        &mut self,
        identity: &str,
        observed_at: i64,
        unread_pages: u32,
    ) -> Result<()> {
        let (provider, host, id): (String, String, i64) = self.db.query_row(
            "SELECT provider,host,provider_repository_id FROM pipeline_binding WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if observed_at < 0
            || !identity.starts_with(&format!("{provider}://{host}/"))
            || !identity.ends_with(&format!("#{id}"))
        {
            return Err(Error::Invalid(
                "pipeline poll belongs to another repository".into(),
            ));
        }
        self.db.execute("INSERT INTO pipeline_polls(repository,observed_at,unread_pages) VALUES(?1,?2,?3) ON CONFLICT(repository) DO UPDATE SET observed_at=excluded.observed_at,unread_pages=excluded.unread_pages", params![identity,observed_at,unread_pages])?;
        Ok(())
    }
    fn confirmed_hint(
        &self,
        provider: &str,
        repository: &ConnectedRepository,
        number: i64,
        commit: &str,
    ) -> Result<bool> {
        self.assert_repository(provider, &repository.host, repository.repository_id)?;
        let key = [
            provider,
            repository.host.as_str(),
            &repository.repository_id.to_string(),
            &number.to_string(),
            commit,
        ]
        .join("\0");
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pipeline_events WHERE semantic_key=?1)",
            [key],
            |r| r.get(0),
        )?)
    }
    pub fn polls(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.db.prepare(
            "SELECT repository,observed_at,unread_pages FROM pipeline_polls ORDER BY repository",
        )?;
        Ok(stmt.query_map([],|r|Ok(serde_json::json!({"repository":r.get::<_,String>(0)?,"observed_at":r.get::<_,i64>(1)?,"unread_pages":r.get::<_,u32>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?)
    }
    pub fn rules(&self) -> Result<Vec<PipelineRule>> {
        let mut stmt = self
            .db
            .prepare("SELECT body FROM pipeline_rules ORDER BY repository,rule_id")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|v| Ok(serde_json::from_str(&v)?))
            .collect()
    }
    pub fn rule(&self, repository: &str, id: &str) -> Result<PipelineRule> {
        self.rules()?
            .into_iter()
            .find(|r| r.repository == repository && r.rule_id == id)
            .ok_or_else(|| Error::NotFound("pipeline rule".into()))
    }
    pub fn accept(&mut self, event: &ConfirmedMerge) -> Result<Vec<PendingDispatch>> {
        validate_event(event)?;
        ensure_binding(
            &self.db,
            &self.native_repository,
            &event.provider,
            &event.host,
            event.provider_repository_id,
        )?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stmt =
            tx.prepare("SELECT body FROM pipeline_rules WHERE repository=?1 ORDER BY rule_id")?;
        let rows = stmt
            .query_map([&event.repository], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let event_body = serde_json::to_string(event)?;
        tx.execute(
            "INSERT OR IGNORE INTO pipeline_events(semantic_key,body) VALUES(?1,?2)",
            params![event_key(event), event_body],
        )?;
        let stored: String = tx.query_row(
            "SELECT body FROM pipeline_events WHERE semantic_key=?1",
            [event_key(event)],
            |r| r.get(0),
        )?;
        let event: ConfirmedMerge = serde_json::from_str(&stored)?;
        let event = &event;
        let mut accepted = Vec::new();
        for body in rows {
            let rule: PipelineRule = serde_json::from_str(&body)?;
            let matched = preview(&rule, event);
            // A different repository is rejected before any observation is retained.
            if rule.provider != event.provider
                || rule.host != event.host
                || rule.provider_repository_id != event.provider_repository_id
            {
                continue;
            }
            let semantic = semantic_key(event, &rule.rule_id);
            let first = tx.execute("INSERT OR IGNORE INTO pipeline_observations(semantic_key,event_key,rule_id,rule_generation,reasons) VALUES(?1,?2,?3,?4,?5)",
                params![semantic,event_key(event),rule.rule_id,rule.generation,serde_json::to_string(&matched.reasons)?])?;
            if first == 0 || !matched.matches {
                continue;
            }
            let dispatch = PendingDispatch {
                dispatch_id: format!("pds_{}", &digest(b"dispatch", &semantic)[..32]),
                run_id: format!("pipeline-{}", &digest(b"run", &semantic)[..32]),
                event: event.clone(),
                rule,
            };
            tx.execute(
                "INSERT OR IGNORE INTO pipeline_events(semantic_key,body) VALUES(?1,?2)",
                params![event_key(event), serde_json::to_string(event)?],
            )?;
            let changed = tx.execute("INSERT OR IGNORE INTO pipeline_dispatches(dispatch_id,run_id,semantic_key,event_key,rule_id,body,state)
              VALUES(?1,?2,?3,?4,?5,?6,'pending')", params![dispatch.dispatch_id,dispatch.run_id,semantic,event_key(event),dispatch.rule.rule_id,serde_json::to_string(&dispatch)?])?;
            if changed == 1 {
                accepted.push(dispatch);
            }
        }
        tx.commit()?;
        Ok(accepted)
    }
    pub fn dispatches(&self, limit: usize) -> Result<Vec<DispatchView>> {
        self.dispatches_page(limit, None)
    }
    pub fn dispatches_page(&self, limit: usize, before: Option<i64>) -> Result<Vec<DispatchView>> {
        let mut stmt=self.db.prepare("SELECT rowid,body,state,claim_generation,error FROM pipeline_dispatches WHERE (?1 IS NULL OR rowid<?1) ORDER BY rowid DESC LIMIT ?2")?;
        let rows = stmt
            .query_map(params![before, limit.min(256) as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(sequence, body, state, claim_generation, reason)| {
                Ok(DispatchView {
                    sequence,
                    dispatch: serde_json::from_str(&body)?,
                    state,
                    claim_generation,
                    reason,
                })
            })
            .collect()
    }
    pub fn dispatch(&self, id: &str) -> Result<DispatchView> {
        let row=self.db.query_row("SELECT rowid,body,state,claim_generation,error FROM pipeline_dispatches WHERE dispatch_id=?1",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((sequence, body, state, claim_generation, reason)) = row else {
            return Err(Error::NotFound("pipeline dispatch".into()));
        };
        Ok(DispatchView {
            sequence,
            dispatch: serde_json::from_str(&body)?,
            state,
            claim_generation,
            reason,
        })
    }
    pub fn pending(&self, limit: usize) -> Result<Vec<PendingDispatch>> {
        let mut stmt = self.db.prepare(
            "SELECT body FROM pipeline_dispatches WHERE state='pending' ORDER BY rowid LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit.min(256) as i64], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|body| Ok(serde_json::from_str(&body)?))
            .collect()
    }
    pub fn claim(&mut self, id: &str) -> Result<DispatchClaim> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE pipeline_dispatches SET state='claimed',claim_generation=claim_generation+1,error=NULL WHERE dispatch_id=?1 AND state='pending'",[id])?!=1 {
            return Err(Error::Conflict("dispatch is not pending; explicit generation-fenced recovery required".into()));
        }
        let (body, generation): (String, i64) = tx.query_row(
            "SELECT body,claim_generation FROM pipeline_dispatches WHERE dispatch_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        Ok(DispatchClaim {
            dispatch: serde_json::from_str(&body)?,
            generation,
        })
    }
    pub fn finish(
        &mut self,
        claim: &DispatchClaim,
        state: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        if !matches!(state, "pending" | "complete" | "blocked") {
            return Err(Error::Invalid("invalid outbox outcome".into()));
        }
        if self.db.execute("UPDATE pipeline_dispatches SET state=?1,error=?2 WHERE dispatch_id=?3 AND state='claimed' AND claim_generation=?4",
            params![state,reason.map(|v|v.chars().take(1024).collect::<String>()),claim.dispatch.dispatch_id,claim.generation])?!=1 {
            return Err(Error::Conflict("dispatch claim generation changed".into()));
        }
        Ok(())
    }
    /// Caller must hold the local runner lease; a live owner is never displaced.
    pub fn recover(&mut self, id: &str, expected_generation: i64) -> Result<()> {
        if self.db.execute("UPDATE pipeline_dispatches SET state='pending',claim_generation=claim_generation+1,error='explicit recovery; reuse retained workflow run' WHERE dispatch_id=?1 AND state IN ('claimed','blocked') AND claim_generation=?2",params![id,expected_generation])?!=1 {
            return Err(Error::Conflict("dispatch recovery generation conflict".into()));
        }
        Ok(())
    }
}

fn event_key(event: &ConfirmedMerge) -> String {
    [
        event.provider.as_str(),
        event.host.as_str(),
        &event.provider_repository_id.to_string(),
        &event.proposal_number.to_string(),
        event.accepted_commit.as_str(),
    ]
    .join("\0")
}
fn semantic_key(event: &ConfirmedMerge, rule_id: &str) -> String {
    format!("{}\0{rule_id}", event_key(event))
}
fn digest(domain: &[u8], value: &str) -> String {
    let mut h = Sha256::new();
    h.update(domain);
    h.update([0]);
    h.update(value);
    format!("{:x}", h.finalize())
}
fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn validate_rule(r: &PipelineRule) -> Result<()> {
    if r.repository.is_empty()
        || r.repository.len() > 512
        || r.rule_id.is_empty()
        || r.rule_id.len() > 64
        || !r
            .rule_id
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
        || r.generation < 1
        || !matches!(r.provider.as_str(), "github" | "gitlab")
        || r.provider_repository_id < 1
        || r.host.is_empty()
        || r.trigger.target.is_empty()
        || r.trigger.labels_all.len() > 64
        || r.trigger.labels_all.iter().any(|v| v.len() > 256)
        || r.recipe.digest()? != r.recipe_digest
        || r.skill_digest.is_empty()
        || r.profile_digest.len() != 64
        || r.profile_id.is_empty()
        || !r.profile_path.is_absolute()
        || !r.git.is_absolute()
        || r.git_identity.path != r.git
        || r.git_identity.sha256.len() != 64
        || !r
            .git_identity
            .sha256
            .bytes()
            .all(|v| v.is_ascii_hexdigit() && !v.is_ascii_uppercase())
        || r.git_identity.size > 512 * 1024 * 1024
        || r.git_identity.mode & 0o111 == 0
        || r.git_identity.mode & 0o022 != 0
        || !r.git_repository.is_absolute()
        || r.authority_ceiling != "productive_local"
        || r.recipe.steps.iter().any(|s| {
            !matches!(
                s.action,
                StepAction::AgentAction { .. } | StepAction::SourcePaths { .. }
            )
        })
    {
        return Err(Error::Invalid(
            "invalid local pipeline rule or unsupported authority ceiling".into(),
        ));
    }
    Ok(())
}
fn validate_event(e: &ConfirmedMerge) -> Result<()> {
    if !matches!(e.provider.as_str(), "github" | "gitlab")
        || e.host.is_empty()
        || e.repository.is_empty()
        || e.provider_repository_id < 1
        || e.proposal_number == 0
        || !is_oid(&e.accepted_commit)
        || e.target.is_empty()
        || e.labels.len() > 256
        || e.labels.iter().any(|v| v.len() > 256)
        || e.delivery_id.as_ref().is_some_and(|v| v.len() > 512)
    {
        return Err(Error::Invalid("invalid confirmed merge".into()));
    }
    Ok(())
}

/// Compare only identity, without echoing a potentially authenticated URL.
fn origin_matches(remote: &str, host: &str, repository: &str) -> bool {
    if remote.contains(['\n', '\r', '\0']) {
        return false;
    }
    let identity = if let Ok(url) = reqwest::Url::parse(remote) {
        if !matches!(url.scheme(), "https" | "ssh")
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port().is_some()
            || (url.scheme() == "https" && !url.username().is_empty())
        {
            return false;
        }
        (
            url.host_str().unwrap_or("").to_owned(),
            url.path().trim_start_matches('/').to_owned(),
        )
    } else if let Some((authority, path)) = remote.split_once(':') {
        let host = authority.strip_prefix("git@").unwrap_or(authority);
        if host.contains(['@', '/', '\\']) {
            return false;
        }
        (host.to_owned(), path.to_owned())
    } else {
        return false;
    };
    identity.0 == host && identity.1.strip_suffix(".git").unwrap_or(&identity.1) == repository
}
#[cfg(unix)]
fn verify_origin(rule: &PipelineRule) -> Result<()> {
    use std::os::unix::process::CommandExt;
    crate::execution::executable::verify(&rule.git_identity, &[&rule.git_repository])?;
    let mut command = std::process::Command::new(&rule.git_identity.path);
    command
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(&rule.git_repository)
        .args([
            "--no-lazy-fetch",
            "--no-optional-locks",
            "-c",
            "core.hooksPath=/dev/null",
            "remote",
            "get-url",
            "--",
            "origin",
        ])
        .process_group(0);
    let out = crate::execution::process::capture_with_input(
        &mut command,
        None,
        Duration::from_secs(10),
        4096,
        &crate::execution::Cancellation::default(),
    )?;
    let remote = std::str::from_utf8(&out.stdout)
        .ok()
        .and_then(|v| v.strip_suffix('\n'));
    if out.stop.is_some()
        || out.code != Some(0)
        || remote.is_none_or(|v| !origin_matches(v, &rule.host, &rule.repository))
    {
        return Err(Error::Conflict(
            "local Git origin does not identify the installed forge repository".into(),
        ));
    }
    Ok(())
}

/// Resolve checkout/common metadata without running a repository-selected program.
pub fn executable_exclusions(data: &Path, source: &Path) -> Result<Vec<PathBuf>> {
    let source = source.canonicalize()?;
    let mut roots = vec![data.canonicalize()?];
    for parent in source.ancestors() {
        let control = parent.join(".git");
        let meta = match std::fs::symlink_metadata(&control) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let read_path = |path: &Path| -> Result<String> {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(4097)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(Error::Limit("Git metadata pointer exceeds bound".into()));
            }
            String::from_utf8(bytes)
                .map_err(|_| Error::Invalid("Git metadata pointer is not UTF-8".into()))
        };
        let git_dir = if meta.is_dir() && !meta.file_type().is_symlink() {
            control.canonicalize()?
        } else if meta.is_file() && !meta.file_type().is_symlink() {
            let text = read_path(&control)?;
            let path = text
                .trim_end_matches(['\r', '\n'])
                .strip_prefix("gitdir: ")
                .ok_or_else(|| Error::Invalid("unsupported Git pointer".into()))?;
            parent.join(path).canonicalize()?
        } else {
            return Err(Error::Invalid("unsupported Git metadata path".into()));
        };
        roots.push(parent.to_owned());
        roots.push(git_dir.clone());
        let common = git_dir.join("commondir");
        if common.try_exists()? {
            let text = read_path(&common)?;
            roots.push(
                git_dir
                    .join(text.trim_end_matches(['\r', '\n']))
                    .canonicalize()?,
            );
        }
        return Ok(roots);
    }
    Err(Error::Invalid(
        "pipeline source requires an existing Git checkout".into(),
    ))
}

/// Resolve only locally available exact Git objects; never fetch or alter a checkout.
/// The caller owns this operator data directory and its source connection.
#[cfg(unix)]
pub fn compile_source(
    data_dir: &Path,
    rule: &PipelineRule,
    commit: &str,
) -> Result<skill_workflow::CompiledPlan> {
    validate_rule(rule)?;
    let exclusions = executable_exclusions(data_dir, &rule.git_repository)?;
    let exclusions: Vec<&Path> = exclusions.iter().map(PathBuf::as_path).collect();
    crate::execution::executable::verify(&rule.git_identity, &exclusions)?;
    read_profile(rule)?.executable_identities(&exclusions)?;
    let git_repository = rule.git_repository.canonicalize()?;
    let data = data_dir.canonicalize()?;
    let journal = RuleJournal::open(&data.join("pipeline-rules/state.sqlite"))?;
    ensure_binding(
        &journal.db,
        &journal.native_repository,
        &rule.provider,
        &rule.host,
        rule.provider_repository_id,
    )?;
    if data.starts_with(&git_repository) || git_repository.starts_with(&data) {
        return Err(Error::Invalid(
            "pipeline state and Git checkout must be separate non-nested directories".into(),
        ));
    }
    let request = crate::git_snapshot::SnapshotRequest {
        git: rule.git.clone(),
        repository: git_repository,
        revision: commit.into(),
        exclude: vec![],
    };
    // Resolve the actual top-level/common Git directory before writing native objects.
    let report = crate::git_snapshot::preflight(&request)?;
    for source in [Path::new(&report.repository), Path::new(&report.git_dir)] {
        let source = source.canonicalize()?;
        if data.starts_with(&source) || source.starts_with(&data) {
            return Err(Error::Invalid(
                "pipeline state and Git directories must be separate".into(),
            ));
        }
    }
    let toplevel = Path::new(&report.repository).canonicalize()?;
    if rule.profile_path.canonicalize()?.starts_with(&toplevel) {
        return Err(Error::Invalid(
            "operator profile must be outside the actual source checkout".into(),
        ));
    }
    crate::execution::executable::verify(
        &rule.git_identity,
        &[&data, &toplevel, Path::new(&report.git_dir)],
    )?;
    verify_origin(rule)?;
    let authority = crate::authority::Authority::open_read_only(&data)?;
    let repository = authority.repository_id()?;
    let engine = crate::store::Engine::open(&data)?;
    let (revision, _) = crate::git_snapshot::capture_immutable(&engine, &repository, &request)?;
    let source = WorkflowSource {
        repository,
        revision,
        policy_epoch: authority.policy_epoch()?,
    };
    skill_workflow::compile(&data, &rule.recipe.definition(source))
}

#[cfg(unix)]
pub fn read_profile_configuration(path: &Path) -> Result<skill_workflow::action::ActionProfile> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    // Open without following a final symlink or waiting on a substituted FIFO. Check
    // the opened descriptor before reading; a concurrent growth still hits take().
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Invalid(
            "profile must be a regular operator file".into(),
        ));
    }
    let canonical = path.canonicalize()?;
    // geteuid reads process metadata and takes no pointers.
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
        || canonical
            .ancestors()
            .any(|parent| std::fs::symlink_metadata(parent.join(".git")).is_ok())
    {
        return Err(Error::Invalid("action profile must be operator-owned, not writable by others, and outside Git checkouts".into()));
    }
    if metadata.len() > 64 * 1024 {
        return Err(Error::Limit("action profile exceeds 64 KiB".into()));
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Limit("action profile exceeds 64 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(unix)]
fn verify_profile_configuration(
    path: &Path,
    expected: &skill_workflow::action::ActionProfile,
) -> Result<()> {
    // Configuration is compared semantically, so whitespace/key order alone does not
    // revoke it. Executable content checks belong to dispatch boundaries, not polling.
    if read_profile_configuration(path)? != *expected {
        return Err(Error::Conflict(
            "installed profile revoked or changed".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub fn read_profile(rule: &PipelineRule) -> Result<skill_workflow::action::ActionProfile> {
    let profile = read_profile_configuration(&rule.profile_path)?;
    if profile.id != rule.profile_id || profile.digest()? != rule.profile_digest {
        return Err(Error::Conflict(
            "installed profile revoked or changed".into(),
        ));
    }
    Ok(profile)
}

/// Deliver one generation-fenced claim via the existing workflow start/attempt journal.
#[cfg(unix)]
pub fn deliver(
    data_dir: &Path,
    journal: &mut RuleJournal,
    claim: &DispatchClaim,
    work_dir: &Path,
) -> Result<skill_workflow::RunView> {
    let cancellation = crate::execution::Cancellation::default();
    deliver_with_cancellation(data_dir, journal, claim, work_dir, &cancellation)
}
#[cfg(unix)]
pub fn deliver_with_cancellation(
    data_dir: &Path,
    journal: &mut RuleJournal,
    claim: &DispatchClaim,
    work_dir: &Path,
    cancellation: &crate::execution::Cancellation,
) -> Result<skill_workflow::RunView> {
    use skill_workflow::runtime;
    ensure_binding(
        &journal.db,
        &journal.native_repository,
        &claim.dispatch.rule.provider,
        &claim.dispatch.rule.host,
        claim.dispatch.rule.provider_repository_id,
    )?;
    let result = (|| {
        let current = journal.rule(
            &claim.dispatch.rule.repository,
            &claim.dispatch.rule.rule_id,
        )?;
        if !current.enabled {
            return Err(Error::Conflict("rule is disabled".into()));
        }
        let rule = &claim.dispatch.rule;
        let profile = read_profile(rule)?;
        // A restarted delivery adopts the retained plan, even if policy or source later changed.
        // advance_with_actions revalidates that plan and never retries an interrupted attempt.
        let existing = data_dir.join("skill-workflows/state.sqlite").exists();
        let view = if existing {
            match runtime::status(data_dir, &claim.dispatch.run_id) {
                Ok(view) => Some(view),
                Err(Error::NotFound(_)) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        if view.is_none() {
            let plan = compile_source(data_dir, rule, &claim.dispatch.event.accepted_commit)?;
            if plan.skill_digest() != rule.skill_digest {
                return Err(Error::Conflict(
                    "pinned recipe resources changed at accepted commit".into(),
                ));
            }
            runtime::start(data_dir, &claim.dispatch.run_id, &plan)?;
        }
        let event = &claim.dispatch.event;
        let inputs = std::collections::BTreeMap::from([
            ("accepted_commit".into(), event.accepted_commit.clone()),
            ("proposal_number".into(), event.proposal_number.to_string()),
            ("target".into(), event.target.clone()),
            ("labels".into(), serde_json::to_string(&event.labels)?),
            ("repository".into(), event.repository.clone()),
            ("provider".into(), event.provider.clone()),
            ("host".into(), event.host.clone()),
            ("observed_at".into(), event.observed_at.to_string()),
        ]);
        runtime::bind_inputs(data_dir, &claim.dispatch.run_id, &inputs)?;

        let db_path = journal
            .db
            .path()
            .ok_or_else(|| Error::Invalid("delivery needs a durable rule journal".into()))?
            .to_owned();
        let native_repository = journal.native_repository.clone();
        let guard = || -> Result<()> {
            let db =
                Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(Duration::from_secs(5))?;
            ensure_binding(
                &db,
                &native_repository,
                &rule.provider,
                &rule.host,
                rule.provider_repository_id,
            )?;
            let body: String = db.query_row(
                "SELECT body FROM pipeline_rules WHERE repository=?1 AND rule_id=?2",
                params![rule.repository, rule.rule_id],
                |r| r.get(0),
            )?;
            let current: PipelineRule = serde_json::from_str(&body)?;
            if !current.enabled {
                return Err(Error::Conflict("rule disabled during delivery".into()));
            }
            let owns:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pipeline_dispatches WHERE dispatch_id=?1 AND state='claimed' AND claim_generation=?2)",params![claim.dispatch.dispatch_id,claim.generation],|r|r.get(0))?;
            if !owns {
                return Err(Error::Conflict(
                    "dispatch claim changed during delivery".into(),
                ));
            }
            verify_profile_configuration(&rule.profile_path, &profile)?;
            Ok(())
        };
        let view = runtime::advance_actions_guarded(
            data_dir,
            &claim.dispatch.run_id,
            32,
            &skill_workflow::action::ActionExecutor {
                profile: &profile,
                work_dir,
                cancellation,
            },
            &guard,
        )?;
        guard()?;
        Ok(view)
    })();
    match result {
        Ok(view) => {
            let succeeded = view.state == skill_workflow::RunState::Succeeded;
            let reason = if succeeded {
                None
            } else {
                Some(format!(
                    "workflow outcome {}; inspect retained run before recovery; reserved attempts are never replayed",
                    serde_json::to_value(view.state)?
                ))
            };
            journal.finish(
                claim,
                if succeeded { "complete" } else { "blocked" },
                reason.as_deref(),
            )?;
            Ok(view)
        }
        Err(error) => {
            journal.finish(claim, "blocked", Some(&error.to_string()))?;
            Err(error)
        }
    }
}

fn ensure_binding(
    db: &Connection,
    native: &str,
    provider: &str,
    host: &str,
    id: i64,
) -> Result<()> {
    let bound:Option<(String,String,String,i64)>=db.query_row("SELECT native_repository,provider,host,provider_repository_id FROM pipeline_binding WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if bound != Some((native.into(), provider.into(), host.into(), id)) {
        return Err(Error::Conflict(
            "pipeline root is unbound or belongs to a different forge repository".into(),
        ));
    }
    Ok(())
}

/// Read-only typed validation, including frozen payload and semantic/FK bindings.
/// Recovery must call this without opening runtime services or probing executables.
pub(crate) fn validate_recovery(db: &Connection, native: &str) -> Result<()> {
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(Error::Invalid(
            "unsupported pipeline journal identity/schema".into(),
        ));
    }
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" || db.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(Error::Invalid(
            "pipeline journal integrity/foreign-key check failed".into(),
        ));
    }
    let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for ddl in SCHEMA.split(';').map(str::trim).filter(|v| !v.is_empty()) {
        let name = ddl
            .strip_prefix("CREATE TABLE ")
            .and_then(|v| v.split('(').next())
            .ok_or_else(|| Error::Invalid("pipeline schema declaration".into()))?;
        let sql: String = db.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )?;
        if normalize(ddl) != normalize(&sql) {
            return Err(Error::Invalid("unsupported pipeline table shape".into()));
        }
    }
    let count: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if count != 6 {
        return Err(Error::Invalid(
            "unsupported extra pipeline relations".into(),
        ));
    }
    let bound:Option<(String,String,String,i64)>=db.query_row("SELECT native_repository,provider,host,provider_repository_id FROM pipeline_binding WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let mut bytes = 0i64;
    for (table, limit) in [
        ("pipeline_rules", 256),
        ("pipeline_events", 100_000),
        ("pipeline_observations", 100_000),
        ("pipeline_dispatches", 100_000),
        ("pipeline_polls", 256),
    ] {
        let count: i64 =
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        if count > limit || (bound.is_none() && count != 0) {
            return Err(Error::Invalid(
                "unbound or oversized pipeline journal".into(),
            ));
        }
    }
    let Some((repository, provider, host, id)) = bound else {
        return Ok(());
    };
    if repository != native
        || !matches!(provider.as_str(), "github" | "gitlab")
        || host.is_empty()
        || id < 1
    {
        return Err(Error::Invalid(
            "pipeline native/forge binding differs".into(),
        ));
    }
    for (table, column, limit) in [
        ("pipeline_rules", "body", 262144),
        ("pipeline_events", "body", 65536),
        ("pipeline_observations", "reasons", 32768),
        ("pipeline_dispatches", "body", 524288),
    ] {
        let (max,sum):(i64,i64)=db.query_row(&format!("SELECT coalesce(max(length(CAST({column} AS BLOB))),0),coalesce(sum(length(CAST({column} AS BLOB))),0) FROM {table}"),[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        bytes = bytes
            .checked_add(sum)
            .ok_or_else(|| Error::Limit("pipeline payload overflow".into()))?;
        if max > limit || bytes > 128 * 1024 * 1024 {
            return Err(Error::Limit(
                "pipeline recovery payload budget exceeded".into(),
            ));
        }
    }
    let mut rules = std::collections::BTreeMap::new();
    let mut stmt = db.prepare("SELECT repository,rule_id,generation,body FROM pipeline_rules")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (name, rule_id, generation, body) = row?;
        let rule: PipelineRule = serde_json::from_str(&body)?;
        validate_rule(&rule)?;
        ensure_binding(
            db,
            native,
            &rule.provider,
            &rule.host,
            rule.provider_repository_id,
        )?;
        if rule.repository != name || rule.rule_id != rule_id || rule.generation != generation {
            return Err(Error::Invalid(
                "pipeline rule payload binding differs".into(),
            ));
        }
        rules.insert(rule_id, rule);
    }
    let mut events = std::collections::BTreeMap::new();
    let mut stmt = db.prepare("SELECT semantic_key,body FROM pipeline_events")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (key, body) = row?;
        let event: ConfirmedMerge = serde_json::from_str(&body)?;
        validate_event(&event)?;
        ensure_binding(
            db,
            native,
            &event.provider,
            &event.host,
            event.provider_repository_id,
        )?;
        if key != event_key(&event) {
            return Err(Error::Invalid(
                "pipeline event payload binding differs".into(),
            ));
        }
        events.insert(key, event);
    }
    let mut observations = std::collections::BTreeMap::new();
    let mut stmt = db.prepare(
        "SELECT semantic_key,event_key,rule_id,rule_generation,reasons FROM pipeline_observations",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })? {
        let (key, event_id, rule_id, generation, reasons) = row?;
        let rule = rules
            .get(&rule_id)
            .ok_or_else(|| Error::Invalid("pipeline observation rule missing".into()))?;
        let event = events
            .get(&event_id)
            .ok_or_else(|| Error::Invalid("pipeline observation event missing".into()))?;
        let reasons: Vec<String> = serde_json::from_str(&reasons)?;
        if key != semantic_key(event, &rule_id)
            || generation < 1
            || generation > rule.generation
            || reasons.len() > 4
        {
            return Err(Error::Invalid(
                "pipeline observation binding differs".into(),
            ));
        }
        observations.insert(key, (generation, reasons));
    }
    let mut stmt=db.prepare("SELECT dispatch_id,run_id,semantic_key,event_key,rule_id,body,state,claim_generation,error FROM pipeline_dispatches")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let d: PendingDispatch = serde_json::from_str(&row.get::<_, String>(5)?)?;
        validate_rule(&d.rule)?;
        validate_event(&d.event)?;
        ensure_binding(
            db,
            native,
            &d.rule.provider,
            &d.rule.host,
            d.rule.provider_repository_id,
        )?;
        let semantic = semantic_key(&d.event, &d.rule.rule_id);
        let observed = observations
            .get(&semantic)
            .ok_or_else(|| Error::Invalid("dispatch observation missing".into()))?;
        let state: String = row.get(6)?;
        let generation: i64 = row.get(7)?;
        let reason: Option<String> = row.get(8)?;
        if d.dispatch_id != format!("pds_{}", &digest(b"dispatch", &semantic)[..32])
            || d.run_id != format!("pipeline-{}", &digest(b"run", &semantic)[..32])
            || row.get::<_, String>(0)? != d.dispatch_id
            || row.get::<_, String>(1)? != d.run_id
            || row.get::<_, String>(2)? != semantic
            || row.get::<_, String>(3)? != event_key(&d.event)
            || row.get::<_, String>(4)? != d.rule.rule_id
            || events.get(&event_key(&d.event)) != Some(&d.event)
            || observed.0 != d.rule.generation
            || !observed.1.is_empty()
            || !preview(&d.rule, &d.event).matches
            || !matches!(
                state.as_str(),
                "pending" | "claimed" | "complete" | "blocked"
            )
            || generation < 0
            || (state != "pending" && generation == 0)
            || reason.as_ref().is_some_and(|v| v.len() > 4096)
        {
            return Err(Error::Invalid(
                "pipeline frozen dispatch/claim binding differs".into(),
            ));
        }
    }
    let prefix = format!("{provider}://{host}/");
    let suffix = format!("#{id}");
    let mut stmt = db.prepare("SELECT repository,observed_at,unread_pages FROM pipeline_polls")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (key, time, unread) = row?;
        if !key.starts_with(&prefix) || !key.ends_with(&suffix) || time < 0 || unread < 0 {
            return Err(Error::Invalid("pipeline poll binding differs".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::origin_matches;
    #[test]
    fn ssh_and_https_origins_match_only_the_exact_forge_repository() {
        for remote in [
            "https://forge.invalid/acme/widgets.git",
            "https://forge.invalid/acme/widgets",
            "git@forge.invalid:acme/widgets.git",
            "ssh://git@forge.invalid/acme/widgets.git",
        ] {
            assert!(origin_matches(remote, "forge.invalid", "acme/widgets"));
        }
        for remote in [
            "https://other.invalid/acme/widgets.git",
            "https://forge.invalid/other/widgets.git",
            "https://token@forge.invalid/acme/widgets.git",
            "https://forge.invalid/acme/widgets.git?x=1",
            "/local/acme/widgets",
            "git@forge.invalid:acme/widgets-evil.git",
        ] {
            assert!(!origin_matches(remote, "forge.invalid", "acme/widgets"));
        }
    }
    #[test]
    fn frequent_profile_guard_is_bounded_and_never_reads_executable_contents() {
        use super::{read_profile_configuration, verify_profile_configuration};
        use crate::skill_workflow::action::{ActionProfile, CommandProfile};
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("operator-command");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = ActionProfile {
            version: 1,
            id: "local".into(),
            provider: "claude_code".into(),
            claude_executable: None,
            model: None,
            commands: BTreeMap::from([(
                "command".into(),
                CommandProfile {
                    executable: executable.clone(),
                    args: vec![],
                },
            )]),
            timeout_ms: 1000,
            max_output_bytes: 1024,
        };
        let path = root.path().join("profile.json");
        std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
        let accepted = read_profile_configuration(&path).unwrap();
        // Sparse oversized contents make full identity capture fail immediately; the
        // frequent guard must still inspect only the bounded configuration file.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&executable)
            .unwrap()
            .set_len(512 * 1024 * 1024 + 1)
            .unwrap();
        assert!(accepted.digest().is_err());
        for _ in 0..100 {
            verify_profile_configuration(&path, &accepted).unwrap();
        }
        std::fs::remove_file(&executable).unwrap();
        verify_profile_configuration(&path, &accepted).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&profile).unwrap()).unwrap();
        verify_profile_configuration(&path, &accepted).unwrap();
        let mut changed = profile;
        changed.timeout_ms += 1;
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(verify_profile_configuration(&path, &accepted).is_err());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(64 * 1024 + 1)
            .unwrap();
        assert!(verify_profile_configuration(&path, &accepted).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(verify_profile_configuration(&path, &accepted).is_err());
        std::os::unix::fs::symlink(&executable, &path).unwrap();
        assert!(verify_profile_configuration(&path, &accepted).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(verify_profile_configuration(&path, &accepted).is_err());
    }
    #[test]
    fn operator_profile_rejects_other_writers_and_any_git_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("profile.json");
        std::fs::write(&path,r#"{"version":1,"id":"local","provider":"claude_code","claude_executable":null,"model":null,"commands":{},"timeout_ms":1000,"max_output_bytes":1024}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        super::read_profile_configuration(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(super::read_profile_configuration(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        assert!(super::read_profile_configuration(&path).is_err());
    }
}
