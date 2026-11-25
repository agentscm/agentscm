//! Repository memory notes (see `crate::hub::memory`), as in SQLite.
use super::*;
use crate::hub::memory::{
    Actor, ListQuery, MAX_LIVE_NOTES, MAX_NOTES_PER_OWNER, NewNote, Note, Page, Promoted,
    Promotion, Removed, expired_before,
};

const COLUMNS: &str = "id, body, owner, owner_login, token, token_name, session, created_at,
    promoted_by, promoted_by_login, promoted_at";
const PROMOTED: &str = "CASE WHEN promoted_at IS NULL THEN 0 ELSE 1 END";
/// Live notes (`memory::expired_before`), with the expiry time as `$2`.
const LIVE: &str = "(promoted_at IS NOT NULL OR created_at > $2)";

fn note(row: &postgres::Row) -> Note {
    let promotion = match (
        row.get::<_, Option<String>>(8),
        row.get::<_, Option<String>>(9),
        row.get::<_, Option<i64>>(10),
    ) {
        (Some(by), Some(by_login), Some(at)) => Some(Promotion { by, by_login, at }),
        _ => None,
    };
    Note {
        id: row.get(0),
        body: row.get(1),
        owner: row.get(2),
        owner_login: row.get(3),
        token: row.get(4),
        token_name: row.get(5),
        session: row.get(6),
        created_at: row.get(7),
        promotion,
    }
}

/// Serializes the writes of one repository's memory, so its count holds under concurrency.
fn lock(tx: &mut postgres::Transaction<'_>, scope: &str) -> Result<()> {
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        &[&format!("memory:{scope}")],
    )?;
    Ok(())
}

fn live(tx: &mut postgres::Transaction<'_>, scope: &str, now: i64) -> Result<usize> {
    let count: i64 = tx
        .query_one(
            &format!(
                "SELECT COUNT(*) FROM hub_memory_notes WHERE repository_scope = $1 AND {LIVE}"
            ),
            &[&scope, &expired_before(now)],
        )?
        .get(0);
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

/// A live note of `scope`.
fn load(
    tx: &mut postgres::Transaction<'_>,
    scope: &str,
    id: &str,
    now: i64,
) -> Result<Option<Note>> {
    Ok(tx
        .query_opt(
            &format!(
                "SELECT {COLUMNS} FROM hub_memory_notes WHERE repository_scope = $1 AND {LIVE}
                 AND id = $3"
            ),
            &[&scope, &expired_before(now), &id],
        )?
        .as_ref()
        .map(note))
}

pub(super) fn write(
    store: &PgStore,
    token: &str,
    scope: &str,
    new: &NewNote,
    now: i64,
) -> Result<Note> {
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        let who = work_who(&mut tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        lock(&mut tx, scope)?;
        tx.execute(
            "DELETE FROM hub_memory_notes WHERE repository_scope = $1 AND promoted_at IS NULL
             AND created_at <= $2",
            &[&scope, &expired_before(now)],
        )?;
        // Under the repository's lock, so the owner's share holds under concurrency too. Expired
        // notes are gone, so every note left is live. A refused write still keeps that.
        let owned: i64 = tx
            .query_one(
                "SELECT COUNT(*) FROM hub_memory_notes WHERE repository_scope = $1 AND owner = $2",
                &[&scope, &who.owner.id],
            )?
            .get(0);
        let refused = if usize::try_from(owned).unwrap_or(usize::MAX) >= MAX_NOTES_PER_OWNER {
            Some(CapacityLimit::MemoryNotesOwner)
        } else if live(&mut tx, scope, now)? >= MAX_LIVE_NOTES {
            Some(CapacityLimit::MemoryNotes)
        } else {
            None
        };
        if let Some(limit) = refused {
            tx.commit()?;
            return Err(who.capacity(limit));
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
                 token_name, session, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &stored.id,
                &scope,
                &stored.body,
                &stored.owner,
                &stored.owner_login,
                &stored.token,
                &stored.token_name,
                &stored.session,
                &stored.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(stored)
    })
}

pub(super) fn list(store: &PgStore, scope: &str, query: &ListQuery) -> Result<Page> {
    store.run(true, |client| {
        let mut tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()?;
        let notes = tx
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM hub_memory_notes WHERE repository_scope = $1 AND {LIVE}
                     ORDER BY {PROMOTED} DESC, created_at DESC, id COLLATE \"C\" DESC"
                ),
                &[&scope, &expired_before(query.now)],
            )?
            .iter()
            .map(note)
            .collect();
        tx.commit()?;
        Ok(Page::select(notes, query))
    })
}

pub(super) fn remove(
    store: &PgStore,
    scope: &str,
    id: &str,
    by: Actor<'_>,
    now: i64,
) -> Result<Removed> {
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        let (user, token, admin) = match by {
            Actor::Token(token) => {
                let who = work_who(&mut tx, token, now)?;
                if who.read_only {
                    return Err(HubError::Unauthorized);
                }
                (who.owner.id, Some(token), false)
            }
            Actor::Person { user, admin } => (user.to_owned(), None, admin),
        };
        lock(&mut tx, scope)?;
        let Some(found) = load(&mut tx, scope, id, now)? else {
            return Ok(Removed::NotFound);
        };
        if found.owner != user && !admin {
            return Ok(Removed::NotAuthor);
        }
        tx.execute("DELETE FROM hub_memory_notes WHERE id = $1", &[&id])?;
        append(
            &mut tx,
            &Event::new(
                "memory_note_removed",
                id,
                json!({"repository_scope": scope, "by": user, "token": token, "admin": admin}),
            )?,
        )?;
        tx.commit()?;
        Ok(Removed::Removed)
    })
}

pub(super) fn promote(
    store: &PgStore,
    scope: &str,
    id: &str,
    person: &User,
    promoted: bool,
    now: i64,
) -> Result<Promoted> {
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        lock(&mut tx, scope)?;
        let Some(found) = load(&mut tx, scope, id, now)? else {
            return Ok(Promoted::NotFound);
        };
        if found.promotion.is_some() == promoted {
            return Ok(Promoted::Note(Box::new(found)));
        }
        if promoted && found.owner == person.id {
            return Ok(Promoted::OwnNote);
        }
        let promotion = promoted.then(|| Promotion {
            by: person.id.clone(),
            by_login: person.login.clone(),
            at: now,
        });
        tx.execute(
            "UPDATE hub_memory_notes SET promoted_by = $2, promoted_by_login = $3, promoted_at = $4
             WHERE id = $1",
            &[
                &id,
                &promotion.as_ref().map(|p| p.by.as_str()),
                &promotion.as_ref().map(|p| p.by_login.as_str()),
                &promotion.as_ref().map(|p| p.at),
            ],
        )?;
        let kind = if promoted {
            "memory_note_promoted"
        } else {
            "memory_note_unpromoted"
        };
        append(
            &mut tx,
            &Event::new(
                kind,
                id,
                json!({"repository_scope": scope, "by": person.id}),
            )?,
        )?;
        tx.commit()?;
        Ok(Promoted::Note(Box::new(Note { promotion, ..found })))
    })
}
