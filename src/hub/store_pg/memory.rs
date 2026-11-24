//! Repository memory notes (see `crate::hub::memory`), as in SQLite.
use super::*;
use crate::hub::memory::{
    Actor, ListQuery, MAX_LIVE_NOTES, NewNote, Note, Page, Promotion, Removed,
};

const COLUMNS: &str = "id, body, owner, owner_login, token, token_name, session, created_at,
    promoted_by, promoted_by_login, promoted_at";
const PROMOTED: &str = "CASE WHEN promoted_at IS NULL THEN 0 ELSE 1 END";

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

fn live(tx: &mut postgres::Transaction<'_>, scope: &str) -> Result<usize> {
    let count: i64 = tx
        .query_one(
            "SELECT COUNT(*) FROM hub_memory_notes WHERE repository_scope = $1",
            &[&scope],
        )?
        .get(0);
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn load(tx: &mut postgres::Transaction<'_>, scope: &str, id: &str) -> Result<Option<Note>> {
    Ok(tx
        .query_opt(
            &format!(
                "SELECT {COLUMNS} FROM hub_memory_notes WHERE id = $1 AND repository_scope = $2"
            ),
            &[&id, &scope],
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
        if live(&mut tx, scope)? >= MAX_LIVE_NOTES {
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
        let promoted = query.after.as_ref().map(|after| after.promoted as i32);
        let limit = i64::try_from(query.limit + 1).unwrap_or(i64::MAX);
        let mut sql = format!("SELECT {COLUMNS} FROM hub_memory_notes WHERE repository_scope = $1");
        let mut values: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&scope];
        if let Some(pattern) = &query.pattern {
            values.push(pattern);
            sql.push_str(&format!(
                " AND translate(body, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') \
                 LIKE ${} ESCAPE '\\'",
                values.len()
            ));
        }
        if let (Some(after), Some(promoted)) = (&query.after, &promoted) {
            values.extend([
                promoted as &(dyn postgres::types::ToSql + Sync),
                &after.created_at,
                &after.id,
            ]);
            let n = values.len();
            sql.push_str(&format!(
                " AND ({PROMOTED}, created_at, id) < (${}::int, ${}::bigint, ${n}::text)",
                n - 2,
                n - 1
            ));
        }
        values.push(&limit);
        sql.push_str(&format!(
            " ORDER BY {PROMOTED} DESC, created_at DESC, id DESC LIMIT ${}",
            values.len()
        ));
        let notes = tx.query(&sql, &values)?.iter().map(note).collect();
        let live = live(&mut tx, scope)?;
        tx.commit()?;
        Ok(Page::from_rows(notes, query.limit, live))
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
        let Some(found) = load(&mut tx, scope, id)? else {
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
) -> Result<Option<Note>> {
    store.run(false, |client| {
        let mut tx = client.transaction()?;
        lock(&mut tx, scope)?;
        let Some(found) = load(&mut tx, scope, id)? else {
            return Ok(None);
        };
        if found.promotion.is_some() == promoted {
            return Ok(Some(found));
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
        Ok(Some(Note { promotion, ..found }))
    })
}
