use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::{json_value, resolve_work, work_scope_overlaps};
use crate::hub::Result;
use crate::hub::catchup::{
    CatchupMergeObservation, parse_source, repository_namespace, source_name,
};
use crate::hub::work::{WorkStart, WorkWho, scope_overlap_reasons};
use crate::hub::work_preflight::{
    WorkPreflightReason, WorkPreflightResult, checked, included, push_reason, status,
};

const MERGE_COLUMNS: &str = "id,seq,source,source_id,occurred_at,observed_at,title,branch,base,head_commit,merge_commit,pull_number,paths,paths_complete";
type MergeRow = (
    String,
    i64,
    String,
    String,
    Option<i64>,
    i64,
    Option<String>,
    String,
    String,
    String,
    String,
    i64,
    String,
    bool,
);

fn merge_row(row: &Row<'_>) -> rusqlite::Result<MergeRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn observation(row: MergeRow) -> Result<CatchupMergeObservation> {
    Ok(CatchupMergeObservation {
        id: row.0,
        seq: row.1,
        source: parse_source(&row.2)?,
        source_id: row.3,
        occurred_at: row.4,
        observed_at: row.5,
        title: row.6,
        branch: row.7,
        base: row.8,
        head_commit: row.9,
        merge_commit: row.10,
        pull_number: row.11,
        paths: json_value(&row.12)?,
        paths_complete: row.13,
    })
}

pub(super) fn evaluate(
    tx: &Transaction<'_>,
    who: &WorkWho,
    start: &WorkStart,
    scope: &[String],
    now: i64,
) -> Result<WorkPreflightResult> {
    let repository_key = who.repository_id.unwrap_or(-1);
    let repository = repository_namespace(&who.repository, who.repository_id);
    let (floor, current_merge_through): (i64, i64) = tx
        .query_row(
            "SELECT merge_floor_seq, next_merge_seq - 1 FROM hub_catchup_repositories
         WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_key=?4",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                repository,
                repository_key
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0));
    let subject_match = resolve_work(tx, who, start, now)?;
    let scope_overlaps = work_scope_overlaps(
        tx,
        who,
        scope,
        subject_match.as_ref().map(|item| item.id.as_str()),
        now,
    )?;
    let Some(request) = &start.preflight else {
        let mut reasons = vec![
            WorkPreflightReason::MissingSource,
            WorkPreflightReason::MissingHighwater,
        ];
        if scope.is_empty() {
            reasons.insert(0, WorkPreflightReason::MissingScope);
        }
        if !scope_overlaps.is_empty() {
            reasons.push(WorkPreflightReason::ActiveScopeOverlap);
        }
        return Ok(WorkPreflightResult {
            status: status(&reasons),
            source_revision: None,
            merge_after: None,
            merge_through: None,
            current_merge_through,
            source_relation_checked: false,
            subject_match,
            scope_overlaps,
            merges: Vec::new(),
            checked_observations: Vec::new(),
            reasons,
            complete: false,
        });
    };
    request.validate()?;

    let mut reasons = Vec::new();
    if let Some(repository_id) = who.repository_id {
        let mut statement=tx.prepare("SELECT id,scope,state,request_key,source_provider,source_host,source_issue
            FROM hub_work_items WHERE provider=?1 AND host=?2 AND repository_id=?3 AND repository<>?4")?;
        let aliases = statement
            .query_map(
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    repository_id,
                    who.repository
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let alias_relevant = aliases.into_iter().any(
            |(id, stored_scope, state, request_key, provider, host, issue)| {
                let exact = start.work_id.as_deref() == Some(id.as_str())
                    || start
                        .request_key
                        .as_ref()
                        .is_some_and(|value| request_key.as_ref() == Some(value))
                    || start.source.as_ref().is_some_and(|source| {
                        provider.as_ref() == Some(&source.provider)
                            && host.as_ref() == Some(&source.host)
                            && issue.as_ref() == Some(&source.issue)
                    });
                let overlap = state != "finished"
                    && json_value::<Vec<String>>(&stored_scope)
                        .is_ok_and(|stored| !scope_overlap_reasons(scope, &stored).is_empty());
                exact || overlap
            },
        );
        if alias_relevant {
            push_reason(&mut reasons, WorkPreflightReason::RepositoryIdentityRenamed);
        }
    }
    if scope.is_empty() {
        push_reason(&mut reasons, WorkPreflightReason::MissingScope);
    }
    if request.merge_after < floor {
        push_reason(&mut reasons, WorkPreflightReason::MergeHistoryGap);
    }
    if request.merge_through != current_merge_through {
        push_reason(&mut reasons, WorkPreflightReason::MergeHighwaterChanged);
    }
    if !scope_overlaps.is_empty() {
        push_reason(&mut reasons, WorkPreflightReason::ActiveScopeOverlap);
    }

    // A recent-window cursor is caller-selected. Resolve an exact retained source head across
    // this repository partition so an empty window cannot hide an already-merged source revision.
    let source_head_already_merged: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM hub_catchup_merges
         WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_key=?4
           AND head_commit=?5 LIMIT 1)",
        params![
            who.owner.identity.provider,
            who.owner.identity.host,
            repository,
            repository_key,
            request.source_revision
        ],
        |row| row.get(0),
    )?;
    if source_head_already_merged {
        push_reason(&mut reasons, WorkPreflightReason::SourceHeadAlreadyMerged);
    }

    // Exact checked identities are resolved independently of the caller's merge cursor. A caller
    // cannot hide an older already-merged source by choosing an empty recent window.
    let mut checked_observations = Vec::new();
    for candidate in &request.checked_merges {
        let row = tx
            .query_row(
                &format!(
                    "SELECT {MERGE_COLUMNS} FROM hub_catchup_merges
                WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_key=?4
                  AND source=?5 AND source_id=?6 AND pull_number=?7 AND merge_commit=?8"
                ),
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    repository,
                    repository_key,
                    source_name(candidate.source),
                    candidate.source_id,
                    candidate.pull_number,
                    candidate.merge_commit
                ],
                merge_row,
            )
            .optional()?;
        match row {
            Some(row) => {
                let observed = observation(row)?;
                if observed.head_commit == request.source_revision {
                    push_reason(&mut reasons, WorkPreflightReason::SourceHeadAlreadyMerged);
                }
                if !observed.paths_complete && !included(request, &observed) {
                    push_reason(&mut reasons, WorkPreflightReason::MergePathsIncomplete);
                } else if !scope_overlap_reasons(scope, &observed.paths).is_empty()
                    && request.source_revision != observed.merge_commit
                    && !candidate.in_source
                {
                    push_reason(&mut reasons, WorkPreflightReason::SourceMergeMissing);
                }
                checked_observations.push(observed);
            }
            None => push_reason(&mut reasons, WorkPreflightReason::CheckedMergeUnknown),
        }
    }
    let exact_source_anchor = checked_observations
        .iter()
        .any(|merge| merge.merge_commit == request.source_revision);
    if !request.source_relation_checked && !exact_source_anchor {
        push_reason(&mut reasons, WorkPreflightReason::SourceRelationUnknown);
    }

    let coverage: Vec<String> = {
        let mut statement = tx.prepare(
            "SELECT state FROM hub_catchup_sources
             WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_key=?4 ORDER BY source",
        )?;
        statement
            .query_map(
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    repository,
                    repository_key
                ],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if coverage.len() != 2 || coverage.iter().any(|state| state != "observed_only") {
        push_reason(&mut reasons, WorkPreflightReason::MergeCoverageUnknown);
    }

    let mut statement = tx.prepare(&format!(
        "SELECT {MERGE_COLUMNS} FROM hub_catchup_merges
         WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_key=?4
           AND seq>?5 AND seq<=?6 ORDER BY seq LIMIT 51"
    ))?;
    let mut merges = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                repository,
                repository_key,
                request.merge_after,
                current_merge_through
            ],
            merge_row,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(observation)
        .collect::<Result<Vec<_>>>()?;
    let window_complete = merges.len() <= 50;
    if !window_complete {
        merges.truncate(50);
        push_reason(&mut reasons, WorkPreflightReason::MergeWindowIncomplete);
    }
    for merge in &merges {
        if !merge.paths_complete && !included(request, merge) {
            push_reason(&mut reasons, WorkPreflightReason::MergePathsIncomplete);
            continue;
        }
        if scope_overlap_reasons(scope, &merge.paths).is_empty() {
            continue;
        }
        if !checked(request, merge) {
            push_reason(&mut reasons, WorkPreflightReason::UncheckedMerge);
            continue;
        }
        let relation = request.checked_merges.iter().find(|candidate| {
            candidate.pull_number == merge.pull_number
                && candidate.merge_commit == merge.merge_commit
                && candidate.source == merge.source
                && candidate.source_id == merge.source_id
        });
        if request.source_revision == merge.head_commit {
            push_reason(&mut reasons, WorkPreflightReason::SourceHeadAlreadyMerged);
        } else if request.source_revision != merge.merge_commit
            && relation.is_some_and(|checked| !checked.in_source)
        {
            push_reason(&mut reasons, WorkPreflightReason::SourceMergeMissing);
        }
    }
    let source_relation_checked = request.source_relation_checked || exact_source_anchor;
    let complete = window_complete
        && request.merge_after >= floor
        && request.merge_through == current_merge_through
        && !reasons.contains(&WorkPreflightReason::MergeCoverageUnknown)
        && !reasons.contains(&WorkPreflightReason::CheckedMergeUnknown)
        && source_relation_checked;
    Ok(WorkPreflightResult {
        status: status(&reasons),
        source_revision: Some(request.source_revision.clone()),
        merge_after: Some(request.merge_after),
        merge_through: Some(request.merge_through),
        current_merge_through,
        source_relation_checked,
        subject_match,
        scope_overlaps,
        merges,
        checked_observations,
        reasons,
        complete,
    })
}
