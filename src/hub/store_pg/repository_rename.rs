//! Explicit operator repair of a verified stable repository namespace.
use super::super::work::{WorkRepositoryRename, WorkRepositoryRenameResult};
use super::*;

#[derive(serde::Deserialize)]
struct RequestIdentity {
    token: String,
    session: String,
    work: Option<String>,
    provider: Option<String>,
    host: Option<String>,
    repository: Option<String>,
    issue: Option<String>,
    key: Option<String>,
    title: String,
    paths: String,
}

pub(super) fn repair(
    tx: &mut postgres::Transaction<'_>,
    repair: &WorkRepositoryRename,
    now: i64,
) -> Result<WorkRepositoryRenameResult> {
    let scope = &repair.scope;
    scope.validate()?;
    super::super::store::validate_repository(&repair.destination)?;
    let id = scope.repository_id.filter(|id| *id > 0).ok_or_else(|| {
        HubError::Invalid("repository repair requires a stable repository ID".into())
    })?;
    if scope.repository == repair.destination {
        return Err(HubError::Invalid("repository names must differ".into()));
    }
    // Serialize with ordinary Work transactions, token checks, compaction and refresh.
    tx.batch_execute(
        "LOCK TABLE hub_agent_tokens, hub_repositories, hub_work_items,
        hub_work_versions, hub_work_checkpoint_ops, hub_work_participants, hub_work_relations,
        hub_work_acks, hub_work_deliveries, hub_work_usage, hub_work_retention,
        hub_coordination_requests, hub_coordination_targets, hub_coordination_versions,
        hub_coordination_operations, hub_coordination_responses,
        hub_coordination_acknowledgements, hub_coordination_retention IN EXCLUSIVE MODE",
    )?;
    let verified = tx
        .query_one(
            "SELECT COUNT(*)
         FROM hub_repositories r
         JOIN hub_connections c ON c.id = r.connection
         WHERE c.provider = $1
         AND r.host = $2
         AND r.repository_id = $3
         AND r.full_name = $4",
            &[&scope.provider, &scope.host, &id, &repair.destination],
        )?
        .get::<_, i64>(0);
    if verified != 1 {
        return Err(HubError::Invalid(
            "destination does not match the connected stable repository ID".into(),
        ));
    }
    let snapshots = tx
        .query(
            "SELECT snapshot
         FROM hub_work_items
         WHERE provider = $1
         AND host = $2
         AND repository = $3
         AND repository_id = $4
         ORDER BY id",
            &[&scope.provider, &scope.host, &scope.repository, &id],
        )?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    let requests = tx
        .query(
            "SELECT id
         FROM hub_coordination_requests
         WHERE provider = $1
         AND host = $2
         AND repository = $3
         AND repository_id = $4
         ORDER BY id",
            &[&scope.provider, &scope.host, &scope.repository, &id],
        )?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    let result = WorkRepositoryRenameResult {
        work_items: snapshots.len(),
        coordination_requests: requests.len(),
        applied: repair.apply,
    };
    if snapshots.is_empty() && requests.is_empty() {
        return Ok(result);
    }
    // Unique indexes reject native/forge identity collisions. The caller rolls the
    // entire transaction back on any conflict, including a preview.
    tx.execute(
        "UPDATE hub_work_items
         SET repository = $5, source_repository = CASE WHEN source_provider = $1
         AND source_host = $2
         AND source_repository = $3 THEN $5 ELSE source_repository END
         WHERE provider = $1
         AND host = $2
         AND repository = $3
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    for snapshot in snapshots {
        let mut item: WorkItem = json_value(&snapshot)?;
        item.repository = repair.destination.clone();
        if let WorkSource::Forge {
            provider,
            host,
            repository,
            ..
        } = &mut item.source
            && *provider == scope.provider
            && *host == scope.host
            && *repository == scope.repository
        {
            *repository = repair.destination.clone();
        }
        item.updated_at = now;
        let mut event = work_event("work_checkpoint", &item)?;
        event.data = json!({"operation": "repository_namespace_repaired", "previous_repository": scope.repository, "repository": repair.destination, "repository_id": id, "generation": item.generation}).to_string();
        let seq = append(tx, &event)?;
        item.updated_seq = seq;
        save_work_version(tx, &item, seq)?;
    }
    for request in &requests {
        let fields = tx.query_one("SELECT json_build_object('token', requester_token, 'session', requester_session, 'work', proposed_work, 'provider', source_provider, 'host', source_host, 'repository', source_repository, 'issue', source_issue, 'key', request_key, 'title', proposed_title, 'paths', proposed_scope)::text
         FROM hub_coordination_requests
         WHERE id = $1", &[&request])?.get::<_, String>(0);
        let RequestIdentity {
            token,
            session,
            work,
            provider,
            host,
            repository,
            issue,
            key,
            title,
            paths,
        } = json_value(&fields)?;
        let source = match (provider, host, repository, issue) {
            (Some(provider), Some(host), Some(mut repository), Some(issue)) => {
                if provider == scope.provider
                    && host == scope.host
                    && repository == scope.repository
                {
                    repository = repair.destination.clone();
                }
                Some(super::super::work::ForgeWorkRef {
                    provider,
                    host,
                    repository,
                    issue,
                })
            }
            (None, None, None, None) => None,
            _ => {
                return Err(HubError::Unavailable(
                    "incomplete retained coordination source".into(),
                ));
            }
        };
        let identity = if let Some(work) = work {
            json_string(&("work", work))?
        } else if let Some(source) = source {
            json_string(&("source", source))?
        } else if let Some(key) =
            key.filter(|key| !super::super::coordination::is_automatic_request_key(key))
        {
            json_string(&("request", key))?
        } else {
            json_string(&("automatic", &title))?
        };
        let targets = tx
            .query(
                "SELECT work
         FROM hub_coordination_targets
         WHERE request = $1
         ORDER BY work",
                &[&request],
            )?
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>();
        let paths: Vec<String> = json_value(&paths)?;
        let repository = if targets.is_empty() {
            None
        } else {
            Some(repair.destination.as_str())
        };
        let fingerprint = lower_hex_of(
            json_string(&(
                (&token, &session),
                repository,
                &identity,
                &title,
                &paths,
                &targets,
            ))?
            .as_bytes(),
        );
        tx.execute(
            "UPDATE hub_coordination_requests
         SET fingerprint = $2
         WHERE id = $1",
            &[&request, &fingerprint],
        )?;
    }
    tx.execute(
        "UPDATE hub_coordination_requests
         SET repository = $5, source_repository = CASE WHEN source_provider = $1
         AND source_host = $2
         AND source_repository = $3 THEN $5 ELSE source_repository END
         WHERE provider = $1
         AND host = $2
         AND repository = $3
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    // Frozen Work pages contain old names. Invalidate both name-based deliveries
    // and their acknowledgements so no unseen history becomes acknowledged. Stable-ID
    // catch-up deliveries and cursors are already shared and are preserved.
    tx.execute(
        "DELETE
         FROM hub_work_deliveries
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    tx.execute(
        "DELETE
         FROM hub_work_acks
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    let event_floor_seq = tx
        .query_one(
            "SELECT COALESCE(MAX(event_floor_seq), 0)
         FROM hub_work_retention
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
            &[
                &scope.provider,
                &scope.host,
                &scope.repository,
                &id,
                &repair.destination,
            ],
        )?
        .get::<_, i64>(0);
    let snapshot_floor_seq = tx
        .query_one(
            "SELECT COALESCE(MAX(snapshot_floor_seq), 0)
         FROM hub_work_retention
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
            &[
                &scope.provider,
                &scope.host,
                &scope.repository,
                &id,
                &repair.destination,
            ],
        )?
        .get::<_, i64>(0);
    tx.execute(
        "DELETE
         FROM hub_work_retention
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    tx.execute("INSERT INTO hub_work_retention (provider, host, repository, repository_id, event_floor_seq, snapshot_floor_seq, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)", &[&scope.provider, &scope.host, &repair.destination, &id, &event_floor_seq, &snapshot_floor_seq, &now])?;
    let event_floor_seq = tx
        .query_one(
            "SELECT COALESCE(MAX(event_floor_seq), 0)
         FROM hub_coordination_retention
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
            &[
                &scope.provider,
                &scope.host,
                &scope.repository,
                &id,
                &repair.destination,
            ],
        )?
        .get::<_, i64>(0);
    tx.execute(
        "DELETE
         FROM hub_coordination_retention
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    tx.execute("INSERT INTO hub_coordination_retention (provider, host, repository, repository_id, event_floor_seq, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6)", &[&scope.provider, &scope.host, &repair.destination, &id, &event_floor_seq, &now])?;
    tx.execute(
        "DELETE
         FROM hub_work_usage
         WHERE provider = $1
         AND host = $2
         AND repository IN ($3, $5)
         AND repository_id = $4",
        &[
            &scope.provider,
            &scope.host,
            &scope.repository,
            &id,
            &repair.destination,
        ],
    )?;
    let charged = work_payload_bytes(
        tx,
        &scope.provider,
        &scope.host,
        &repair.destination,
        Some(id),
    )?;
    check_work_payload_capacity(0, charged, WORK_PAYLOAD_CAPACITY_BYTES)?;
    tx.execute("INSERT INTO hub_work_usage (provider, host, repository, repository_id, charged_bytes, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6)", &[&scope.provider, &scope.host, &repair.destination, &id, &charged, &now])?;
    Ok(result)
}
