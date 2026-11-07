//! Start controls a person sets and the Hub enforces when an agent asks to start Work: a hold on
//! one Work item (pause and resume) and a repository rule naming whose agents, and which agent
//! kinds, may start Work there. Both stores run the SQL; the decisions and records are here.
//!
//! Neither control stops a running session: a hold refuses new starts on the item, and a rule
//! refuses new starts in the repository. Edits, checkpoints and transitions are unchanged.
use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::http::{ApiError, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::store::{Event, User, unix_now};
use super::viewer::shown;
use super::work::{WorkItem, WorkWho, validate_id};
use super::{HubError, HubState, Result};

/// Starts every refusal message; the HTTP and MCP start replies pass such a message through.
pub const START_REFUSED: &str = "start refused: ";
const MAX_REASON_BYTES: usize = 500;
const MAX_RULE_ENTRIES: usize = 50;
const MAX_KIND_BYTES: usize = 64;

/// A person's hold on one Work item, kept in the Work snapshot so every Work read shows it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkHold {
    /// The user id of the person who paused the Work.
    pub by: String,
    pub login: String,
    pub reason: String,
    pub at: i64,
}

/// `POST /v1/hub/session/work/hold`: pause (`held: true`) or resume (`held: false`) one Work item.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkHoldRequest {
    pub repository: String,
    pub work_id: String,
    /// Compare-and-set fence from the last Work read.
    pub expected_updated_seq: i64,
    pub held: bool,
    /// Required when pausing; shown to every agent the hold refuses.
    #[serde(default)]
    pub reason: Option<String>,
}

impl WorkHoldRequest {
    fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        match (&self.reason, self.held) {
            (Some(reason), _) => super::intent::text("reason", reason, MAX_REASON_BYTES),
            (None, true) => Err(HubError::Invalid("a pause needs a reason".into())),
            (None, false) => Ok(()),
        }
    }
}

/// Applies a person's hold or resume to `item`. `None` when the item is already in that state, so
/// a repeated request records nothing. Only the Work's owner or a Hub administrator may change it.
pub(super) fn apply_hold(
    item: &mut WorkItem,
    who: &WorkWho,
    is_admin: bool,
    request: &WorkHoldRequest,
    now: i64,
) -> Result<Option<Event>> {
    request.validate()?;
    if item.owner != who.owner.id && !is_admin {
        return Err(HubError::Unauthorized);
    }
    // Already in the requested state: a retry after success records nothing, whatever its fence.
    if request.held == item.hold.is_some() {
        return Ok(None);
    }
    if item.updated_seq != request.expected_updated_seq {
        return Err(HubError::Invalid(
            "Work changed since it was read; read it again and retry".into(),
        ));
    }
    item.hold = request.held.then(|| WorkHold {
        by: who.owner.id.clone(),
        login: who.owner.login.clone(),
        reason: request.reason.clone().unwrap_or_default(),
        at: now,
    });
    item.updated_at = now;
    let mut event = Event::new(
        "work_hold",
        &item.id,
        json!({
            "operation": if request.held { "hold" } else { "resume" },
            "generation": item.generation, "owner": item.owner, "repository": item.repository,
            "repository_id": who.repository_id, "author": who.owner.id, "login": who.owner.login,
            "reason": request.reason, "admin": is_admin && item.owner != who.owner.id,
            "via": "hub",
        }),
    )?;
    event.at = now;
    Ok(Some(event))
}

/// Refuses a start that resolved to held Work. The session already leading it keeps its claim.
pub(super) fn check_hold(item: &WorkItem) -> Result<()> {
    let Some(hold) = &item.hold else {
        return Ok(());
    };
    Err(HubError::Invalid(format!(
        "{START_REFUSED}Work {} is paused by {} (since Unix {}): {}. No new start is admitted on it; \
         a session already working on it keeps its claim. Its owner {} or a Hub administrator can \
         resume it on the Hub.",
        item.id,
        hold.login,
        hold.at,
        hold.reason.escape_debug(),
        item.owner_login,
    )))
}

/// Which agents may start Work in one repository. An empty list does not restrict; both lists set
/// means an agent must pass both.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartRule {
    /// Forge logins whose agents may start Work.
    #[serde(default)]
    pub owners: Vec<String>,
    /// Agent kinds, the `<agent>` of the session's connected `hook:<agent>` (`claude`, `codex`).
    #[serde(default)]
    pub agent_kinds: Vec<String>,
}

impl StartRule {
    /// Lowercased, sorted and deduplicated, so equal rules are stored and compared equal.
    fn normalized(&self) -> Result<Self> {
        let clean = |list: &[String], check: &dyn Fn(&str) -> Result<()>| -> Result<Vec<String>> {
            if list.len() > MAX_RULE_ENTRIES {
                return Err(HubError::Invalid(
                    "a start rule list holds at most 50".into(),
                ));
            }
            let mut out = Vec::with_capacity(list.len());
            for entry in list {
                check(entry)?;
                out.push(entry.to_ascii_lowercase());
            }
            out.sort();
            out.dedup();
            Ok(out)
        };
        Ok(Self {
            owners: clean(&self.owners, &super::store::validate_login)?,
            agent_kinds: clean(&self.agent_kinds, &|kind| {
                if kind.is_empty()
                    || kind.len() > MAX_KIND_BYTES
                    || !kind
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(HubError::Invalid(
                        "an agent kind is 1-64 ASCII letters, digits, '-' or '_'".into(),
                    ));
                }
                Ok(())
            })?,
        })
    }

    fn is_open(&self) -> bool {
        self.owners.is_empty() && self.agent_kinds.is_empty()
    }
}

/// The stored rule and who last changed it, as the read route returns it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredStartRule {
    pub rule: StartRule,
    pub updated_at: i64,
    pub updated_by: String,
}

/// The `hub_settings` key of a repository's rule: by forge repository id when the caller has one,
/// so a rename keeps the rule. A token without an id is on a repository no person route can set.
pub(super) fn rule_key(
    provider: &str,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> String {
    match repository_id {
        Some(id) => format!("start_rule:{provider}:{host}:{id}"),
        None => format!(
            "start_rule:{provider}:{host}:name:{}",
            repository.to_ascii_lowercase()
        ),
    }
}

pub(super) fn who_rule_key(who: &WorkWho) -> String {
    rule_key(
        &who.owner.identity.provider,
        &who.owner.identity.host,
        &who.repository,
        who.repository_id,
    )
}

/// The value to store for `rule`, and its event. `None` when the stored rule is already `rule`.
pub(super) fn rule_change(
    previous: Option<&str>,
    user: &User,
    repository: &str,
    repository_id: i64,
    rule: &StartRule,
    now: i64,
) -> Result<(StoredStartRule, Option<(String, Event)>)> {
    let rule = rule.normalized()?;
    let previous = previous.map(parse_rule).transpose()?;
    if let Some(previous) = previous.filter(|p| p.rule == rule) {
        return Ok((previous, None));
    }
    let stored = StoredStartRule {
        rule,
        updated_at: now,
        updated_by: user.login.clone(),
    };
    let key = rule_key(
        &user.identity.provider,
        &user.identity.host,
        repository,
        Some(repository_id),
    );
    let mut event = Event::new(
        "repository_start_rule",
        &key,
        json!({
            "repository": repository, "repository_id": repository_id,
            "owners": stored.rule.owners, "agent_kinds": stored.rule.agent_kinds,
            "author": user.id, "login": user.login, "via": "hub",
        }),
    )?;
    event.at = now;
    let raw = serde_json::to_string(&stored)
        .map_err(|_| HubError::Unavailable("start rule encoding failed".into()))?;
    Ok((stored, Some((raw, event))))
}

/// A stored rule that does not parse fails closed: the store refuses the start as unavailable.
fn parse_rule(raw: &str) -> Result<StoredStartRule> {
    serde_json::from_str(raw)
        .map_err(|_| HubError::Unavailable("stored start rule is corrupt".into()))
}

/// The raw agent kind of a session, from its stored `via` JSON array.
fn agent_kind(via: Option<&str>) -> Option<String> {
    let via: Vec<String> = serde_json::from_str(via?).ok()?;
    via.iter()
        .find_map(|v| v.strip_prefix("hook:"))
        .map(str::to_ascii_lowercase)
}

/// Refuses a start the repository's rule does not admit. `rule` is the stored value, `via` the
/// starting session's stored connection modes.
pub(super) fn admit(rule: Option<&str>, who: &WorkWho, via: Option<&str>) -> Result<()> {
    let Some(stored) = rule.map(parse_rule).transpose()? else {
        return Ok(());
    };
    let rule = &stored.rule;
    let who_is = |list: &[String]| list.join(", ");
    if !rule.owners.is_empty()
        && !rule
            .owners
            .iter()
            .any(|o| o.eq_ignore_ascii_case(&who.owner.login))
    {
        return Err(HubError::Invalid(format!(
            "{START_REFUSED}in {}, only agents of {} may start Work, and this agent is {}'s. \
             A Hub administrator can change the repository's start rule on the Hub.",
            who.repository,
            who_is(&rule.owners),
            who.owner.login,
        )));
    }
    if !rule.agent_kinds.is_empty() {
        let kind = agent_kind(via);
        if !kind.as_ref().is_some_and(|k| rule.agent_kinds.contains(k)) {
            let this = kind.map_or_else(
                || "of unknown kind (no connected hook reports it)".to_owned(),
                |k| format!("a {k} agent"),
            );
            return Err(HubError::Invalid(format!(
                "{START_REFUSED}in {}, only {} agents may start Work, and this session is {this}. \
                 A Hub administrator can change the repository's start rule on the Hub.",
                who.repository,
                who_is(&rule.agent_kinds),
            )));
        }
    }
    Ok(())
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/standup/start-rule", get(read_rule))
        .route("/v1/hub/session/work/hold", post(person_hold))
        .route("/v1/hub/session/start-rule", post(person_rule))
}

/// The caller's repository rule; an open rule when none is stored. Read-only tokens may read it.
async fn read_rule(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let key = rule_key(
        &principal.owner.identity.provider,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    );
    let store = state.store.clone();
    let stored = blocking(&state, move || {
        store
            .get_setting(&key)?
            .as_deref()
            .map(parse_rule)
            .transpose()
    })
    .await?;
    Ok(Json(match stored {
        Some(stored) if !stored.rule.is_open() => json!({
            "repository": principal.repository, "rule": stored.rule,
            "updated_at": stored.updated_at, "updated_by": stored.updated_by,
        }),
        stored => json!({
            "repository": principal.repository, "rule": StartRule::default(),
            "updated_at": stored.as_ref().map(|s| s.updated_at),
            "updated_by": stored.map(|s| s.updated_by),
        }),
    }))
}

async fn person_hold(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(request): JsonBody<WorkHoldRequest>,
) -> std::result::Result<Json<WorkItem>, ApiError> {
    let user = human.web_user()?.clone();
    let viewer = user.id.clone();
    let is_admin = human.admin().is_ok();
    super::store::validate_repository(&request.repository)?;
    let repository =
        super::login::devices::mint_check(&state, &user, &request.repository, "work_hold").await?;
    let store = state.store.clone();
    let item = blocking(&state, move || {
        store.work_hold_set(
            &user,
            &repository.full_name,
            repository.repository_id,
            is_admin,
            &request,
            unix_now(),
        )
    })
    .await
    .map_err(|error| match error {
        HubError::Unauthorized => ApiError::new(StatusCode::FORBIDDEN, "not_your_work")
            .with_message("Only the Work's owner or a Hub administrator can pause or resume it"),
        HubError::Invalid(message) => ApiError::invalid().with_message(message),
        other => other.into(),
    })?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(shown(item, &viewer)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonRule {
    repository: String,
    rule: StartRule,
}

/// Sets a repository's start rule. Hub administrators only: the rule binds every owner's agents.
async fn person_rule(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<PersonRule>,
) -> std::result::Result<Json<StoredStartRule>, ApiError> {
    human.admin()?;
    let user = human.web_user()?.clone();
    super::store::validate_repository(&body.repository)?;
    let repository =
        super::login::devices::mint_check(&state, &user, &body.repository, "start_rule").await?;
    let store = state.store.clone();
    let stored = blocking(&state, move || {
        store.start_rule_set(
            &user,
            &repository.full_name,
            repository.repository_id,
            &body.rule,
            unix_now(),
        )
    })
    .await
    .map_err(|error| match error {
        HubError::Invalid(message) => ApiError::invalid().with_message(message),
        other => other.into(),
    })?;
    Ok(Json(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::ForgeIdentity;

    fn who(login: &str) -> WorkWho {
        WorkWho {
            token: "tok_1".into(),
            repository: "acme/widgets".into(),
            repository_id: Some(7),
            agent: "client-x".into(),
            read_only: false,
            owner: User {
                id: format!("usr_{login}"),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: login.into(),
                name: None,
                disabled: false,
            },
        }
    }

    fn stored(owners: &[&str], kinds: &[&str]) -> String {
        serde_json::to_string(&StoredStartRule {
            rule: StartRule {
                owners: owners.iter().map(|s| (*s).into()).collect(),
                agent_kinds: kinds.iter().map(|s| (*s).into()).collect(),
            }
            .normalized()
            .unwrap(),
            updated_at: 1,
            updated_by: "admin".into(),
        })
        .unwrap()
    }

    fn refused(result: Result<()>) -> String {
        match result {
            Err(HubError::Invalid(message)) => {
                assert!(message.starts_with(START_REFUSED), "{message}");
                message
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn rule_admits_listed_owners_and_kinds() {
        let claude = Some(r#"["hook:claude","mcp"]"#);
        assert!(admit(None, &who("amy"), None).is_ok());
        assert!(admit(Some(&stored(&[], &[])), &who("amy"), None).is_ok());
        assert!(admit(Some(&stored(&["Amy"], &[])), &who("amy"), None).is_ok());
        let text = refused(admit(Some(&stored(&["amy"], &[])), &who("bob"), claude));
        assert!(
            text.contains("only agents of amy") && text.contains("bob's"),
            "{text}"
        );
        assert!(admit(Some(&stored(&[], &["claude"])), &who("bob"), claude).is_ok());
        let text = refused(admit(Some(&stored(&[], &["codex"])), &who("bob"), claude));
        assert!(text.contains("a claude agent"), "{text}");
        let text = refused(admit(Some(&stored(&[], &["codex"])), &who("bob"), None));
        assert!(text.contains("unknown kind"), "{text}");
        // Both lists bind together.
        refused(admit(
            Some(&stored(&["amy"], &["codex"])),
            &who("amy"),
            claude,
        ));
        // A corrupt stored rule fails closed.
        assert!(matches!(
            admit(Some("{"), &who("amy"), None),
            Err(HubError::Unavailable(_))
        ));
    }

    #[test]
    fn rule_entries_are_validated_and_normalized() {
        let rule = StartRule {
            owners: vec!["Bob".into(), "amy".into(), "bob".into()],
            agent_kinds: vec!["Codex".into()],
        }
        .normalized()
        .unwrap();
        assert_eq!(rule.owners, ["amy", "bob"]);
        assert_eq!(rule.agent_kinds, ["codex"]);
        let bad = StartRule {
            owners: Vec::new(),
            agent_kinds: vec!["co dex".into()],
        };
        assert!(bad.normalized().is_err());
        let many = StartRule {
            owners: (0..51).map(|i| format!("u{i}")).collect(),
            agent_kinds: Vec::new(),
        };
        assert!(many.normalized().is_err());
    }

    #[test]
    fn an_unchanged_rule_records_nothing() {
        let user = who("admin").owner;
        let rule = StartRule {
            owners: vec!["amy".into()],
            agent_kinds: Vec::new(),
        };
        let (_, change) = rule_change(None, &user, "acme/widgets", 7, &rule, 5).unwrap();
        let (raw, event) = change.unwrap();
        assert_eq!(event.kind, "repository_start_rule");
        assert_eq!(event.subject, "start_rule:github:github.com:7");
        let (kept, again) = rule_change(Some(&raw), &user, "acme/widgets", 7, &rule, 9).unwrap();
        assert!(again.is_none());
        assert_eq!(kept.updated_at, 5);
    }

    #[test]
    fn rule_keys_prefer_the_repository_id() {
        assert_eq!(
            rule_key("github", "github.com", "Acme/W", Some(7)),
            "start_rule:github:github.com:7"
        );
        assert_eq!(
            rule_key("github", "github.com", "Acme/W", None),
            "start_rule:github:github.com:name:acme/w"
        );
    }
}
