//! The web pages' repository list: for each connected repository a person can read now, counts of
//! unfinished work, what needs a person, live agents and the last activity. Counts only: titles,
//! paths and people stay behind each repository's own read token. The rules mirror the status
//! page's (`ui/src/needs.ts` and `ui/src/derive.ts`), so a row and its Dashboard count alike.
//! The separate device catalog adds bounded role-checked Work and live-agent details.
use std::collections::BTreeSet;

use serde::Serialize;

use super::intent::{Intent, key_of, overlaps};
use super::session::same_agent;
use super::store::SessionView;
use super::work::{ClaimStatus, WorkOverview, WorkState};

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Unfinished {
    pub total: usize,
    /// Active, with a lead that is checking in.
    pub moving: usize,
    /// Blocked, or a lead gone quiet while still holding it.
    pub stuck: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RepositorySummary {
    pub repository: String,
    pub repository_id: i64,
    pub provider: String,
    pub unfinished: Unfinished,
    /// Blockers, quiet leads, handovers, declared-scope overlaps and files two agents hold.
    /// Pull-request overlaps and files with only a recent refusal are not counted.
    pub needs_you: usize,
    pub agents: usize,
    pub last_activity_at: Option<i64>,
    /// False when a list was cut short, so the counts are lower bounds.
    pub complete: bool,
    /// Only used to form the computer catalog's change marker; never serialized.
    #[serde(skip)]
    pub(crate) contested_markers: Vec<String>,
    #[serde(skip)]
    pub(crate) attention_categories: AttentionCategories,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AttentionCategories {
    pub work: usize,
    pub overlaps: usize,
    pub contested_files: usize,
}

/// What the Hub read for one repository.
pub(super) struct Facts {
    pub overview: WorkOverview,
    pub intents: Vec<Intent>,
    pub sessions: Vec<SessionView>,
    /// Whether `intents` and `sessions` are every live one.
    pub live_complete: bool,
    pub last_event_at: Option<i64>,
}

pub(super) fn summarize(
    repository: String,
    repository_id: i64,
    provider: String,
    facts: &Facts,
) -> RepositorySummary {
    let current = &facts.overview.current;
    let (unfinished, decisions) =
        work_counts(current.iter().map(|item| (&item.state, &item.claim_status)));
    let (contested_count, contested_markers) = contested_files_and_markers(&facts.intents);
    let needs_you = decisions + facts.overview.overlaps.len() + contested_count;
    let last_activity_at = current
        .iter()
        .map(|item| item.updated_at)
        .chain(facts.intents.iter().map(|intent| intent.updated_at))
        .chain(facts.sessions.iter().map(|session| session.updated_at))
        .chain(facts.last_event_at)
        .max();
    RepositorySummary {
        repository,
        repository_id,
        provider,
        unfinished,
        needs_you,
        agents: agent_count(&facts.sessions, &facts.intents),
        last_activity_at,
        complete: facts.overview.current_complete
            && facts.overview.overlaps_complete
            && facts.live_complete,
        contested_markers,
        attention_categories: AttentionCategories {
            work: decisions,
            overlaps: facts.overview.overlaps.len(),
            contested_files: contested_count,
        },
    }
}

/// Unfinished work and how many items need a person, by `needsYou`'s order: a blocker first, then
/// a quiet lead, then a handover, each item counted once.
fn work_counts<'a>(
    items: impl IntoIterator<Item = (&'a WorkState, &'a ClaimStatus)>,
) -> (Unfinished, usize) {
    let mut counts = Unfinished::default();
    let mut decisions = 0;
    for (state, claim) in items {
        counts.total += 1;
        let quiet = *claim != ClaimStatus::Active;
        if *state == WorkState::Blocked || quiet {
            counts.stuck += 1;
            decisions += 1;
        } else if *state == WorkState::Handover {
            decisions += 1;
        } else if *state == WorkState::Active {
            counts.moving += 1;
        }
    }
    (counts, decisions)
}

/// Files held by two different agents now, each counted once by its more specific key.
fn contested_files_and_markers(intents: &[Intent]) -> (usize, Vec<String>) {
    let mut found = BTreeSet::new();
    for (i, a) in intents.iter().enumerate() {
        for b in &intents[i + 1..] {
            if same_agent(&a.token, &a.via, b) {
                continue;
            }
            for x in a.paths.iter().map(|path| key_of(path)) {
                for y in b.paths.iter().map(|path| key_of(path)) {
                    if overlaps(x, y) {
                        let key = if y.len() > x.len() { y } else { x };
                        found.insert(key);
                    }
                }
            }
        }
    }
    let count = found.len();
    (count, found.into_iter().map(str::to_owned).collect())
}

/// One per live session, plus one per live intent no session holds, as the status page lists them.
fn agent_count(sessions: &[SessionView], intents: &[Intent]) -> usize {
    let held: BTreeSet<&str> = sessions
        .iter()
        .filter_map(|s| s.intent.as_deref())
        .collect();
    sessions.len()
        + intents
            .iter()
            .filter(|intent| !held.contains(intent.id.as_str()))
            .count()
}

#[cfg(test)]
mod tests {
    use super::super::store::{ForgeIdentity, User};
    use super::*;

    fn intent(id: &str, token: &str, via: &[&str], paths: &[&str]) -> Intent {
        Intent {
            id: id.into(),
            repository: "example-org/api".into(),
            task: String::new(),
            branch: None,
            base: None,
            machine: None,
            paths: paths.iter().map(|p| (*p).into()).collect(),
            expires_at: 2_000,
            created_at: 1_000,
            updated_at: 1_000,
            token: token.into(),
            agent: "agent".into(),
            owner: User {
                id: "usr_1".into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: "example-user".into(),
                name: None,
                disabled: false,
            },
            via: via.iter().map(|v| (*v).into()).collect(),
            session: None,
        }
    }

    fn session(intent: Option<&str>) -> SessionView {
        SessionView {
            key: "key".into(),
            token: "tok".into(),
            agent: "agent".into(),
            owner: "example-user".into(),
            intent: intent.map(Into::into),
            task: None,
            via: "[]".into(),
            created_at: 1_000,
            updated_at: 1_500,
        }
    }

    #[test]
    fn counts_each_item_once_worst_first() {
        use ClaimStatus::{Active, Stale, Unknown};
        use WorkState::{Blocked, Handover, Paused};
        let items = [
            (WorkState::Active, Active),
            (WorkState::Active, Stale),
            (Blocked, Unknown),
            (Blocked, Active),
            (Handover, Active),
            (Paused, Active),
        ];
        let (counts, decisions) = work_counts(items.iter().map(|(s, c)| (s, c)));
        assert_eq!(
            counts,
            Unfinished {
                total: 6,
                moving: 1,
                stuck: 3
            }
        );
        assert_eq!(decisions, 4);
    }

    #[test]
    fn contested_needs_two_agents_on_one_file() {
        let a = intent(
            "int_a",
            "tok_a",
            &["hook:claude"],
            &["src/lib.rs", "docs/**"],
        );
        let b = intent(
            "int_b",
            "tok_b",
            &["hook:codex"],
            &["src/lib.rs", "docs/a.md"],
        );
        // The same token over MCP and a hook is one agent, as the hub's own refusals judge it.
        let same = intent("int_c", "tok_a", &["mcp"], &["src/lib.rs"]);
        let unrelated = intent("int_d", "tok_d", &["hook:codex"], &["other.rs"]);
        let result = contested_files_and_markers(&[a.clone(), b, unrelated]);
        assert_eq!(result.0, 2);
        assert!(
            result
                .1
                .iter()
                .all(|marker| marker.starts_with("src/") || marker.starts_with("docs/"))
        );
        assert_eq!(result.1.len(), 2);
        assert_eq!(contested_files_and_markers(&[a, same]), (0, Vec::new()));
    }

    #[test]
    fn contested_marker_changes_when_the_same_agents_move_their_overlap() {
        let pair = |path| {
            contested_files_and_markers(&[
                intent("int_a", "tok_a", &["hook:claude"], &[path]),
                intent("int_b", "tok_b", &["hook:codex"], &[path]),
            ])
        };
        let before = pair("src/old.rs");
        let after = pair("src/new.rs");
        assert_eq!((before.0, after.0), (1, 1));
        assert_ne!(before.1, after.1);
    }

    #[test]
    fn agents_are_sessions_plus_unheld_intents() {
        let intents = [
            intent("int_a", "tok_a", &["mcp"], &["a"]),
            intent("int_b", "tok_b", &[], &["b"]),
        ];
        assert_eq!(
            agent_count(&[session(Some("int_a")), session(None)], &intents),
            3
        );
        assert_eq!(agent_count(&[], &[]), 0);
    }
}
