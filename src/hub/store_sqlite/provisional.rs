//! Provisional Work: the unnamed, unreserved item the Hub opens for a session that edits under
//! hooks without declaring Work, so that its person sees it and its waits reach them. A
//! task-only start in the same session adopts it; otherwise it ends as abandoned.
use super::super::coordination::PROVISIONAL_KEY_PREFIX;
use super::*;

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
    let snapshot = save_work_version(tx, &item, seq)?;
    adjust_work_payload_capacity(
        tx,
        who,
        work_transition_payload_bytes(&snapshot, &event)?,
        now,
    )?;
    Ok(item)
}
