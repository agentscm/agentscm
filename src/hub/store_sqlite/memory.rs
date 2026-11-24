//! Repository memory notes (see `crate::hub::memory`).
use super::*;
use crate::hub::memory::{
    Actor, ListQuery, MAX_LIVE_NOTES, MAX_NOTES_PER_OWNER, NewNote, Note, Page, Promoted,
    Promotion, Removed, expired_before,
};

const COLUMNS: &str = "id, body, owner, owner_login, token, token_name, session, created_at,
    promoted_by, promoted_by_login, promoted_at";
const PROMOTED: &str = "CASE WHEN promoted_at IS NULL THEN 0 ELSE 1 END";
/// Live notes (`memory::expired_before`), with the expiry time as `?2`.
const LIVE: &str = "(promoted_at IS NOT NULL OR created_at > ?2)";

fn note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let promoted_by: Option<String> = row.get(8)?;
    let promoted_by_login: Option<String> = row.get(9)?;
    let promoted_at: Option<i64> = row.get(10)?;
    Ok(Note {
        id: row.get(0)?,
        body: row.get(1)?,
        owner: row.get(2)?,
        owner_login: row.get(3)?,
        token: row.get(4)?,
        token_name: row.get(5)?,
        session: row.get(6)?,
        created_at: row.get(7)?,
        promotion: match (promoted_by, promoted_by_login, promoted_at) {
            (Some(by), Some(by_login), Some(at)) => Some(Promotion { by, by_login, at }),
            _ => None,
        },
    })
}

fn live(tx: &Transaction<'_>, scope: &str, now: i64) -> Result<usize> {
    let count: i64 = tx.query_row(
        &format!("SELECT COUNT(*) FROM hub_memory_notes WHERE repository_scope = ?1 AND {LIVE}"),
        params![scope, expired_before(now)],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// A live note of `scope`.
fn load(tx: &Transaction<'_>, scope: &str, id: &str, now: i64) -> Result<Option<Note>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM hub_memory_notes WHERE repository_scope = ?1 AND {LIVE}
                 AND id = ?3"
            ),
            params![scope, expired_before(now), id],
            note,
        )
        .optional()?)
}

pub(super) fn write(
    store: &SqliteStore,
    token: &str,
    scope: &str,
    new: &NewNote,
    now: i64,
) -> Result<Note> {
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let who = work_who(&tx, token, now)?;
    if who.read_only {
        return Err(HubError::Unauthorized);
    }
    tx.execute(
        "DELETE FROM hub_memory_notes WHERE repository_scope = ?1 AND promoted_at IS NULL
         AND created_at <= ?2",
        params![scope, expired_before(now)],
    )?;
    // Expired notes are gone, so every note left is live; promoted notes use no owner's share.
    let owned: i64 = tx.query_row(
        "SELECT COUNT(*) FROM hub_memory_notes WHERE repository_scope = ?1 AND owner = ?2
         AND promoted_at IS NULL",
        params![scope, who.owner.id],
        |row| row.get(0),
    )?;
    if usize::try_from(owned).unwrap_or(usize::MAX) >= MAX_NOTES_PER_OWNER {
        return Err(who.capacity(CapacityLimit::MemoryNotesOwner));
    }
    if live(&tx, scope, now)? >= MAX_LIVE_NOTES {
        return Err(who.capacity(CapacityLimit::MemoryNotes));
    }
    let stored = Note {
        id: new_id("mem")?,
        body: new.body.clone(),
        owner: who.owner.id.clone(),
        owner_login: who.owner.login.clone(),
        token: token.to_owned(),
        token_name: who.agent.clone(),
        session: new.session.clone(),
        created_at: now,
        promotion: None,
    };
    tx.execute(
        "INSERT INTO hub_memory_notes (id, repository_scope, body, owner, owner_login, token,
             token_name, session, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            stored.id,
            scope,
            stored.body,
            stored.owner,
            stored.owner_login,
            stored.token,
            stored.token_name,
            stored.session,
            stored.created_at
        ],
    )?;
    tx.commit()?;
    Ok(stored)
}

pub(super) fn list(store: &SqliteStore, scope: &str, query: &ListQuery) -> Result<Page> {
    let mut conn = store.conn();
    let tx = conn.transaction()?;
    let notes = tx
        .prepare(&format!(
            "SELECT {COLUMNS} FROM hub_memory_notes WHERE repository_scope = ?1 AND {LIVE}
             ORDER BY {PROMOTED} DESC, created_at DESC, id DESC"
        ))?
        .query_map(params![scope, expired_before(query.now)], note)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(Page::select(notes, query))
}

pub(super) fn remove(
    store: &SqliteStore,
    scope: &str,
    id: &str,
    by: Actor<'_>,
    now: i64,
) -> Result<Removed> {
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (user, token, admin) = match by {
        Actor::Token(token) => {
            let who = work_who(&tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            (who.owner.id, Some(token), false)
        }
        Actor::Person { user, admin } => (user.to_owned(), None, admin),
    };
    let Some(found) = load(&tx, scope, id, now)? else {
        return Ok(Removed::NotFound);
    };
    if found.owner != user && !admin {
        return Ok(Removed::NotAuthor);
    }
    tx.execute("DELETE FROM hub_memory_notes WHERE id = ?1", params![id])?;
    append(
        &tx,
        &Event::new(
            "memory_note_removed",
            id,
            json!({"repository_scope": scope, "by": user, "token": token, "admin": admin}),
        )?,
    )?;
    tx.commit()?;
    Ok(Removed::Removed)
}

pub(super) fn promote(
    store: &SqliteStore,
    scope: &str,
    id: &str,
    person: &User,
    promoted: bool,
    now: i64,
) -> Result<Promoted> {
    let mut conn = store.conn();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(found) = load(&tx, scope, id, now)? else {
        return Ok(Promoted::NotFound);
    };
    if promoted && found.owner == person.id {
        return Ok(Promoted::OwnNote);
    }
    if found.promotion.is_some() == promoted {
        return Ok(Promoted::Note(Box::new(found)));
    }
    let promotion = promoted.then(|| Promotion {
        by: person.id.clone(),
        by_login: person.login.clone(),
        at: now,
    });
    tx.execute(
        "UPDATE hub_memory_notes SET promoted_by = ?2, promoted_by_login = ?3, promoted_at = ?4
         WHERE id = ?1",
        params![
            id,
            promotion.as_ref().map(|p| &p.by),
            promotion.as_ref().map(|p| &p.by_login),
            promotion.as_ref().map(|p| p.at)
        ],
    )?;
    let kind = if promoted {
        "memory_note_promoted"
    } else {
        "memory_note_unpromoted"
    };
    append(
        &tx,
        &Event::new(
            kind,
            id,
            json!({"repository_scope": scope, "by": person.id}),
        )?,
    )?;
    tx.commit()?;
    Ok(Promoted::Note(Box::new(Note { promotion, ..found })))
}
