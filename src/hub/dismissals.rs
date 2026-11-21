//! A person's own dismissed and snoozed attention items, per repository. The Hub stores an item
//! key with a fingerprint of the item's state; the item is hidden while its stored fingerprint
//! still matches, so a dismissed item returns when it changes. A snooze also carries `until`,
//! after which the entry is pruned. The shared status reads its viewer's entries and stops
//! counting the items they name (see `actionable_status`); any other key stays the page's own.
//! Nobody else's entries are read.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use super::http::{ApiError, JsonBody, blocking};
use super::login::HumanSession;
use super::store::{Store, unix_now, validate_repository};
use super::{HubError, HubState, Result};

/// Entries kept per person and repository; an upsert beyond it drops the oldest.
pub const MAX_DISMISSALS: usize = 200;
pub const MAX_KEY_BYTES: usize = 200;
pub const MAX_FINGERPRINT_BYTES: usize = 128;
/// The longest snooze.
pub const MAX_SNOOZE_SECONDS: i64 = 90 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dismissal {
    pub key: String,
    pub fingerprint: String,
    /// Unix seconds the snooze ends; `None` hides the item until its fingerprint changes.
    pub until: Option<i64>,
    pub at: i64,
}

pub fn routes() -> Router<HubState> {
    Router::new().route(
        "/v1/hub/session/dismissals",
        get(list).put(upsert).delete(remove),
    )
}

/// Keyed by the repository's forge id, so a rename keeps the entries.
pub(super) fn setting_key(user_id: &str, repository_id: i64) -> String {
    format!("dismissals:{user_id}:{repository_id}")
}

/// A person's live entries for a repository.
pub(super) fn current(
    store: &dyn Store,
    user_id: &str,
    repository_id: i64,
    now: i64,
) -> Result<Vec<Dismissal>> {
    load(store, &setting_key(user_id, repository_id), now)
}

/// A missing or corrupt value is no entries; expired snoozes are dropped.
fn load(store: &dyn Store, key: &str, now: i64) -> Result<Vec<Dismissal>> {
    let mut entries: Vec<Dismissal> = store
        .get_setting(key)?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    entries.retain(|d| d.until.is_none_or(|until| until > now));
    Ok(entries)
}

fn save(store: &dyn Store, key: &str, entries: &[Dismissal], actor: &str) -> Result<()> {
    let raw = serde_json::to_string(entries)
        .map_err(|_| HubError::Unavailable("dismissal encoding failed".into()))?;
    store.set_setting(key, &raw, actor)
}

/// Replaces the entry with the same key, then drops the oldest beyond [`MAX_DISMISSALS`].
fn upserted(mut entries: Vec<Dismissal>, entry: Dismissal) -> Vec<Dismissal> {
    entries.retain(|d| d.key != entry.key);
    entries.push(entry);
    entries.sort_by_key(|d| d.at);
    let excess = entries.len().saturating_sub(MAX_DISMISSALS);
    entries.drain(..excess);
    entries
}

fn checked_text(name: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(HubError::Invalid(format!(
            "{name} must be 1..={max} bytes without control characters"
        )));
    }
    Ok(())
}

fn checked(body: &Upsert, now: i64) -> Result<()> {
    checked_text("key", &body.key, MAX_KEY_BYTES)?;
    checked_text("fingerprint", &body.fingerprint, MAX_FINGERPRINT_BYTES)?;
    match body.until {
        Some(until) if until <= now || until > now + MAX_SNOOZE_SECONDS => Err(HubError::Invalid(
            format!("until must be in the next {MAX_SNOOZE_SECONDS} seconds"),
        )),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryQuery {
    repository: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upsert {
    repository: String,
    key: String,
    fingerprint: String,
    #[serde(default)]
    until: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remove {
    repository: String,
    key: String,
}

#[derive(Serialize)]
struct Page {
    dismissals: Vec<Dismissal>,
}

/// The signed-in person and their setting key for a repository they can read now.
async fn scoped(
    state: &HubState,
    human: &HumanSession,
    repository: &str,
) -> std::result::Result<(String, String), ApiError> {
    let user = human.web_user()?.clone();
    validate_repository(repository)?;
    // A person's own dismissals change nothing anyone else sees, so reading is enough.
    let repo = super::login::devices::read_check(state, &user, repository, "dismissals").await?;
    let key = setting_key(&user.id, repo.repository_id);
    Ok((user.id, key))
}

async fn list(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> std::result::Result<Json<Page>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let (_, key) = scoped(&state, &human, &query.repository).await?;
    let store = state.store.clone();
    let dismissals = blocking(&state, move || load(&*store, &key, unix_now())).await?;
    Ok(Json(Page { dismissals }))
}

async fn upsert(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<Upsert>,
) -> std::result::Result<Json<Page>, ApiError> {
    checked(&body, unix_now())?;
    let (user, key) = scoped(&state, &human, &body.repository).await?;
    let store = state.store.clone();
    // shortcut: read-modify-write of one value is last-writer-wins; only this person's own pages
    // write it, so a race loses one click. Move to a table if that matters.
    let dismissals = blocking(&state, move || {
        let now = unix_now();
        let entry = Dismissal {
            key: body.key,
            fingerprint: body.fingerprint,
            until: body.until,
            at: now,
        };
        let entries = upserted(load(&*store, &key, now)?, entry);
        save(&*store, &key, &entries, &user)?;
        Ok(entries)
    })
    .await?;
    Ok(Json(Page { dismissals }))
}

async fn remove(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<Remove>, QueryRejection>,
) -> std::result::Result<Json<Page>, ApiError> {
    let Query(body) = query.map_err(|_| ApiError::invalid())?;
    checked_text("key", &body.key, MAX_KEY_BYTES)?;
    let (user, key) = scoped(&state, &human, &body.repository).await?;
    let store = state.store.clone();
    let dismissals = blocking(&state, move || {
        let mut entries = load(&*store, &key, unix_now())?;
        let before = entries.len();
        entries.retain(|d| d.key != body.key);
        if entries.len() != before {
            save(&*store, &key, &entries, &user)?;
        }
        Ok(entries)
    })
    .await?;
    Ok(Json(Page { dismissals }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, at: i64, until: Option<i64>) -> Dismissal {
        Dismissal {
            key: key.into(),
            fingerprint: "f".into(),
            until,
            at,
        }
    }

    #[test]
    fn upsert_replaces_by_key_and_bounds_by_age() {
        let mut entries = Vec::new();
        for at in 0..MAX_DISMISSALS as i64 {
            entries = upserted(entries, entry(&format!("k{at}"), at, None));
        }
        entries = upserted(entries, entry("k5", 1000, Some(2000)));
        assert_eq!(entries.len(), MAX_DISMISSALS);
        assert_eq!(entries.last().unwrap().until, Some(2000));
        entries = upserted(entries, entry("new", 1001, None));
        assert_eq!(entries.len(), MAX_DISMISSALS);
        assert_eq!(entries[0].key, "k1", "the oldest, k0, was dropped");
    }

    #[test]
    fn checks_bounds() {
        let now = 1_000;
        let body = |key: &str, fingerprint: &str, until| Upsert {
            repository: "acme/widgets".into(),
            key: key.into(),
            fingerprint: fingerprint.into(),
            until,
        };
        assert!(checked(&body("k", "f", None), now).is_ok());
        assert!(checked(&body("k", "f", Some(now + 86_400)), now).is_ok());
        assert!(checked(&body("", "f", None), now).is_err());
        assert!(checked(&body(&"k".repeat(201), "f", None), now).is_err());
        assert!(checked(&body("k", &"f".repeat(129), None), now).is_err());
        assert!(checked(&body("k\n", "f", None), now).is_err());
        assert!(checked(&body("k", "f", Some(now)), now).is_err());
        assert!(checked(&body("k", "f", Some(now + MAX_SNOOZE_SECONDS + 1)), now).is_err());
    }
}
