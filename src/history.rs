//! Bounded, resumable graph inspection for the trusted-local source profile.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::Revision;
use crate::store::{SourceRead, digest, validate_id};
use crate::{Error, Result};

pub const MAX_HISTORY_PAGE: usize = 1_000;
pub const MAX_HISTORY_REVISIONS: usize = 100_000;
const MAX_CURSOR_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: String,
    pub revision: Revision,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub needed_objects: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HeadRelation {
    Equal,
    LeftAncestor,
    RightAncestor,
    Diverged,
    Pending,
    Incomplete,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationPage {
    pub relation: HeadRelation,
    pub needed_objects: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Walk {
    root: String,
    pending: BTreeMap<u64, String>,
    next_position: u64,
    seen: BTreeSet<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    walks: Vec<Walk>,
}

impl Walk {
    fn new(root: &str) -> Result<Self> {
        validate_id(root)?;
        Ok(Self {
            root: root.to_owned(),
            pending: BTreeMap::from([(0, root.to_owned())]),
            next_position: 1,
            seen: BTreeSet::new(),
        })
    }

    fn validate(&self) -> Result<()> {
        validate_id(&self.root)?;
        if self.seen.len().saturating_add(self.pending.len()) > MAX_HISTORY_REVISIONS {
            return Err(Error::Limit("history cursor exceeds graph bounds".into()));
        }
        if self.next_position == 0
            || self.next_position > (MAX_HISTORY_REVISIONS * 8 + 1) as u64
            || self
                .pending
                .keys()
                .any(|position| *position >= self.next_position)
        {
            return Err(Error::Invalid("history cursor priority is invalid".into()));
        }
        let pending: BTreeSet<_> = self.pending.values().collect();
        if pending.len() != self.pending.len() || pending.iter().any(|id| self.seen.contains(*id)) {
            return Err(Error::Invalid(
                "history cursor repeats frontier revisions".into(),
            ));
        }
        for id in self.pending.values().chain(self.seen.iter()) {
            validate_id(id)?;
        }
        Ok(())
    }

    fn advance<R: SourceRead + ?Sized>(
        &mut self,
        engine: &R,
        budget: usize,
    ) -> Result<HistoryPage> {
        let mut entries = Vec::new();
        let mut needed = BTreeSet::new();
        let mut queued: BTreeMap<String, u64> = self
            .pending
            .iter()
            .map(|(position, id)| (id.clone(), *position))
            .collect();
        for _ in 0..budget {
            let Some((position, id)) = self.pending.pop_last() else {
                break;
            };
            if self.seen.contains(&id) {
                continue;
            }
            let revision = match engine.load_revision(&id) {
                Ok(revision) => revision,
                Err(error) if error.missing_object_id().is_some() => {
                    let missing = error
                        .missing_object_id()
                        .expect("guard checked missing object");
                    validate_id(missing)?;
                    needed.insert(missing.to_owned());
                    self.pending.insert(position, id);
                    break;
                }
                Err(error) => return Err(error),
            };
            if self.seen.len() == MAX_HISTORY_REVISIONS {
                return Err(Error::Limit("history exceeds revision bound".into()));
            }
            self.seen.insert(id.clone());
            queued.remove(&id);
            // A previously queued shared parent must move to the top too:
            // M[A, B], A[B, C] visits M, A, B, C rather than M, A, C, B.
            for parent in revision.parents.iter().rev() {
                if self.seen.contains(parent) {
                    continue;
                }
                if let Some(previous) = queued.insert(parent.clone(), self.next_position) {
                    self.pending.remove(&previous);
                }
                self.pending.insert(self.next_position, parent.clone());
                self.next_position += 1;
            }
            entries.push(HistoryEntry { id, revision });
        }
        self.validate()?;
        Ok(HistoryPage {
            entries,
            needed_objects: needed.into_iter().collect(),
            next: None,
        })
    }
}

/// Each page examines at most `limit` frontier entries, including duplicates and
/// unavailable objects. An empty page with `next` is not completion.
pub fn history_page<R: SourceRead + ?Sized>(
    engine: &R,
    root: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<HistoryPage> {
    validate_limit(limit)?;
    let mut state = state(cursor, &[root])?;
    let mut page = state.walks[0].advance(engine, limit)?;
    if !state.walks[0].pending.is_empty() {
        page.next = Some(encode(&state)?);
    }
    Ok(page)
}

/// Classifies only after both reachable histories are available. Missing source
/// returns its object IDs and retains a resumable inspection state.
pub fn relation_page<R: SourceRead + ?Sized>(
    engine: &R,
    left: &str,
    right: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<RelationPage> {
    validate_limit(limit)?;
    let mut state = state(cursor, &[left, right])?;
    let mut needed = BTreeSet::new();
    for walk in &mut state.walks {
        needed.extend(walk.advance(engine, limit)?.needed_objects);
    }
    let pending = state.walks.iter().any(|walk| !walk.pending.is_empty());
    let relation = if !needed.is_empty() {
        HeadRelation::Incomplete
    } else if pending {
        HeadRelation::Pending
    } else if left == right {
        HeadRelation::Equal
    } else if state.walks[1].seen.contains(left) {
        HeadRelation::LeftAncestor
    } else if state.walks[0].seen.contains(right) {
        HeadRelation::RightAncestor
    } else {
        HeadRelation::Diverged
    };
    Ok(RelationPage {
        relation,
        needed_objects: needed.into_iter().collect(),
        next: pending.then(|| encode(&state)).transpose()?,
    })
}

fn validate_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_HISTORY_PAGE).contains(&limit) {
        return Err(Error::Limit(format!(
            "history page limit must be between 1 and {MAX_HISTORY_PAGE}"
        )));
    }
    Ok(())
}

fn state(cursor: Option<&str>, roots: &[&str]) -> Result<Cursor> {
    for root in roots {
        validate_id(root)?;
    }
    let state = match cursor {
        None => Cursor {
            version: 1,
            walks: roots
                .iter()
                .map(|root| Walk::new(root))
                .collect::<Result<_>>()?,
        },
        Some(cursor) => decode(cursor)?,
    };
    if state.version != 1
        || state.walks.len() != roots.len()
        || state
            .walks
            .iter()
            .zip(roots)
            .any(|(walk, root)| walk.root != *root)
    {
        return Err(Error::Invalid(
            "history cursor does not match requested roots".into(),
        ));
    }
    for walk in &state.walks {
        walk.validate()?;
    }
    Ok(state)
}

fn encode(state: &Cursor) -> Result<String> {
    let body = serde_json::to_string(state)?;
    let cursor = format!("{}:{body}", digest(body.as_bytes()));
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(Error::Limit("history cursor exceeds byte bound".into()));
    }
    Ok(cursor)
}

fn decode(cursor: &str) -> Result<Cursor> {
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(Error::Limit("history cursor exceeds byte bound".into()));
    }
    let (checksum, body) = cursor
        .split_once(':')
        .ok_or_else(|| Error::Invalid("malformed history cursor".into()))?;
    validate_id(checksum)?;
    if digest(body.as_bytes()) != checksum {
        return Err(Error::Invalid("history cursor checksum mismatch".into()));
    }
    Ok(serde_json::from_str(body)?)
}
