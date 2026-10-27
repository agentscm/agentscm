//! Bounded, read-only forge observations for return-to-work deliveries.

use std::collections::BTreeSet;

use serde_json::Value;

use super::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    MAX_CATCHUP_PATHS, NewCatchupMerge, bounded_title,
};
use super::connection::{ConnectedRepository, Connection};
use super::forge_api::ForgeApi;
use super::github::{Gh, GhError, parse_rfc3339};
use super::http::blocking;
use super::store::unix_now;
use super::{HubError, HubState};

const NEW_MERGES_PER_POLL: usize = 20;

fn scope(conn: &Connection, repo: &ConnectedRepository) -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: conn.provider.clone(),
        host: conn.host.clone(),
        repository: repo.full_name.clone(),
        repository_id: Some(repo.repository_id),
    }
}

pub(super) async fn begin(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
) -> Gh<()> {
    source_status(
        state,
        conn,
        repo,
        forge_status(CatchupCoverageState::Incomplete, unix_now(), 0, 0, None),
    )
    .await
}

/// Observations are independent of Work acknowledgement and forge writes.
pub(super) async fn reconcile(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    activity_days: i64,
) -> Gh<()> {
    let checked_at = unix_now();
    let window_from = checked_at - activity_days * 86_400;
    // A bounded read/ingestion batch is not a completed source observation.
    // Preflight must see that uncertainty before any member changes its watermark.
    source_status(
        state,
        conn,
        repo,
        forge_status(
            CatchupCoverageState::Incomplete,
            checked_at,
            0,
            0,
            Some(window_from),
        ),
    )
    .await?;
    let result = read_recent(state, conn, repo, token, checked_at, window_from).await;
    let (coverage, unread_pages, deferred) = match &result {
        Ok((unread, deferred)) if *unread == 0 && *deferred == 0 => {
            (CatchupCoverageState::ObservedOnly, 0, 0)
        }
        Ok((unread, deferred)) => (CatchupCoverageState::Incomplete, *unread, *deferred),
        Err(_) => (CatchupCoverageState::Unavailable, 0, 0),
    };
    source_status(
        state,
        conn,
        repo,
        forge_status(
            coverage,
            checked_at,
            unread_pages,
            deferred,
            Some(window_from),
        ),
    )
    .await?;
    result.map(drop)
}

pub(super) async fn unavailable(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
) -> Gh<()> {
    source_status(
        state,
        conn,
        repo,
        forge_status(CatchupCoverageState::Unavailable, unix_now(), 0, 0, None),
    )
    .await
}

pub(super) fn forge_status(
    state: CatchupCoverageState,
    checked_at: i64,
    unread_pages: u64,
    deferred: u64,
    window_from: Option<i64>,
) -> CatchupSourceStatus {
    CatchupSourceStatus {
        source: CatchupMergeSource::ForgePull,
        state,
        checked_at,
        unread_pages,
        deferred,
        window_from,
    }
}

pub(super) async fn source_status(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    status: CatchupSourceStatus,
) -> Gh<()> {
    let scope = scope(conn, repo);
    let store = state.store.clone();
    blocking(state, move || {
        store.record_catchup_source_status(&scope, &status)
    })
    .await
    .map_err(failure)
}

/// First closed-proposal page size. A power of two so halving after an over-cap
/// body keeps `offset % per_page == 0`.
const FIRST_PAGE_SIZE: u32 = 32;
/// Closed-listing requests (including size-halving retries) per reconcile pass.
const MAX_LIST_REQUESTS: u32 = 40;
/// A second look at the list head tolerates forge/hub clock skew.
const HEAD_SKEW_SECS: i64 = 300;

/// Progress shared by the main walk and the head recheck of one pass.
struct Pass {
    per_page: u32,
    requests: u32,
    read: usize,
    deferred: u64,
    unread: u64,
    seen: BTreeSet<i64>,
}

async fn read_recent(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    checked_at: i64,
    window_from: i64,
) -> Gh<(u64, u64)> {
    let api = state.sync.forge(conn)?;
    let mut pass = Pass {
        per_page: FIRST_PAGE_SIZE,
        requests: 0,
        read: 0,
        deferred: 0,
        unread: 0,
        seen: BTreeSet::new(),
    };
    let complete = walk(state, conn, repo, token, &api, window_from, &mut pass).await?;
    // Proposals updated while the walk ran move to the list head, past its offset.
    let head_complete = if pass.requests > 1 {
        walk(
            state,
            conn,
            repo,
            token,
            &api,
            checked_at - HEAD_SKEW_SECS,
            &mut pass,
        )
        .await?
    } else {
        true
    };
    if !(complete && head_complete) {
        pass.unread = pass.unread.max(1);
    }
    Ok((pass.unread, pass.deferred))
}

/// Reads closed proposals newest-updated first until one is older than `threshold`
/// or the list ends. `false` means the pass stopped short of both.
async fn walk(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &ForgeApi<'_>,
    threshold: i64,
    pass: &mut Pass,
) -> Gh<bool> {
    let mut offset: u32 = 0;
    loop {
        if pass.requests >= MAX_LIST_REQUESTS {
            return Ok(false);
        }
        pass.requests += 1;
        let page = match api
            .closed_pulls_page(token, repo, pass.per_page, offset / pass.per_page + 1)
            .await
        {
            Ok(page) => page,
            Err(GhError::TooLarge(_)) if pass.per_page > 1 => {
                pass.per_page /= 2;
                continue;
            }
            Err(GhError::TooLarge(_)) => {
                pass.unread += 1;
                offset += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let len = page.items.len();
        for hint in page.items {
            let updated = hint["updated_at"]
                .as_str()
                .and_then(parse_rfc3339)
                .ok_or_else(|| GhError::Failed("invalid closed proposal update time".into()))?;
            if updated < threshold {
                return Ok(true);
            }
            candidate(state, conn, repo, token, api, &hint, pass).await?;
        }
        if !page.next || len == 0 {
            return Ok(true);
        }
        if len != pass.per_page as usize {
            // A short page with a successor cannot be addressed by offset.
            return Ok(false);
        }
        offset += pass.per_page;
    }
}

async fn candidate(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    api: &ForgeApi<'_>,
    hint: &Value,
    pass: &mut Pass,
) -> Gh<()> {
    let Some(number) = hint["number"].as_i64().filter(|number| *number > 0) else {
        return Err(GhError::Failed("invalid closed proposal identity".into()));
    };
    if !pass.seen.insert(number) {
        return Ok(());
    }
    if hint["merged"] != true && hint["merged_at"].as_str().is_none() {
        return Ok(());
    }
    let scope = scope(conn, repo);
    let store = state.store.clone();
    let merge_commit = hint["merge_commit_sha"].as_str().map(str::to_owned);
    let known = blocking(state, move || {
        store.catchup_merge_known(&scope, number, merge_commit.as_deref())
    })
    .await
    .map_err(failure)?;
    if known {
        return Ok(());
    }
    if pass.read == NEW_MERGES_PER_POLL {
        pass.deferred += 1;
        return Ok(());
    }
    pass.read += 1;
    let Some(pull) = api.pull(token, repo, number).await? else {
        pass.deferred += 1;
        return Ok(());
    };
    if pull["merged"] != true {
        return Ok(());
    }
    observe(state, conn, repo, token, number, &pull).await
}

/// Called after the normal sync's authenticated individual read too, so webhook
/// delivery does not have to wait for the next bounded history poll.
pub(super) async fn observe(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    expected_number: i64,
    pull: &Value,
) -> Gh<()> {
    let number = pull["number"]
        .as_i64()
        .filter(|number| *number > 0)
        .ok_or_else(|| GhError::Failed("invalid merged proposal identity".into()))?;
    if number != expected_number {
        return Err(GhError::Failed("merged proposal identity changed".into()));
    }
    let merge_commit = pull["merge_commit_sha"]
        .as_str()
        .ok_or_else(|| GhError::Failed("missing merged proposal commit".into()))?
        .to_owned();
    let scope = scope(conn, repo);
    let store = state.store.clone();
    let known_scope = scope.clone();
    let known_commit = merge_commit.clone();
    if blocking(state, move || {
        store.catchup_merge_known(&known_scope, number, Some(&known_commit))
    })
    .await
    .map_err(failure)?
    {
        return Ok(());
    }
    let (mut paths, truncated) = state
        .sync
        .forge(conn)?
        .pull_files(token, repo, number)
        .await?;
    paths.sort();
    paths.dedup();
    let paths_complete = !truncated && paths.len() <= MAX_CATCHUP_PATHS;
    paths.truncate(MAX_CATCHUP_PATHS);
    let observed_at = unix_now();
    let merge = projection(conn, repo, pull, paths, paths_complete, observed_at)?;
    let store = state.store.clone();
    blocking(state, move || {
        store.record_catchup_merge(&scope, &merge, observed_at)
    })
    .await
    .map_err(failure)?;
    Ok(())
}

fn projection(
    conn: &Connection,
    repo: &ConnectedRepository,
    pull: &Value,
    paths: Vec<String>,
    paths_complete: bool,
    observed_at: i64,
) -> Gh<NewCatchupMerge> {
    let invalid = || GhError::Failed("invalid merged proposal metadata".into());
    let target = match conn.provider.as_str() {
        "github" => pull["base"]["repo"]["id"].as_i64(),
        "gitlab" => pull["target_project_id"].as_i64(),
        _ => None,
    };
    if pull["merged"] != true || target != Some(repo.repository_id) {
        return Err(invalid());
    }
    let text = |value: &Value| value.as_str().map(str::to_owned).ok_or_else(invalid);
    let title = pull["title"].as_str().and_then(bounded_title);
    let occurred_at = match &pull["merged_at"] {
        Value::Null => None,
        Value::String(value)
            if (20..=40).contains(&value.len())
                && value.as_bytes()[..19]
                    .iter()
                    .enumerate()
                    .all(|(index, byte)| match index {
                        4 | 7 => *byte == b'-',
                        10 => *byte == b'T',
                        13 | 16 => *byte == b':',
                        _ => byte.is_ascii_digit(),
                    }) =>
        {
            Some(
                match conn.provider.as_str() {
                    "gitlab" => super::gitlab::time(value),
                    _ => parse_rfc3339(value),
                }
                .ok_or_else(invalid)?,
            )
        }
        _ => return Err(invalid()),
    };
    let number = pull["number"].as_i64().ok_or_else(invalid)?;
    let merge = NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: format!("pull:{number}"),
        occurred_at,
        title,
        branch: text(&pull["head"]["ref"])?,
        base: text(&pull["base"]["ref"])?,
        head_commit: text(&pull["head"]["sha"])?,
        merge_commit: text(&pull["merge_commit_sha"])?,
        pull_number: number,
        paths,
        paths_complete,
    };
    merge.validate(observed_at).map_err(failure)?;
    Ok(merge)
}

fn failure(error: HubError) -> GhError {
    GhError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identities(provider: &str) -> (Connection, ConnectedRepository) {
        (
            Connection {
                id: "connection".into(),
                provider: provider.into(),
                host: "forge.example".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: None,
                outbound: false,
                status: "enabled".into(),
                status_detail: None,
                next_poll_at: 0,
            },
            ConnectedRepository {
                id: "repository".into(),
                connection: "connection".into(),
                host: "forge.example".into(),
                repository_id: 42,
                full_name: "team/project".into(),
                node_id: None,
                installation_id: None,
                pulls_etag: None,
            },
        )
    }

    fn pull() -> Value {
        json!({"number": 7, "merged": true, "merged_at": "2026-10-25T01:02:03Z",
            "title": "Merge changes", "head": {"ref":"feature", "sha":"a".repeat(40)},
            "base":{"ref":"main", "repo":{"id":42}},
            "merge_commit_sha":"b".repeat(40), "target_project_id":42})
    }

    #[test]
    fn projection_binds_the_provider_target_and_preserves_chronology() {
        for provider in ["github", "gitlab"] {
            let (connection, repository) = identities(provider);
            let mut body = pull();
            let merge = projection(
                &connection,
                &repository,
                &body,
                vec!["src/a.rs".into()],
                true,
                1_800_000_000,
            )
            .unwrap();
            assert_eq!(merge.occurred_at, Some(1_792_890_123));
            assert!(merge.paths_complete);
            body["base"]["repo"]["id"] = json!(99);
            body["target_project_id"] = json!(99);
            assert!(
                projection(
                    &connection,
                    &repository,
                    &body,
                    Vec::new(),
                    false,
                    1_800_000_000
                )
                .is_err()
            );
        }
    }

    #[test]
    fn projection_bounds_unicode_titles_and_rejects_unconfirmed_or_invalid_identities() {
        let (connection, repository) = identities("github");
        let mut body = pull();
        body["title"] = json!(format!("\n{}", "é".repeat(200)));
        let merge = projection(
            &connection,
            &repository,
            &body,
            Vec::new(),
            false,
            1_800_000_000,
        )
        .unwrap();
        assert_eq!(merge.title.unwrap().len(), 256);
        for field in ["merge_commit_sha", "merged"] {
            let mut invalid = body.clone();
            invalid[field] = Value::Null;
            assert!(
                projection(
                    &connection,
                    &repository,
                    &invalid,
                    Vec::new(),
                    false,
                    1_800_000_000
                )
                .is_err()
            );
        }
        body["head"]["ref"] = json!("../outside");
        assert!(
            projection(
                &connection,
                &repository,
                &body,
                Vec::new(),
                false,
                1_800_000_000
            )
            .is_err()
        );
    }
}
