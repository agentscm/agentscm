//! Hosted local-validation controller. Signed archives, never the comment, decide statuses.
//! A durable CAS owns a whole status sequence; the summary is its final completion marker.
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use super::connection::{ConnectedRepository, Connection};
use super::github::{Api, CommitStatus, Gh, GhError, PullHead, StatusState};
use super::validation_evidence::{
    PublicationPhase, ValidationPublication, ValidationSnapshot, ValidationStatusSlot, read_bundle,
};
use super::{HubState, Store};
use crate::evidence::EvaluationClock;
use crate::local_validation::checks::{self, LoadedCheck};
use crate::local_validation::fetch::FetchContext;
use crate::local_validation::{self as validation, Observed, Verdict, VerdictState};
use crate::policy::issuers::IssuerRegistrySnapshot;

const CONFIG_DIR: &str = ".github/local-validation";
const MARKER: &str = "<!-- agentscm-validation-summary:v1 ";
const MAX_COMMENTS: u32 = 30;
const MAX_RECEIPTS: usize = 1000;
const MAX_VERIFICATION_BYTES: usize = 64 * 1024 * 1024;

fn failed(message: &str) -> GhError {
    GhError::Failed(message.into())
}
fn digest(bytes: impl AsRef<[u8]>) -> String {
    crate::store::digest(bytes.as_ref())
}
fn store_call<T>(action: impl FnOnce() -> super::Result<T>) -> Gh<T> {
    tokio::task::block_in_place(action).map_err(GhError::from)
}
fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().take(4096) {
        if c.is_ascii_alphanumeric() || c == ' ' {
            out.push(c);
        } else {
            write!(out, "&#x{:x};", c as u32).expect("string write");
        }
    }
    out
}

/// Presentation only. This model never serializes or modifies signed evidence.
#[derive(Debug)]
struct Row {
    requirement: String,
    state: VerdictState,
    reason: String,
    head: String,
    envelope: String,
    omissions: Vec<String>,
}
fn render(rows: &[Row], snapshot: &ValidationSnapshot, problem: Option<&str>) -> Gh<String> {
    let mut body = String::from(
        "## Local validation\n\nInformational summary; the Hub verifier publishes verification statuses. Branch protection remains operator configured. Signatures attest to worker assertions, not independently observed execution.\n\n",
    );
    if let Some(problem) = problem {
        writeln!(body, "Verification unavailable: {}.\n", escape(problem)).expect("string write");
    }
    body.push_str("| Requirement | Outcome / freshness | Validated commit | Omissions |\n| --- | --- | --- | --- |\n");
    for row in rows {
        writeln!(
            body,
            "| {} | {}: {} | {} | {} |",
            escape(&row.requirement),
            row.state.as_str(),
            escape(&row.reason),
            if row.envelope.is_empty() {
                "none for current head".into()
            } else {
                escape(&row.head)
            },
            escape(&row.omissions.join(", "))
        )
        .expect("string write");
    }
    body.push_str("\nAudit archives (received unverified; retained until explicit repository administration/removal):\n");
    writeln!(body, "{} submitted generations. Complete authorized history: `/v1/hub/repos/{{repository}}/validation-evidence/{{proposal}}?after=0&limit=100` (follow `next_after`). Latest archive references:", snapshot.generation).expect("string write");
    for receipt in snapshot.entries.iter().rev().take(20) {
        writeln!(
            body,
            "- generation {}: `{}`",
            receipt.generation, receipt.bundle_sha256
        )
        .expect("string write");
    }
    if body.len() > 58 * 1024 {
        return Err(failed("validation summary exceeds presentation limit"));
    }
    Ok(body)
}

struct Trusted {
    checks: Vec<LoadedCheck>,
    registry: IssuerRegistrySnapshot,
}
fn trusted(fetch: &FetchContext<'_>, commit: &str) -> crate::Result<Trusted> {
    // The filesystem loader's symlink checks inspect only this fresh private directory.
    // Every requested byte still comes from a regular Git blob at the pinned default head.
    let private = tempfile::tempdir()?;
    let read = |path: &Path| {
        let relative = path
            .strip_prefix(private.path())
            .map_err(|_| crate::Error::Invalid("configuration path escaped".into()))?;
        fetch.read_file(
            commit,
            &Path::new(CONFIG_DIR).join(relative),
            2 * 1024 * 1024,
        )
    };
    Ok(Trusted {
        checks: checks::load(private.path(), &read)?,
        registry: IssuerRegistrySnapshot::from_slice(&read(
            &private.path().join("registry.json"),
        )?)?,
    })
}
fn clock() -> EvaluationClock {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .map_or(EvaluationClock::Uncertain, |unix_ms| {
            EvaluationClock::Known { unix_ms }
        })
}
fn status_state(state: VerdictState) -> StatusState {
    match state {
        VerdictState::Success => StatusState::Success,
        VerdictState::Failure => StatusState::Failure,
        VerdictState::Error => StatusState::Error,
        VerdictState::Pending => StatusState::Pending,
    }
}
struct Plan {
    rows: Vec<Row>,
    statuses: Vec<(ValidationStatusSlot, StatusState, String)>,
}
fn evaluate(
    state: &HubState,
    repo: &ConnectedRepository,
    snapshot: &ValidationSnapshot,
    fetch: &FetchContext<'_>,
    commit: &str,
    head: &PullHead,
) -> Gh<Plan> {
    let trusted = trusted(fetch, commit)
        .map_err(|_| failed("trusted validation configuration unavailable"))?;
    let canonical = format!("github.com/{}", repo.full_name);
    if trusted.registry.repository != canonical
        || trusted
            .checks
            .iter()
            .any(|c| c.policy.repository != canonical)
    {
        return Err(failed("validation policy repository differs"));
    }
    let mut pairs = Vec::new();
    let mut bytes = 0_usize;
    for receipt in &snapshot.entries {
        let bundle = read_bundle(&*state.objects, receipt)
            .map_err(|_| failed("validation archive unavailable"))?;
        bundle
            .validate_repository(&canonical)
            .map_err(|_| failed("validation archive repository differs"))?;
        for pair in bundle.entries {
            let decoded = pair
                .decode()
                .map_err(|_| failed("validation archive invalid"))?;
            bytes = bytes
                .saturating_add(decoded.0.len())
                .saturating_add(decoded.1.len());
            if bytes > MAX_VERIFICATION_BYTES {
                return Err(failed(
                    "validation verification inventory exceeds byte limit",
                ));
            }
            pairs.push(decoded);
        }
    }
    let git = fetch.repository_path();
    let observed = validation::run::observe(
        git,
        &head.head_sha,
        &format!("refs/remotes/origin/{}", head.base_ref),
    )
    .map_err(|_| failed("validation Git source unavailable"))?;
    if observed.tree != head.head_tree || observed.base_tip != head.base_tip {
        return Err(failed("validation source moved during fetch"));
    }
    let trust = validation::run::trust_paths(CONFIG_DIR, validation::DEFAULT_VERIFIER_WORKFLOW);
    let mut plan = Plan {
        rows: Vec::new(),
        statuses: Vec::new(),
    };
    for check in trusted.checks {
        let required = validation::run::required_commands(
            git,
            &observed.merge_base,
            &head.head_sha,
            &check.recipe,
            &trust,
        )
        .map_err(|_| failed("required command inventory unavailable"))?;
        let selections = validation::run::command_selections(
            git,
            &observed.merge_base,
            &head.head_sha,
            &check.recipe,
            &trust,
        )
        .map_err(|_| failed("command selection unavailable"))?;
        let verdict = validation::select_pairs(
            pairs.iter().map(|(e, r)| (e, r)),
            &check.recipe,
            &trusted.registry,
            &check.policy,
            Observed {
                repository: &canonical,
                commit: &head.head_sha,
                tree: &observed.tree,
                base_ref: &format!("refs/heads/{}", head.base_ref),
                merge_base: &observed.merge_base,
                required: &required,
                selections: &selections,
            },
            clock(),
        );
        let omissions = selected_omissions(&pairs, &verdict);
        plan.statuses.push((
            ValidationStatusSlot {
                head: head.head_sha.clone(),
                context: check.policy.status_context(),
            },
            status_state(verdict.state),
            verdict.reason.into(),
        ));
        if let Some(groups) = &check.recipe.constituents {
            for group in groups {
                let result = verdict.constituents.iter().find(|v| v.id == group.id);
                plan.statuses.push((
                    ValidationStatusSlot {
                        head: head.head_sha.clone(),
                        context: check.policy.constituent_status_context(&group.id),
                    },
                    status_state(result.map_or(verdict.state, |v| v.state)),
                    result.map_or(verdict.reason, |v| v.reason).into(),
                ));
            }
        }
        plan.rows.push(Row {
            requirement: check.name.unwrap_or_else(|| "Local validation".into()),
            state: verdict.state,
            reason: format!("{}; {}", verdict.reason, verdict.detail),
            head: head.head_sha.clone(),
            envelope: if verdict.applies_to_head {
                verdict.envelope_sha256
            } else {
                String::new()
            },
            omissions,
        });
    }
    Ok(plan)
}
fn selected_omissions(pairs: &[(Vec<u8>, Vec<u8>)], verdict: &Verdict) -> Vec<String> {
    if !verdict.applies_to_head {
        return Vec::new();
    }
    pairs
        .iter()
        .find(|(e, _)| digest(e) == verdict.envelope_sha256)
        .and_then(|(e, _)| crate::evidence::EvidenceEnvelope::from_slice(e).ok())
        .map(|e| e.payload.coverage.omissions)
        .unwrap_or_default()
}

fn same_head(a: &PullHead, b: &PullHead) -> bool {
    a.open == b.open
        && a.head_sha == b.head_sha
        && a.base_ref == b.base_ref
        && a.base_tip == b.base_tip
        && a.head_tree == b.head_tree
        && a.base_repo_id == b.base_repo_id
        && a.head_repo_id == b.head_repo_id
}
fn marker(operation: &str) -> String {
    format!("{MARKER}{operation} -->\n")
}
fn cas(
    store: &dyn Store,
    repo: &str,
    proposal: i64,
    previous: Option<&ValidationPublication>,
    next: &ValidationPublication,
) -> Gh<bool> {
    store_call(|| {
        store.compare_validation_publication(
            repo,
            proposal,
            previous.map_or(0, |p| p.revision),
            next,
        )
    })
}

/// Called with the connected credential and Sync's repository mutex. The journal provides
/// exclusion across processes. There is deliberately no lease or automatic uncertain retry.
pub(super) async fn reconcile_repo(
    hub: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    only: Option<i64>,
) -> Gh<()> {
    let config = store_call(|| hub.store.validation_summary_config(&repo.id))?;
    if !config.is_some_and(|c| c.enabled && c.connection == conn.id) {
        return Ok(());
    }
    if conn.provider != "github"
        || conn.host != "github.com"
        || repo.host != "github.com"
        || !hub.sync.matches_forge(conn)
    {
        return Err(failed(
            "hosted local validation supports canonical GitHub only",
        ));
    }
    let api = hub.sync.api()?;
    let discovery = async {
        let numbers = if let Some(number) = only {
            vec![number]
        } else {
            let page = api
                .open_pulls(token, &repo.full_name, None)
                .await?
                .ok_or_else(|| failed("open pull inventory unavailable"))?;
            if page.unread != 0 {
                return Err(failed("open pull inventory incomplete"));
            }
            page.items
                .iter()
                .map(|p| {
                    p["number"]
                        .as_i64()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| failed("malformed pull inventory"))
                })
                .collect::<Gh<Vec<_>>>()?
        };
        let identity = if conn.kind == "app" {
            format!(
                "{}[bot]",
                conn.app_slug
                    .as_deref()
                    .ok_or_else(|| failed("connection bot identity unavailable"))?
            )
        } else {
            let (_, login) = api.user(token).await?;
            if conn.acts_as.as_deref() != Some(login.as_str()) {
                return Err(failed("connection identity changed"));
            }
            login
        };
        Ok::<_, GhError>((numbers, identity))
    }
    .await;
    let (numbers, identity) = match discovery {
        Ok(discovery) => discovery,
        Err(error) => {
            if !matches!(error, GhError::Auth(_) | GhError::Limited(_)) {
                invalidate_known(hub, conn, repo, token, api).await?;
            }
            return Err(error);
        }
    };
    let mut failure = None;
    for number in numbers {
        if let Err(error) = reconcile_pull(hub, conn, repo, token, api, &identity, number).await {
            if matches!(error, GhError::Auth(_) | GhError::Limited(_)) {
                return Err(error);
            }
            eprintln!("hub: validation pull {number}: {error:?}");
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// A failed forge inventory is not an empty repository. Discover prior journals
/// independently of the forge and invalidate only definite Idle owners.
async fn invalidate_known(
    hub: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &Api,
) -> Gh<()> {
    let (mut after, mut through) = (0, None);
    let mut failure = None;
    loop {
        let page = store_call(|| {
            hub.store
                .validation_publication_proposals(&repo.id, after, through, 100)
        })?;
        through = Some(page.through);
        if page.proposals.is_empty() {
            break;
        }
        for proposal in page.proposals {
            if proposal <= after || proposal > page.through {
                return Err(failed("validation journal inventory invalid"));
            }
            after = proposal;
            if let Err(error) = invalidate_idle(hub, conn, repo, token, api, proposal).await {
                if matches!(error, GhError::Auth(_) | GhError::Limited(_)) {
                    return Err(error);
                }
                failure.get_or_insert(error);
            }
        }
        if after == page.through {
            break;
        }
    }
    failure.map_or(Ok(()), Err)
}

async fn reconcile_pull(
    hub: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &Api,
    identity: &str,
    number: i64,
) -> Gh<()> {
    let result = reconcile_pull_inner(hub, conn, repo, token, api, identity, number).await;
    if result.is_err() {
        // Discovery/setup/read errors cannot preserve earlier green statuses. This
        // path only reserves Idle journals; an ambiguous Attempting write is untouched.
        invalidate_idle(hub, conn, repo, token, api, number).await?;
    }
    result
}

async fn invalidate_idle(
    hub: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &Api,
    number: i64,
) -> Gh<()> {
    let Some(old) = store_call(|| hub.store.validation_publication(&repo.id, number))? else {
        return Ok(());
    };
    if old.phase != PublicationPhase::Idle || old.connection != conn.id {
        return Ok(());
    }
    let fence = store_call(|| hub.store.validation_fence(&repo.id, number))?;
    let mut next = old.clone();
    next.revision += 1;
    next.phase = PublicationPhase::Preparing;
    next.evidence_generation = fence.generation;
    next.operation_id = digest(super::store::new_id("validation")?);
    // An unpublished failure body forces reevaluation even if the recovered source
    // would otherwise match the last visible comment after pending invalidations.
    next.body_sha256 = digest(format!("unavailable:{}", next.operation_id));
    if !cas(&*hub.store, &repo.id, number, Some(&old), &next)? {
        return Ok(());
    }
    let preparing = next.clone();
    next.revision += 1;
    next.phase = PublicationPhase::Attempting;
    if !cas(&*hub.store, &repo.id, number, Some(&preparing), &next)? {
        return Ok(());
    }
    abort_attempt(hub, api, token, repo, number, &next).await
}

async fn reconcile_pull_inner(
    hub: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &Api,
    identity: &str,
    number: i64,
) -> Gh<()> {
    use PublicationPhase::*;
    let old = store_call(|| hub.store.validation_publication(&repo.id, number))?;
    let comments = api
        .validation_comments(token, &repo.full_name, number, MAX_COMMENTS)
        .await?;
    if let Some(previous) = &old {
        if previous.connection != conn.id {
            return Err(failed("validation publisher connection changed"));
        }
        if previous.phase == Attempting {
            // Exact final marker proves that all preceding status replies returned. A missing
            // marker cannot distinguish an unsent request from an in-flight one.
            let matches: Vec<_> = comments
                .iter()
                .filter(|(_, author, body)| {
                    author == identity
                        && body.starts_with(&marker(&previous.operation_id))
                        && digest(&body[marker(&previous.operation_id).len()..])
                            == previous.body_sha256
                })
                .collect();
            if matches.len() != 1 || previous.comment_id.is_some_and(|id| id != matches[0].0) {
                return Err(failed(
                    "validation publication uncertain; operator recovery required",
                ));
            }
            let mut idle = previous.clone();
            idle.revision += 1;
            idle.phase = Idle;
            idle.comment_id = Some(matches[0].0);
            cas(&*hub.store, &repo.id, number, old.as_ref(), &idle)?;
            return Ok(());
        }
        if previous.phase == Preparing {
            // No remote write may occur in Preparing. Abort fences a crashed or slower worker.
            let mut idle = previous.clone();
            idle.revision += 1;
            idle.phase = Idle;
            cas(&*hub.store, &repo.id, number, old.as_ref(), &idle)?;
            return Ok(());
        }
    }
    // Never adopt a marker-only comment. A new journal cannot prove who created it.
    let owned: Vec<_> = comments
        .iter()
        .filter(|(_, author, body)| author == identity && body.starts_with(MARKER))
        .collect();
    if owned.len() > 1
        || owned
            .first()
            .is_some_and(|c| old.as_ref().and_then(|p| p.comment_id) != Some(c.0))
    {
        return Err(failed("untracked or duplicate owned validation summary"));
    }
    let mut canonical_id = old.as_ref().and_then(|p| p.comment_id);
    if let Some(id) = canonical_id
        && !comments.iter().any(|(n, a, _)| *n == id && a == identity)
    {
        match api.validation_comment(token, &repo.full_name, id).await? {
            None => canonical_id = None,
            Some(_) => return Err(failed("canonical summary discovery or ownership differs")),
        }
    }
    let full_snapshot = store_call(|| {
        hub.store
            .validation_snapshot(&repo.id, number, MAX_RECEIPTS)
    });
    let inventory_available = full_snapshot.is_ok();
    let snapshot = match full_snapshot {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let fence = store_call(|| hub.store.validation_fence(&repo.id, number))?;
            ValidationSnapshot {
                config: fence.config,
                generation: fence.generation,
                entries: Vec::new(),
            }
        }
    };
    let head = api
        .pull_head(token, &repo.full_name, number, &mut HashMap::new())
        .await?
        .ok_or_else(|| failed("pull unavailable"))?;
    if !head.open {
        return Ok(());
    }
    if head.base_repo_id != repo.repository_id as u64 {
        return Err(failed("validation base repository identity changed"));
    }
    let pulls = api
        .open_pulls(token, &repo.full_name, None)
        .await?
        .ok_or_else(|| failed("open pull inventory unavailable"))?;
    if pulls.unread != 0 {
        return Err(failed("open pull inventory incomplete"));
    }
    let shared_head = pulls
        .items
        .iter()
        .filter(|p| p["head"]["sha"].as_str() == Some(head.head_sha.as_str()))
        .count()
        != 1;
    let default = api.default_branch(token, &repo.full_name).await.ok();
    let fetch =
        tokio::task::block_in_place(|| FetchContext::hosted_github(&repo.full_name, token)).ok();
    let commit = fetch
        .as_ref()
        .zip(default.as_ref())
        .and_then(|(fetch, default)| {
            tokio::task::block_in_place(|| fetch.pin_default_branch(default)).ok()
        });
    let evaluated = tokio::task::block_in_place(|| {
        match (inventory_available, fetch.as_ref(), commit.as_ref()) {
            (true, Some(fetch), Some(commit)) => fetch
                .fetch_pull(&head.base_ref, number, &head.head_sha)
                .map_err(|_| failed("pull source unavailable"))
                .and_then(|()| evaluate(hub, repo, &snapshot, fetch, commit, &head)),
            _ => Err(failed("configuration, source or evidence unavailable")),
        }
    });
    let (mut plan, problem) = match evaluated {
        Ok(plan) => (plan, None),
        Err(_) => (
            Plan {
                rows: Vec::new(),
                statuses: Vec::new(),
            },
            Some("configuration, source or evidence unavailable"),
        ),
    };
    if shared_head {
        for (_, state, reason) in &mut plan.statuses {
            *state = StatusState::Pending;
            *reason = "head shared by multiple pull requests".into();
        }
        for row in &mut plan.rows {
            row.state = VerdictState::Pending;
            row.reason = "head shared by multiple pull requests".into();
            row.envelope.clear();
        }
    }
    let migration_pending = comments
        .iter()
        .any(|(_, _, body)| body.starts_with(validation::comment::MARKER_PREFIX));
    let mut body = render(&plan.rows, &snapshot, problem)?;
    if migration_pending {
        body.push_str("\nLegacy evidence comments remain: explicit archive migration and ownership-aware cleanup are pending.\n");
    }
    body = body
        .replace("{repository}", &repo.id)
        .replace("{proposal}", &number.to_string());
    let source = digest(format!(
        "{}:{}:{}:{}",
        commit.as_deref().unwrap_or("unavailable"),
        head.head_sha,
        head.base_tip,
        head.base_ref
    ));
    let body_digest = digest(&body);
    if old.as_ref().is_some_and(|p| {
        p.body_sha256 == body_digest
            && p.source_sha256 == source
            && p.evidence_generation == snapshot.generation
    }) && owned.first().is_some_and(|c| {
        c.2.strip_prefix(&marker(&old.as_ref().unwrap().operation_id)) == Some(body.as_str())
    }) {
        return Ok(());
    }
    let mut slots = old
        .as_ref()
        .map(|p| p.status_slots.clone())
        .unwrap_or_default();
    for (slot, _, _) in &plan.statuses {
        if !slots.contains(slot) {
            slots.push(slot.clone());
        }
    }
    if slots.len() > 512 {
        return Err(failed("retained validation status inventory exceeds limit"));
    }
    let mut next = ValidationPublication {
        revision: old.as_ref().map_or(1, |p| p.revision + 1),
        evidence_generation: snapshot.generation,
        connection: conn.id.clone(),
        phase: Preparing,
        operation_id: digest(super::store::new_id("validation")?.as_bytes()),
        body_sha256: body_digest,
        source_sha256: source,
        comment_id: canonical_id,
        status_slots: slots,
    };
    if !cas(&*hub.store, &repo.id, number, old.as_ref(), &next)? {
        return Ok(());
    }
    // All source/configuration/evidence reads precede ownership of remote sends.
    let current = api
        .pull_head(token, &repo.full_name, number, &mut HashMap::new())
        .await?
        .ok_or_else(|| failed("pull unavailable on recheck"))?;
    if !same_head(&head, &current)
        || (problem.is_none()
            && !configuration_current(api, token, repo, default.as_deref(), fetch.as_ref()).await?)
    {
        return Err(failed("validation source changed before publication"));
    }
    let preparing = next.clone();
    next.revision += 1;
    next.phase = Attempting;
    if !cas(&*hub.store, &repo.id, number, Some(&preparing), &next)? {
        return Ok(());
    }
    // Stop on every uncertain or refused reply. The nonce comment is always LAST.
    for slot in &next.status_slots {
        let status = CommitStatus::new(
            &slot.head,
            StatusState::Pending,
            &slot.context,
            "Hub validation rechecking",
        )?;
        api.post_commit_status(token, &repo.full_name, &status)
            .await?
            .map_err(|_| failed("validation invalidation refused"))?;
    }
    for (slot, result, reason) in &plan.statuses {
        // Changed evidence cannot release this Attempting owner; no other controller can
        // write until its final marker. Re-read source before each verdict.
        let current = async {
            let current = api
                .pull_head(token, &repo.full_name, number, &mut HashMap::new())
                .await?
                .ok_or_else(|| failed("pull unavailable during publication"))?;
            let now = store_call(|| {
                hub.store
                    .validation_snapshot(&repo.id, number, MAX_RECEIPTS)
            })?;
            let pulls = api
                .open_pulls(token, &repo.full_name, None)
                .await?
                .ok_or_else(|| failed("pull inventory unavailable during publication"))?;
            let shared_now = pulls
                .items
                .iter()
                .filter(|p| p["head"]["sha"].as_str() == Some(head.head_sha.as_str()))
                .count()
                != 1;
            Ok::<bool, GhError>(
                same_head(&head, &current)
                    && pulls.unread == 0
                    && shared_now == shared_head
                    && now == snapshot
                    && configuration_current(api, token, repo, default.as_deref(), fetch.as_ref())
                        .await?,
            )
        }
        .await;
        if !matches!(current, Ok(true)) {
            abort_attempt(hub, api, token, repo, number, &next).await?;
            return Err(failed(
                "validation changed during publication; statuses invalidated",
            ));
        }
        let status = CommitStatus::new(
            &slot.head,
            *result,
            &slot.context,
            &reason.chars().take(140).collect::<String>(),
        )?;
        api.post_commit_status(token, &repo.full_name, &status)
            .await?
            .map_err(|_| failed("validation verdict refused"))?;
    }
    let current = async {
        let current = api
            .pull_head(token, &repo.full_name, number, &mut HashMap::new())
            .await?
            .ok_or_else(|| failed("pull unavailable before summary"))?;
        let fence = store_call(|| hub.store.validation_fence(&repo.id, number))?;
        Ok::<bool, GhError>(
            same_head(&head, &current)
                && fence.generation == snapshot.generation
                && fence.config == snapshot.config
                && (problem.is_some()
                    || configuration_current(api, token, repo, default.as_deref(), fetch.as_ref())
                        .await?),
        )
    }
    .await;
    if !matches!(current, Ok(true)) {
        abort_attempt(hub, api, token, repo, number, &next).await?;
        return Err(failed(
            "validation changed before summary; statuses invalidated",
        ));
    }
    let rendered = format!("{}{body}", marker(&next.operation_id));
    let id = match next.comment_id {
        Some(id) => {
            if !api
                .edit_comment(token, &repo.full_name, id, &rendered)
                .await?
            {
                // Definite 404: all preceding sends returned. Release this operation,
                // then let the next complete discovery decide whether to recreate.
                let mut idle = next.clone();
                idle.revision += 1;
                idle.phase = Idle;
                cas(&*hub.store, &repo.id, number, Some(&next), &idle)?;
                return Ok(());
            }
            id
        }
        None => {
            api.create_comment(token, &repo.full_name, number, &rendered)
                .await?
        }
    };
    let mut idle = next.clone();
    idle.revision += 1;
    idle.phase = Idle;
    idle.comment_id = Some(id);
    if !cas(&*hub.store, &repo.id, number, Some(&next), &idle)? {
        return Err(failed("validation journal completion changed"));
    }
    Ok(())
}

/// All earlier mutations returned definite success. A failed source read or changed
/// source is not an ambiguous mutation: invalidate successes before releasing ownership.
async fn abort_attempt(
    hub: &HubState,
    api: &Api,
    token: &str,
    repo: &ConnectedRepository,
    number: i64,
    attempt: &ValidationPublication,
) -> Gh<()> {
    for slot in &attempt.status_slots {
        let status = CommitStatus::new(
            &slot.head,
            StatusState::Pending,
            &slot.context,
            "Hub validation source changed",
        )?;
        api.post_commit_status(token, &repo.full_name, &status)
            .await?
            .map_err(|_| failed("validation invalidation refused"))?;
    }
    let mut idle = attempt.clone();
    idle.revision += 1;
    idle.phase = PublicationPhase::Idle;
    cas(&*hub.store, &repo.id, number, Some(attempt), &idle)?;
    Ok(())
}

async fn configuration_current(
    api: &Api,
    token: &str,
    repo: &ConnectedRepository,
    default: Option<&str>,
    fetch: Option<&FetchContext<'_>>,
) -> Gh<bool> {
    let (Some(default), Some(fetch)) = (default, fetch) else {
        return Ok(false);
    };
    Ok(api.default_branch(token, &repo.full_name).await? == default
        && tokio::task::block_in_place(|| fetch.config_dir_unchanged(default, CONFIG_DIR))
            .map_err(|_| failed("configuration recheck unavailable"))?)
}

#[cfg(test)]
mod tests;
