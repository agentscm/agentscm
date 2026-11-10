//! Provisional Work: the unnamed, unreserved item the Hub opens for a session that edits under
//! hooks without declaring Work, so that its person sees it and its waits reach them. A
//! task-only start in the same session adopts it; otherwise it ends as abandoned.
use super::super::coordination::PROVISIONAL_KEY_PREFIX;
use super::super::work_transition::TerminalOutcome;
use super::*;

/// Why provisional Work ended, as abandoned, when its session never named it.
pub(super) const UNNAMED: &str = "Its session ended without naming this Work";
/// Idle provisional items one write ends at most; later writes end the rest.
const SWEEP_LIMIT: i64 = 8;

/// How provisional Work ends. Its outcome comes from no agent or person, so its provenance is
/// unknown.
pub(super) enum Ending<'a> {
    /// Its session ended or went quiet without naming it: the reason.
    Abandoned(&'a str),
    /// Its session declared named Work instead: that Work's id.
    Superseded(&'a str),
}

/// The start that would open the provisional Work of `session` over `paths`: unnamed until the
/// session names it, and validated like any start.
fn provisional_start(who: &WorkWho, session: &str, paths: &[String]) -> Result<WorkStart> {
    let key = super::super::store::session_key(&who.token, session);
    Ok(WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(new_id(PROVISIONAL_KEY_PREFIX)?),
        title: format!("Unnamed work in session {}", &key[..8]),
        description: None,
        session: session.to_owned(),
        role: WorkRole::Lead,
        scope: paths.to_vec(),
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: None,
    })
}

/// The provisional Work of `session`, opened for its edit of `paths` or widened to cover them.
/// `None` when the session leads named unfinished Work: the edit check links that one.
pub(super) fn touch(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: &str,
    paths: &[String],
    now: i64,
) -> Result<Option<WorkItem>> {
    sweep(tx, who, now)?;
    match unfinished_for_session(tx, who, session, now)? {
        Some(item) if item.is_provisional() => widen(tx, who, item, paths, now).map(Some),
        Some(_) => Ok(None),
        None => {
            let start = provisional_start(who, session, paths)?;
            let scope = start.validate(who)?;
            let source = WorkSource::Native {
                request_key: start.request_key.clone().unwrap_or_default(),
            };
            let id = new_id("wrk")?;
            insert_work_in(tx, who, &start, &id, source, scope, false, now).map(Some)
        }
    }
}

/// Adds the newly edited `paths` to the provisional item's scope. A scope that would no longer
/// validate stays as it was: the scope only shows what the session edited, and the edit's own
/// intent still records the file.
fn widen(
    tx: &Transaction<'_>,
    who: &WorkWho,
    mut item: WorkItem,
    paths: &[String],
    now: i64,
) -> Result<WorkItem> {
    let mut wider = item.scope.clone();
    for path in paths {
        if !wider.contains(path) {
            wider.push(path.clone());
        }
    }
    if wider.len() == item.scope.len() {
        return Ok(item);
    }
    let Ok(scope) = provisional_start(who, &item.lead_session, &wider)?.validate(who) else {
        return Ok(item);
    };
    item.scope = scope;
    item.updated_at = now;
    let mut event = work_event("work_checkpoint", &item)?;
    event.data = json!({"operation": "provisional_scope_widened", "generation": item.generation})
        .to_string();
    let seq = append(tx, &event)?;
    item.updated_seq = seq;
    save(tx, who, &item, &event, now)?;
    Ok(item)
}

/// Names the provisional `item` for a task-only `start` of its session, keeping its id: a named
/// key, and the start's declarations over the paths it asks for and everything already edited.
/// The caller then resumes it like any unheld Work, so it wins its scope like any start.
pub(super) fn adopt(
    tx: &Transaction<'_>,
    who: &WorkWho,
    mut item: WorkItem,
    start: &WorkStart,
    scope: &[String],
    now: i64,
) -> Result<WorkItem> {
    let key = new_id("req")?;
    let mut named = start.clone();
    named.request_key = Some(key.clone());
    named.scope = scope.to_vec();
    for path in &item.scope {
        if !named.scope.contains(path) {
            named.scope.push(path.clone());
        }
    }
    // Too many paths together: the start's own scope, which validated already.
    let scope = named.validate(who).or_else(|_| {
        named.scope = scope.to_vec();
        named.validate(who)
    })?;
    relation_targets(tx, who, &named.depends_on)?;
    relation_targets(tx, who, &named.related_work)?;
    item.source = WorkSource::Native {
        request_key: key.clone(),
    };
    item.issue_link = named
        .issue_choice
        .as_ref()
        .and_then(|choice| choice.linked_issue())
        .cloned();
    item.initial_issue_choice = named.issue_choice.unwrap_or_default();
    item.title = named.title;
    item.description = named.description;
    item.scope = scope;
    item.branch = named.branch;
    item.worktree = named.worktree;
    item.next_action = named.next_action;
    item.depends_on = named.depends_on;
    item.related_work = named.related_work;
    item.updated_at = now;
    let mut event = work_event("work_checkpoint", &item)?;
    event.data =
        json!({"operation": "provisional_adopted", "generation": item.generation}).to_string();
    let seq = append(tx, &event)?;
    item.updated_seq = seq;
    tx.execute(
        "UPDATE hub_work_items SET title = ?2, request_key = ?3 WHERE id = ?1",
        params![item.id, item.title, key],
    )?;
    save(tx, who, &item, &event, now)?;
    replace_relations(tx, who, &item.id, &item.depends_on, &item.related_work)?;
    tx.execute(
        "UPDATE hub_work_declarations SET snapshot = ?2, event_seq = ?3 WHERE work = ?1",
        params![item.id, json_string(&WorkDeclarations::legacy(&item))?, seq],
    )?;
    Ok(item)
}

/// Finishes the provisional `item`. Its event is an internal checkpoint, so no new event kind or
/// value reaches a reader, and its outcome's provenance is unknown.
pub(super) fn end(
    tx: &Transaction<'_>,
    who: &WorkWho,
    mut item: WorkItem,
    ending: Ending<'_>,
    now: i64,
) -> Result<WorkItem> {
    let (outcome, data) = match ending {
        Ending::Abandoned(reason) => (
            TerminalOutcome::Abandoned,
            json!({"operation": "provisional_ended", "outcome": "abandoned", "reason": reason,
                "generation": item.generation}),
        ),
        Ending::Superseded(by) => {
            if !item.related_work.iter().any(|work| work == by) {
                item.related_work.push(by.to_owned());
            }
            (
                TerminalOutcome::Superseded,
                json!({"operation": "provisional_ended", "outcome": "superseded",
                    "superseded_by": by, "generation": item.generation}),
            )
        }
    };
    item.state = WorkState::Finished;
    item.updated_at = now;
    let mut event = work_event("work_checkpoint", &item)?;
    event.data = data.to_string();
    let seq = append(tx, &event)?;
    item.updated_seq = seq;
    save(tx, who, &item, &event, now)?;
    set_work_reservation(tx, &item.id, Some(outcome.column()), None, now)?;
    replace_relations(tx, who, &item.id, &item.depends_on, &item.related_work)?;
    Ok(item)
}

/// Ends, as abandoned, up to `SWEEP_LIMIT` provisional items of `who`'s repository whose session
/// has been quiet for longer than a lease. Runs on writes only, so reads stay pure.
fn sweep(tx: &Transaction<'_>, who: &WorkWho, now: i64) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT w.snapshot FROM hub_work_items w
           LEFT JOIN hub_sessions s ON s.token = w.lead_token AND s.session = w.lead_session
         WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3 AND w.repository_id IS ?4
           AND w.state IN ('active', 'blocked') AND substr(w.request_key, 1, 5) = 'prov_'
           AND MAX(COALESCE(s.updated_at, 0), COALESCE(s.tool_activity_at, 0)) < ?5
         ORDER BY w.updated_at, w.id LIMIT ?6",
    )?;
    let idle = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                now - LEASE_SECONDS,
                SWEEP_LIMIT
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for snapshot in idle {
        let item: WorkItem = json_value(&snapshot)?;
        end(tx, who, item, Ending::Abandoned(UNNAMED), now)?;
    }
    Ok(())
}

/// Saves `item` at its `updated_seq` and accounts for its new version and event, less the stored
/// row it replaces.
fn save(
    tx: &Transaction<'_>,
    who: &WorkWho,
    item: &WorkItem,
    event: &Event,
    now: i64,
) -> Result<()> {
    let previous = stored_work_item_bytes(tx, &item.id)?;
    let snapshot = save_work_version(tx, item, item.updated_seq)?;
    let delta = work_transition_payload_bytes(&snapshot, event)?
        .checked_sub(previous)
        .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?;
    adjust_work_payload_capacity(tx, who, delta, now)
}
