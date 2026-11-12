//! The digest: what one person's agents left unfinished, and where agents in a repository keep
//! colliding, over a period of days. It is a read-only projection of records the Hub already
//! keeps: the reader owner's unfinished Work, the `edit_denied` and `edit_repeated` events, the
//! `intent_announced` and `handoff_request` events that say who held a path, and live intents.
//!
//! Nothing is written. A count covers only the history that was read: a period that starts
//! before the oldest retained event, or holds more events than one digest reads, is reported
//! as incomplete with the time the counts start at, never as zero. Work titles, branch names,
//! paths and logins are other people's text, bounded and cleaned with [`sanitize`].
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::advice::sanitize;
use super::intent::{Intent, key_of, overlaps};
use super::store::{ActivityEvent, Principal, Store};
use super::viewer::Viewer;
use super::work::{AUTO_PAUSE_SUMMARY, WorkItem, WorkState};
use super::work_lifecycle::{
    ActorVia, MergeStatus, ReservationStatus, WorkActorKind, WorkTransitionKind, decide_transition,
    operational_page,
};
use super::work_recovery::{MAX_RECOVERY_PAGE, RecoveryRead};
use super::work_transition::WorkLifecycleReader;
use super::{HubError, Result};

pub const DIGEST_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_DAYS: u32 = 7;
pub const MAX_DAYS: u32 = 31;
/// Unfinished Work is stale once its lead has not checked in for this many days.
pub const QUIET_DAYS: i64 = 3;
/// Two people are a colliding pair once their agents collided on one path this many times.
pub const PAIR_COLLISIONS: u64 = 3;
/// The most rows each list of a digest holds.
pub const MAX_LISTED: usize = 10;

const DAY: i64 = 86_400;
const EVENT_PAGE: usize = 1000;
/// Event pages one digest reads, newest first; a busier period is reported as truncated.
const MAX_EVENT_PAGES: usize = 10;
/// Pages of the owner's unfinished Work one digest reads.
const MAX_WORK_PAGES: usize = 4;
const MAX_LIVE_INTENTS: usize = 1000;
/// Paths of one event that are counted; an event naming more is reported as truncated.
const MAX_EVENT_PATHS: usize = 32;
const TITLE_CHARS: usize = 120;
const PATH_CHARS: usize = 120;
const NAME_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DigestQuery {
    /// The period, in days up to now.
    #[serde(default = "default_days")]
    pub days: u32,
}

const fn default_days() -> u32 {
    DEFAULT_DAYS
}

impl Default for DigestQuery {
    fn default() -> Self {
        Self { days: DEFAULT_DAYS }
    }
}

/// The reply of the digest route. Readers ignore fields they do not know.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkDigestV1 {
    pub schema_version: u16,
    pub repository: String,
    pub days: u32,
    pub since: i64,
    pub until: i64,
    pub stale: StaleWork,
    pub collisions: Collisions,
}

/// The reader owner's unfinished Work whose lead has been quiet for `quiet_days` or more.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StaleWork {
    pub quiet_days: i64,
    /// Longest quiet first, at most [`MAX_LISTED`].
    pub items: Vec<StaleItem>,
    /// How many stale items were found, listed or not.
    pub total: u64,
    /// False when the owner has more unfinished Work than one digest reads.
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StaleItem {
    pub work_id: String,
    pub title: String,
    pub branch: Option<String>,
    pub state: WorkState,
    /// The lead's last check-in. For Work paused because its session ended, the last check-in
    /// before that pause.
    pub quiet_since: i64,
    /// Whether a pull request from its branch merged: `observed`, `none` or `unknown`.
    pub merged: MergeStatus,
    /// The fences a transition of this item needs.
    pub generation: i64,
    pub updated_seq: i64,
    /// Whether its owner, acting as a person, may release or complete it now.
    pub can_release: bool,
    pub can_complete: bool,
}

/// Refused and repeated edits in the period, for the whole repository.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Collisions {
    /// True when the counts cover the whole period.
    pub complete: bool,
    /// The time the counts start at: `since` when complete, later when older history is not
    /// retained or was not read, absent when no history is retained at all.
    pub known_from: Option<i64>,
    /// Why the counts are incomplete, as stable words.
    pub limits: Vec<String>,
    pub refused: u64,
    pub repeated: u64,
    /// Most collisions first, at most [`MAX_LISTED`].
    pub hot_paths: Vec<HotPath>,
    pub hot_paths_complete: bool,
    /// Most collisions first, at most [`MAX_LISTED`].
    pub pairs: Vec<CollidingPair>,
    pub pairs_complete: bool,
    /// Collisions whose holding side could not be named, so no pair counts them.
    pub unattributed: u64,
}

/// Paths that share their first two segments.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HotPath {
    pub path: String,
    pub refused: u64,
    pub repeated: u64,
    /// The median time between the holder taking the path and the other agent's edit, over
    /// the collisions where the Hub knows when the holder took it; absent when it knows none.
    pub typical_gap_seconds: Option<i64>,
    pub gap_samples: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CollidingPair {
    /// Two logins, sorted.
    pub owners: [String; 2],
    pub path: String,
    pub collisions: u64,
    /// One of the two is the reader.
    pub includes_you: bool,
}

impl WorkDigestV1 {
    /// Nothing to act on or look at: no stale Work, no hot path and no pair.
    pub fn is_empty(&self) -> bool {
        self.stale.items.is_empty()
            && self.collisions.hot_paths.is_empty()
            && self.collisions.pairs.is_empty()
    }
}

/// The digest for `who`: their own stale Work, and the collisions of their repository.
pub fn read(
    store: &dyn Store,
    who: &Principal,
    query: &DigestQuery,
    now: i64,
) -> Result<WorkDigestV1> {
    if !(1..=MAX_DAYS).contains(&query.days) {
        return Err(HubError::Invalid(format!("days must be 1 to {MAX_DAYS}")));
    }
    let since = now - i64::from(query.days) * DAY;
    let (events, coverage) = events(store, who, since)?;
    let live = store.live_intents_scoped(who, now, MAX_LIVE_INTENTS)?;
    Ok(WorkDigestV1 {
        schema_version: DIGEST_SCHEMA_VERSION,
        repository: sanitize(&who.repository, PATH_CHARS),
        days: query.days,
        since,
        until: now,
        stale: stale(store, who, now)?,
        collisions: collisions(&events, &live, (since, now), coverage, &who.owner.login),
    })
}

/// When the lead last checked in. The pause a session end writes is not a check-in.
fn quiet_since(item: &WorkItem) -> i64 {
    let auto_paused = item.state == WorkState::Paused
        && item
            .current
            .as_ref()
            .is_some_and(|report| report.summary == AUTO_PAUSE_SUMMARY);
    if auto_paused {
        item.reported_at.unwrap_or(item.created_at)
    } else {
        item.last_checkin_at
    }
}

fn stale(store: &dyn Store, who: &Principal, now: i64) -> Result<StaleWork> {
    let reader = WorkLifecycleReader::Token(&who.token_id);
    let mut found = Vec::new();
    let mut after = None;
    let mut complete = false;
    for _ in 0..MAX_WORK_PAGES {
        let read = RecoveryRead {
            work_id: None,
            after,
            limit: MAX_RECOVERY_PAGE,
            paths: Vec::new(),
            all_owners: false,
        };
        let facts = store.work_recovery_read(&reader, &read, now)?;
        let items: BTreeMap<String, WorkItem> = facts
            .rows
            .iter()
            .filter(|row| facts.page.contains(&row.row.item.id))
            .map(|row| (row.row.item.id.clone(), row.row.item.clone()))
            .collect();
        let views = operational_page(
            facts.rows.into_iter().map(|row| row.row).collect(),
            facts.forge_window_from,
            false,
            now,
            Viewer(&facts.viewer.owner),
        );
        for view in views.work {
            let Some(item) = items.get(&view.work_id) else {
                continue;
            };
            let quiet_since = quiet_since(item);
            if item.owner != who.owner.id
                || item.state == WorkState::Finished
                || now - quiet_since < QUIET_DAYS * DAY
            {
                continue;
            }
            let held = view.reservation.status != ReservationStatus::None;
            let may = |kind| {
                decide_transition(
                    kind,
                    WorkActorKind::Owner,
                    ActorVia::Person,
                    item.state,
                    held,
                )
                .is_ok()
            };
            found.push(StaleItem {
                work_id: item.id.clone(),
                title: sanitize(&item.title, TITLE_CHARS),
                branch: item
                    .branch
                    .as_deref()
                    .map(|branch| sanitize(branch, PATH_CHARS)),
                state: item.state,
                quiet_since,
                merged: view.merge.status,
                generation: item.generation,
                updated_seq: item.updated_seq,
                can_release: may(WorkTransitionKind::Release),
                can_complete: may(WorkTransitionKind::Complete),
            });
        }
        after = facts.page.last().cloned();
        if !facts.more || after.is_none() {
            complete = true;
            break;
        }
    }
    found.sort_by(|a, b| (a.quiet_since, &a.work_id).cmp(&(b.quiet_since, &b.work_id)));
    let total = found.len() as u64;
    found.truncate(MAX_LISTED);
    Ok(StaleWork {
        quiet_days: QUIET_DAYS,
        items: found,
        total,
        complete,
    })
}

/// What the event read covers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Coverage {
    known_from: Option<i64>,
    limits: BTreeSet<String>,
}

/// The repository's activity events back to `since`, newest first, and what they cover.
fn events(
    store: &dyn Store,
    who: &Principal,
    since: i64,
) -> Result<(Vec<ActivityEvent>, Coverage)> {
    let mut all: Vec<ActivityEvent> = Vec::new();
    let mut before = None;
    let mut reached = false;
    for _ in 0..MAX_EVENT_PAGES {
        let page = store.repository_events(who, before, EVENT_PAGE)?;
        let full = page.len() == EVENT_PAGE;
        all.extend(page);
        let oldest = all.last().map(|event| (event.event.seq, event.event.at));
        match oldest {
            Some((seq, at)) if full && at >= since => before = Some(seq),
            _ => {
                reached = true;
                break;
            }
        }
    }
    let mut coverage = Coverage::default();
    if !reached {
        coverage.known_from = all.last().map(|event| event.event.at);
        coverage.limits.insert("collision_history_truncated".into());
        return Ok((all, coverage));
    }
    // Activity events are never pruned one by one, so the oldest event the Hub retains bounds
    // what it can know: a new or restored Hub has no history before it.
    match store.events_after(0, 1)?.first() {
        Some(oldest) if oldest.at <= since => coverage.known_from = Some(since),
        Some(oldest) => {
            coverage.known_from = Some(oldest.at);
            coverage
                .limits
                .insert("event_history_starts_in_period".into());
        }
        None => {
            coverage.limits.insert("event_history_empty".into());
        }
    }
    Ok((all, coverage))
}

/// The first two segments of a path: `src/hub/slack.rs` is `src/hub`.
fn group(path: &str) -> String {
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    match (parts.next(), parts.next()) {
        (Some(first), Some(second)) => format!("{first}/{second}"),
        (Some(first), None) => first.to_owned(),
        _ => String::new(),
    }
}

/// One refused or repeated edit, with cleaned text.
struct Edit<'a> {
    at: i64,
    token: &'a str,
    owner: String,
    session: String,
    paths: Vec<String>,
    /// The holders' intent ids a refusal named.
    holders: Vec<String>,
    repeated: bool,
}

/// A holder: its login and, when known, when it took the path.
type Holder = (String, Option<i64>);

/// An earlier refusal of one session: its paths and who held them.
type Refused<'a> = (&'a [String], Vec<Holder>);

#[derive(Default)]
struct Hot {
    refused: u64,
    repeated: u64,
    gaps: Vec<i64>,
}

fn strings(value: &Value, max_chars: usize) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|text| sanitize(text, max_chars))
        .collect()
}

/// Counts the refused and repeated edits of `events` (newest first) inside `window`.
///
/// The editing side is the event's own token. The holding side of a refusal is whoever announced
/// the intents it names, found among the announcements read and the live intents; a repeat has the
/// holders of the same session's latest earlier refusal over one of its paths. Where neither
/// names anyone, the hand-over request filed for the refused file does.
pub(super) fn collisions(
    events: &[ActivityEvent],
    live: &[Intent],
    window: (i64, i64),
    coverage: Coverage,
    you: &str,
) -> Collisions {
    let (since, until) = window;
    let Coverage {
        known_from,
        mut limits,
    } = coverage;
    let mut starts: BTreeMap<&str, Holder> = live
        .iter()
        .map(|intent| {
            let holder = (
                sanitize(&intent.owner.login, NAME_CHARS),
                Some(intent.created_at),
            );
            (intent.id.as_str(), holder)
        })
        .collect();
    let mut asked: BTreeMap<(&str, String, String), String> = BTreeMap::new();
    let mut edits = Vec::new();
    for event in events.iter().rev() {
        let kind = event.event.kind.as_str();
        if kind == "intent_announced" {
            starts
                .entry(event.event.subject.as_str())
                .or_insert_with(|| (sanitize(&event.owner, NAME_CHARS), Some(event.event.at)));
            continue;
        }
        if !matches!(kind, "edit_denied" | "edit_repeated" | "handoff_request") {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&event.event.data) else {
            limits.insert("collision_event_unreadable".into());
            continue;
        };
        let session = data["session"].as_str().unwrap_or_default().to_owned();
        let mut paths = strings(&data["paths"], PATH_CHARS);
        if paths.len() > MAX_EVENT_PATHS {
            limits.insert("collision_event_paths_truncated".into());
            paths.truncate(MAX_EVENT_PATHS);
        }
        if kind == "handoff_request" {
            if let Some(login) = data["to"]["login"].as_str() {
                for path in paths {
                    asked.insert(
                        (event.token.as_str(), session.clone(), path),
                        sanitize(login, NAME_CHARS),
                    );
                }
            }
            continue;
        }
        edits.push(Edit {
            at: event.event.at,
            token: &event.token,
            owner: sanitize(&event.owner, NAME_CHARS),
            session,
            paths,
            holders: strings(&data["holders"], NAME_CHARS),
            repeated: kind == "edit_repeated",
        });
    }

    let mut refusals: BTreeMap<(&str, &str), Vec<Refused<'_>>> = BTreeMap::new();
    let mut hot: BTreeMap<String, Hot> = BTreeMap::new();
    let mut pairs: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    let (mut refused, mut repeated, mut unattributed) = (0, 0, 0);
    for edit in &edits {
        let same = (edit.token, edit.session.as_str());
        let touches = |paths: &[String]| {
            paths.iter().any(|earlier| {
                edit.paths
                    .iter()
                    .any(|path| overlaps(key_of(earlier), key_of(path)))
            })
        };
        let mut holders: Vec<Holder> = if edit.repeated {
            refusals
                .get(&same)
                .and_then(|earlier| earlier.iter().rev().find(|(paths, _)| touches(paths)))
                .map(|(_, holders)| holders.clone())
                .unwrap_or_default()
        } else {
            edit.holders
                .iter()
                .filter_map(|id| starts.get(id.as_str()).cloned())
                .collect()
        };
        if holders.is_empty() {
            holders = edit
                .paths
                .iter()
                .filter_map(|path| asked.get(&(edit.token, edit.session.clone(), path.clone())))
                .map(|login| (login.clone(), None))
                .collect();
        }
        if !edit.repeated {
            refusals
                .entry(same)
                .or_default()
                .push((edit.paths.as_slice(), holders.clone()));
        }
        if edit.at < since || edit.at > until {
            continue;
        }
        if edit.repeated {
            repeated += 1;
        } else {
            refused += 1;
        }
        let groups: BTreeSet<String> = edit.paths.iter().map(|path| group(path)).collect();
        for group in groups {
            let entry = hot.entry(group).or_default();
            if edit.repeated {
                entry.repeated += 1;
            } else {
                entry.refused += 1;
            }
            entry.gaps.extend(
                holders
                    .iter()
                    .filter_map(|(_, start)| start.map(|start| edit.at - start))
                    .filter(|gap| *gap >= 0),
            );
        }
        if holders.is_empty() {
            unattributed += 1;
        }
        // One person's two agents collide too, but they are not a pair of people.
        let others: BTreeSet<&str> = holders
            .iter()
            .map(|(login, _)| login.as_str())
            .filter(|login| *login != edit.owner)
            .collect();
        for other in others {
            let (first, second) = if other < edit.owner.as_str() {
                (other, edit.owner.as_str())
            } else {
                (edit.owner.as_str(), other)
            };
            for path in &edit.paths {
                *pairs
                    .entry((first.to_owned(), second.to_owned(), path.clone()))
                    .or_default() += 1;
            }
        }
    }

    let mut hot_paths: Vec<HotPath> = hot
        .into_iter()
        .map(|(path, mut entry)| {
            entry.gaps.sort_unstable();
            HotPath {
                path,
                refused: entry.refused,
                repeated: entry.repeated,
                typical_gap_seconds: entry
                    .gaps
                    .get(entry.gaps.len().saturating_sub(1) / 2)
                    .copied(),
                gap_samples: entry.gaps.len() as u64,
            }
        })
        .collect();
    hot_paths
        .sort_by(|a, b| (b.refused + b.repeated, &a.path).cmp(&(a.refused + a.repeated, &b.path)));
    let hot_paths_complete = hot_paths.len() <= MAX_LISTED;
    hot_paths.truncate(MAX_LISTED);
    let you = sanitize(you, NAME_CHARS);
    let mut pairs: Vec<CollidingPair> = pairs
        .into_iter()
        .filter(|(_, collisions)| *collisions >= PAIR_COLLISIONS)
        .map(|((first, second, path), collisions)| CollidingPair {
            includes_you: first == you || second == you,
            owners: [first, second],
            path,
            collisions,
        })
        .collect();
    pairs.sort_by(|a, b| {
        (b.collisions, &a.owners, &a.path).cmp(&(a.collisions, &b.owners, &b.path))
    });
    let pairs_complete = pairs.len() <= MAX_LISTED;
    pairs.truncate(MAX_LISTED);
    Collisions {
        complete: limits.is_empty(),
        known_from,
        limits: limits.into_iter().collect(),
        refused,
        repeated,
        hot_paths,
        hot_paths_complete,
        pairs,
        pairs_complete,
        unattributed,
    }
}

/// `2026-11-09`, in UTC.
pub fn day(epoch: i64) -> String {
    let (year, month, day, ..) = crate::civil::utc_civil(epoch);
    format!("{year:04}-{month:02}-{day:02}")
}

fn count(n: i64, unit: &str) -> String {
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// A length of time in its largest whole unit: `40 minutes`, `3 hours`, `2 days`.
pub fn span(seconds: i64) -> String {
    match seconds {
        ..60 => "under a minute".into(),
        60..3600 => count(seconds / 60, "minute"),
        3600..DAY => count(seconds / 3600, "hour"),
        _ => count(seconds / DAY, "day"),
    }
}

/// What a merge observation says about a stale item's branch.
pub fn merge_phrase(status: MergeStatus) -> &'static str {
    match status {
        MergeStatus::Observed => "a pull request from its branch merged",
        MergeStatus::None => "no pull request from its branch has merged",
        MergeStatus::Unknown => "whether a pull request from its branch merged is unknown",
    }
}

fn state_word(state: WorkState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// `paused, branch feat/x; quiet 5 days; a pull request from its branch merged`.
pub fn stale_detail(item: &StaleItem, now: i64) -> String {
    let branch = item.branch.as_ref().map_or_else(
        || "no branch".to_owned(),
        |branch| format!("branch {branch}"),
    );
    format!(
        "{}, {branch}; quiet {}; {}",
        state_word(item.state),
        span(now - item.quiet_since),
        merge_phrase(item.merged)
    )
}

/// `src/hub: 4 refused, 2 repeated; typically 40 minutes after the holder took the path`.
pub fn hot_path_line(path: &HotPath) -> String {
    let gap = path.typical_gap_seconds.map_or_else(
        || "how long the holder had held the path is unknown".to_owned(),
        |gap| format!("typically {} after the holder took the path", span(gap)),
    );
    format!(
        "{}: {} refused, {} repeated; {gap}",
        path.path, path.refused, path.repeated
    )
}

/// `alice and bob: src/hub/slack.rs, 4 times`.
pub fn pair_line(pair: &CollidingPair) -> String {
    format!(
        "{} and {}: {}, {} times",
        pair.owners[0], pair.owners[1], pair.path, pair.collisions
    )
}

/// What the collision counts cover, absent when they cover the whole period.
pub fn coverage_line(collisions: &Collisions) -> Option<String> {
    if collisions.complete {
        return None;
    }
    let reason = if collisions
        .limits
        .iter()
        .any(|limit| limit == "collision_history_truncated")
    {
        "the period holds more events than one digest reads"
    } else {
        "the Hub retains no earlier events"
    };
    Some(match collisions.known_from {
        Some(from) => format!(
            "Collisions are counted from {} only: {reason}. Earlier ones are unknown, not zero.",
            day(from)
        ),
        None => "Collisions are unknown: the Hub retains no events.".to_owned(),
    })
}

/// The digest as plain text for a terminal.
pub fn render_text(digest: &WorkDigestV1) -> String {
    let mut out = vec![format!(
        "Digest for {}: {} to {} (UTC).",
        digest.repository,
        day(digest.since),
        day(digest.until)
    )];
    let stale = &digest.stale;
    out.push(String::new());
    if stale.items.is_empty() {
        out.push(format!(
            "None of your unfinished Work has been quiet for {} days or more.",
            stale.quiet_days
        ));
    } else {
        out.push(format!(
            "Your unfinished Work, quiet for {} days or more: {}",
            stale.quiet_days, stale.total
        ));
        for item in &stale.items {
            out.push(format!(
                "- {} \"{}\": {}.",
                item.work_id,
                item.title,
                stale_detail(item, digest.until)
            ));
        }
        let more = stale.total - stale.items.len() as u64;
        if more > 0 {
            out.push(format!("+{more} more"));
        }
    }
    if !stale.complete {
        out.push(
            "You have more unfinished Work than one digest reads; this list may miss some.".into(),
        );
    }
    let collisions = &digest.collisions;
    out.push(String::new());
    if collisions.hot_paths.is_empty() {
        out.push("No refused or repeated edits are recorded.".into());
    } else {
        out.push(format!(
            "Paths with the most refused or repeated edits ({} refused, {} repeated in all):",
            collisions.refused, collisions.repeated
        ));
        out.extend(
            collisions
                .hot_paths
                .iter()
                .map(|path| format!("- {}", hot_path_line(path))),
        );
        if !collisions.hot_paths_complete {
            out.push("More paths had collisions than are listed.".into());
        }
    }
    if !collisions.pairs.is_empty() {
        out.push(String::new());
        out.push(format!(
            "People whose agents collided on one path {PAIR_COLLISIONS} or more times:"
        ));
        out.extend(
            collisions
                .pairs
                .iter()
                .map(|pair| format!("- {}", pair_line(pair))),
        );
        if !collisions.pairs_complete {
            out.push("More pairs collided than are listed.".into());
        }
    }
    if collisions.unattributed > 0 {
        out.push(format!(
            "{} could not be matched to who held the path, so no pair counts them.",
            count(collisions.unattributed as i64, "collision")
        ));
    }
    out.extend(coverage_line(collisions));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::HubEvent;
    use serde_json::json;

    const SINCE: i64 = 1_000_000;
    const UNTIL: i64 = SINCE + 7 * DAY;

    /// Builds events oldest first; `collisions` reads them newest first.
    #[derive(Default)]
    struct Log(Vec<ActivityEvent>);

    impl Log {
        fn push(&mut self, at: i64, kind: &str, subject: &str, who: (&str, &str), data: Value) {
            let seq = self.0.len() as i64 + 1;
            self.0.push(ActivityEvent {
                event: HubEvent {
                    seq,
                    id: format!("evt_{seq}"),
                    at,
                    kind: kind.into(),
                    subject: subject.into(),
                    data: data.to_string(),
                },
                token: who.0.into(),
                agent: "agent".into(),
                owner: who.1.into(),
            });
        }

        fn announce(&mut self, at: i64, intent: &str, who: (&str, &str)) {
            self.push(at, "intent_announced", intent, who, json!({}));
        }

        fn refuse(&mut self, at: i64, who: (&str, &str), path: &str, holders: &[&str]) {
            let data = json!({"token": who.0, "session": "s", "paths": [path], "holders": holders});
            self.push(at, "edit_denied", who.0, who, data);
        }

        fn repeat(&mut self, at: i64, who: (&str, &str), path: &str) {
            let data = json!({"token": who.0, "session": "s", "paths": [path]});
            self.push(at, "edit_repeated", who.0, who, data);
        }

        fn read(&self, coverage: Coverage) -> Collisions {
            let newest_first: Vec<ActivityEvent> = self.0.iter().rev().cloned().collect();
            collisions(&newest_first, &[], (SINCE, UNTIL), coverage, "bob")
        }
    }

    fn whole() -> Coverage {
        Coverage {
            known_from: Some(SINCE),
            limits: BTreeSet::new(),
        }
    }

    const ALICE: (&str, &str) = ("tok_a", "alice");
    const BOB: (&str, &str) = ("tok_b", "bob");

    #[test]
    fn an_empty_period_is_a_known_zero_only_with_whole_history() {
        let none = Log::default().read(whole());
        assert!(none.complete && none.limits.is_empty());
        assert_eq!(none.known_from, Some(SINCE));
        assert_eq!((none.refused, none.repeated, none.unattributed), (0, 0, 0));
        assert!(none.hot_paths.is_empty() && none.pairs.is_empty());
        assert_eq!(coverage_line(&none), None);
    }

    #[test]
    fn a_retention_gap_is_reported_as_unknown_never_as_zero() {
        let retained_from = SINCE + 2 * DAY;
        let gap = Coverage {
            known_from: Some(retained_from),
            limits: ["event_history_starts_in_period".to_owned()].into(),
        };
        let read = Log::default().read(gap);
        assert!(!read.complete);
        assert_eq!(read.known_from, Some(retained_from));
        assert_eq!(read.limits, ["event_history_starts_in_period"]);
        let line = coverage_line(&read).unwrap();
        assert!(line.contains(&day(retained_from)), "{line}");
        assert!(line.contains("unknown, not zero"), "{line}");

        let truncated = Coverage {
            known_from: Some(retained_from),
            limits: ["collision_history_truncated".to_owned()].into(),
        };
        let line = coverage_line(&Log::default().read(truncated)).unwrap();
        assert!(line.contains("more events than one digest reads"), "{line}");

        let nothing = Coverage {
            known_from: None,
            limits: ["event_history_empty".to_owned()].into(),
        };
        let read = Log::default().read(nothing);
        assert_eq!(read.known_from, None);
        assert!(
            coverage_line(&read)
                .unwrap()
                .starts_with("Collisions are unknown")
        );
    }

    #[test]
    fn collisions_group_by_two_segments_and_pair_at_three() {
        let mut log = Log::default();
        log.announce(SINCE + 100, "int_a", ALICE);
        // Bob is refused once and repeats twice on one file: three collisions with alice.
        log.refuse(SINCE + 700, BOB, "src/hub/slack.rs", &["int_a"]);
        log.repeat(SINCE + 800, BOB, "src/hub/slack.rs");
        log.repeat(SINCE + 900, BOB, "src/hub/slack.rs");
        // Two collisions on another file of the same group stay below the pair threshold.
        log.refuse(SINCE + 1000, BOB, "src/hub/mod.rs", &["int_a"]);
        log.repeat(SINCE + 1100, BOB, "src/hub/mod.rs");
        log.refuse(SINCE + 1200, BOB, "README.md", &["int_a"]);
        let read = log.read(whole());
        assert_eq!((read.refused, read.repeated, read.unattributed), (3, 3, 0));
        let hub = &read.hot_paths[0];
        assert_eq!(
            (hub.path.as_str(), hub.refused, hub.repeated),
            ("src/hub", 2, 3)
        );
        // Gaps 600, 700, 800, 900 and 1000 seconds after alice announced: the median is 800.
        assert_eq!((hub.typical_gap_seconds, hub.gap_samples), (Some(800), 5));
        assert_eq!(read.hot_paths[1].path, "README.md");
        assert_eq!(
            read.pairs,
            [CollidingPair {
                owners: ["alice".into(), "bob".into()],
                path: "src/hub/slack.rs".into(),
                collisions: 3,
                includes_you: true,
            }]
        );
        assert_eq!(
            pair_line(&read.pairs[0]),
            "alice and bob: src/hub/slack.rs, 3 times"
        );
        assert_eq!(
            hot_path_line(hub),
            "src/hub: 2 refused, 3 repeated; typically 13 minutes after the holder took the path"
        );
    }

    #[test]
    fn an_unknown_holder_is_unattributed_and_a_hand_over_request_names_one() {
        let mut log = Log::default();
        // The holder's intent was announced before anything this digest read.
        log.refuse(SINCE + 10, BOB, "src/a.rs", &["int_old"]);
        log.repeat(SINCE + 20, BOB, "src/a.rs");
        // A repeat with no earlier refusal in the history read.
        log.repeat(SINCE + 30, BOB, "src/b.rs");
        let read = log.read(whole());
        assert_eq!((read.refused, read.repeated, read.unattributed), (1, 2, 3));
        assert!(read.pairs.is_empty());
        assert_eq!(read.hot_paths[0].typical_gap_seconds, None);
        assert!(hot_path_line(&read.hot_paths[0]).contains("is unknown"));

        // The refusal filed a hand-over request, which names the holder but not since when.
        let request = json!({"request": "r", "paths": ["src/a.rs"], "session": "s",
            "to": {"agent": "claude", "login": "alice", "session": null}});
        log.push(SINCE + 11, "handoff_request", BOB.0, BOB, request);
        log.repeat(SINCE + 40, BOB, "src/a.rs");
        let read = log.read(whole());
        assert_eq!(read.unattributed, 1, "only the repeat on src/b.rs");
        assert_eq!(read.pairs[0].collisions, 3);
        assert_eq!(read.hot_paths[0].gap_samples, 0);
    }

    #[test]
    fn one_persons_own_agents_and_edits_outside_the_period_make_no_pair() {
        let mut log = Log::default();
        log.announce(SINCE - 500, "int_a", ALICE);
        let other_agent = ("tok_a2", "alice");
        for at in [10, 20, 30] {
            log.refuse(SINCE + at, other_agent, "src/a.rs", &["int_a"]);
        }
        // Before the period: it names the holders of a later repeat, and is not counted itself.
        log.refuse(SINCE - 100, BOB, "src/c.rs", &["int_a"]);
        log.repeat(SINCE + 50, BOB, "src/c.rs");
        let read = log.read(whole());
        assert_eq!((read.refused, read.repeated, read.unattributed), (3, 1, 0));
        assert!(read.pairs.is_empty(), "{:?}", read.pairs);
        let listed: Vec<&str> = read.hot_paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(listed, ["src/a.rs", "src/c.rs"]);
    }

    #[test]
    fn other_peoples_text_is_bounded_and_cleaned() {
        let hostile = format!("src/\u{202e}\u{7}\"{}", "x".repeat(400));
        let mut log = Log::default();
        log.announce(SINCE + 1, "int_a", ("tok_a", "ali\u{200b}ce\u{7}"));
        for at in [10, 20, 30] {
            log.refuse(SINCE + at, BOB, &hostile, &["int_a"]);
        }
        log.refuse(SINCE + 40, BOB, "src/unreadable.rs", &["int_a"]);
        log.0.last_mut().unwrap().event.data = "{".into();
        let read = log.read(whole());
        assert_eq!(read.limits, ["collision_event_unreadable"]);
        assert!(!read.complete);
        let pair = &read.pairs[0];
        assert_eq!(pair.owners, ["alice".to_owned(), "bob".to_owned()]);
        for text in [&pair.path, &read.hot_paths[0].path] {
            assert!(text.chars().count() <= PATH_CHARS + 1, "{text}");
            assert!(!text.contains(['\u{202e}', '\u{7}', '"']), "{text}");
        }
    }

    #[test]
    fn lists_are_bounded_and_say_when_more_exist() {
        let mut log = Log::default();
        log.announce(SINCE + 1, "int_a", ALICE);
        for index in 0..=MAX_LISTED as i64 {
            for at in [10, 20, 30] {
                let path = format!("src/dir{index}/file.rs");
                log.refuse(SINCE + 100 * index + at, BOB, &path, &["int_a"]);
            }
        }
        let read = log.read(whole());
        assert_eq!(read.hot_paths.len(), MAX_LISTED);
        assert_eq!(read.pairs.len(), MAX_LISTED);
        assert!(!read.hot_paths_complete && !read.pairs_complete);
        assert!(read.complete, "a bounded list is not missing history");
    }

    #[test]
    fn text_names_spans_days_and_merge_evidence() {
        assert_eq!(span(59), "under a minute");
        assert_eq!(span(60), "1 minute");
        assert_eq!(span(2 * 3600 + 5), "2 hours");
        assert_eq!(span(5 * DAY), "5 days");
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(group("src/hub/slack.rs"), "src/hub");
        assert_eq!(group("docs/hub.md"), "docs/hub.md");
        assert_eq!(group("README.md"), "README.md");
        let item = StaleItem {
            work_id: "wrk_1".into(),
            title: "Add the widget".into(),
            branch: Some("feat/widget".into()),
            state: WorkState::Paused,
            quiet_since: UNTIL - 5 * DAY,
            merged: MergeStatus::Observed,
            generation: 1,
            updated_seq: 2,
            can_release: false,
            can_complete: true,
        };
        assert_eq!(
            stale_detail(&item, UNTIL),
            "paused, branch feat/widget; quiet 5 days; a pull request from its branch merged"
        );
        let digest = WorkDigestV1 {
            schema_version: DIGEST_SCHEMA_VERSION,
            repository: "acme/widgets".into(),
            days: 7,
            since: SINCE,
            until: UNTIL,
            stale: StaleWork {
                quiet_days: QUIET_DAYS,
                items: vec![item],
                total: 12,
                complete: false,
            },
            collisions: Log::default().read(whole()),
        };
        assert!(!digest.is_empty());
        let text = render_text(&digest);
        assert!(text.contains("quiet for 3 days or more: 12"), "{text}");
        assert!(
            text.contains("- wrk_1 \"Add the widget\": paused"),
            "{text}"
        );
        assert!(text.contains("+11 more"), "{text}");
        assert!(
            text.contains("more unfinished Work than one digest reads"),
            "{text}"
        );
        assert!(
            text.contains("No refused or repeated edits are recorded."),
            "{text}"
        );
    }
}
