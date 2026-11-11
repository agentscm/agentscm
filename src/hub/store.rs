//! Hub persistence: one trait, SQLite and PostgreSQL implementations.
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, CaptureBatchV1, ChangeRecordSummaryV1,
    ConversationEntryV1, IngestReceiptV1,
};
use crate::hub::catchup::{
    CatchupAck, CatchupAckResult, CatchupMergeObservation, CatchupPage, CatchupPreview,
    CatchupPreviewQuery, CatchupPublicationRepair, CatchupPublicationRepairResult, CatchupQuery,
    CatchupRepositoryScope, CatchupSourceStatus, NewCatchupMerge,
};

use super::capacity::CapacityRefusal;
use super::change_record::{SharedBodyAccess, SharedBodyRow, SharedBodyState};
use super::connection::{
    ConnectedRepository, Connection, ForgeCheck, ForgeCheckReserve, MergeGroup, NewConnection,
    NewForgeCheck, NewMergeGroup, Observation, Observed, Outbound,
};
use super::coordination::{
    CoordinationAck, CoordinationInbox, CoordinationInboxQuery, CoordinationListQuery,
    CoordinationReply, CoordinationRequest, CoordinationResolve, CoordinationWithdraw,
};
use super::device::{Device, DevicePrincipal, Refusal, RepositoryUsage};
use super::external_runs::{ExternalKind, ExternalObserved, ExternalRun};
use super::handoff::{Answered, Asked, HandoffView, Via};
use super::insights::{InsightPage, InsightQuery, UsageReceipt, UsageRecord, WorkInsight};
use super::intent::{Announced, Intent, NewIntent};
use super::pickup::Pickup;
use super::promise::{
    PromiseAdoptV1, PromiseChangeAuthority, PromiseChangeV1, PromiseCreateV1, PromiseHistoryPageV1,
    PromiseInputPageV1, PromiseInputReceiptV1, PromiseInputV1, PromiseInputViewV1, PromisePageV1,
    PromiseReceiptV1, PromiseViewV1,
};
use super::publish::{Create, NewPublication, Publication, PublicationSearch, Update};
use super::session::{SessionCall, SessionEnd, Touched};
use super::skills::{EvidenceRow, RunHead, SkillAdoption};
use super::store_pg::PgStore;
use super::store_sqlite::SqliteStore;
use super::work::{
    Briefing, BriefingAck, BriefingQuery, WorkCheckpoint, WorkCheckpointResult, WorkCompaction,
    WorkItem, WorkOverview, WorkOverviewQuery, WorkRepositoryScope, WorkScopeCheck, WorkSearchPage,
    WorkStart, WorkStartResult,
};
use super::work_links::{WorkPrLinkQuery, WorkPrLinkResult, WorkPrLinkUpdate, WorkPrLinkView};
use super::work_preflight::WorkPreflightResult;
use super::work_transition::{
    OperationalOverviewV1, OperationalSelect, WorkLifecycleReader, WorkTransitionActor,
    WorkTransitionRequest, WorkTransitionResult,
};
use super::{HubError, Result};
use crate::local_ci::DeviceAuthorization;
use crate::team::auth::{Secret, generate_secret, lower_hex, token_digest};

const AGENT_TOKEN_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.agent-token.v1\0";
const WEB_READ_TOKEN_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.web-read-token.v1\0";
const DEVICE_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.device.v1\0";
const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 255;
const MAX_DISPLAY_NAME_CHARS: usize = 100;
const MAX_REPOSITORY_BYTES: usize = 255;
const MAX_AGENT_NAME_BYTES: usize = 64;
const MAX_EVENTS_PER_READ: usize = 1000;
pub const MAX_AGENT_TOKEN_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// A person on a forge. The numeric id is the identity; the login is a cached
/// display value that may change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeIdentity {
    /// `github` or `gitlab`.
    pub provider: String,
    pub host: String,
    pub user_id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// Stable portable id: `usr_` + 32 lowercase hex.
    pub id: String,
    pub identity: ForgeIdentity,
    pub login: String,
    /// The forge profile name, cached and refreshed at each sign-in. Read by the user lookups,
    /// `list_users` and `browser_session`; `None` in the owner joins of other records.
    pub name: Option<String>,
    pub disabled: bool,
}

/// Digest of an agent token secret. The only constructor is
/// [`agent_token_digest`], so a raw secret cannot be stored as a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDigest(String);

impl TokenDigest {
    /// Restore a digest from a trusted controller record, never a request credential.
    pub(super) fn from_persisted(value: &str) -> Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(HubError::Unauthorized);
        }
        Ok(Self(value.to_owned()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAgentToken {
    pub owner: String,
    pub name: String,
    pub repository: String,
    pub digest: TokenDigest,
    /// Unix seconds; the token is valid while now < expires_at.
    pub expires_at: i64,
    /// The forge's stable repository id, when the forge confirmed access at minting.
    pub repository_id: Option<i64>,
    /// May only call the read routes.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// `tok_` + 32 lowercase hex.
    pub token_id: String,
    pub name: String,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub expires_at: i64,
    pub read_only: bool,
    pub owner: User,
}

/// A token as its owner sees it; never holds the secret or digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentToken {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub expires_at: i64,
    pub revoked: bool,
    pub read_only: bool,
    pub created_at: i64,
}

/// One row of the Hub event log; `data` is canonical JSON.
/// General events are append-only. Stand-up work events permit operator compaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubEvent {
    pub seq: i64,
    /// `evt_` + 32 lowercase hex.
    pub id: String,
    pub at: i64,
    pub kind: String,
    pub subject: String,
    pub data: String,
}

/// A live session as the status page shows it; never its notice cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    /// Stable per session and not its id; see [`session_key`].
    pub key: String,
    /// The Hub's name for the session, to look up what the Hub holds for it. Sent only to the
    /// session's own person, as their Work replies name it.
    pub session: String,
    /// The session's owner-wide name; see [`owner_session`].
    pub name: Option<String>,
    /// The owner-wide name of the session that started this one, when one declared it.
    pub started_by: Option<String>,
    pub token: String,
    pub agent: String,
    /// The login of the person whose agent token the session is on.
    pub owner: String,
    /// That person's id. Two people can carry one login for a while after a rename, so whose a
    /// session is is decided by this. Never sent.
    pub owner_id: String,
    pub intent: Option<String>,
    pub task: Option<String>,
    /// Canonical JSON array of connection modes.
    pub via: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An event with the agent token and owner login it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEvent {
    pub event: HubEvent,
    pub token: String,
    pub agent: String,
    pub owner: String,
}

/// The event kinds the status page shows, as an SQL list.
pub(super) const ACTIVITY_KINDS: &str = "'session_started', 'session_connected', 'session_ended',
    'edit_denied', 'edit_repeated', 'hold_expired', 'intent_announced', 'intent_released',
    'intent_notice', 'pr_notice', 'handoff_request', 'handoff_grant', 'handoff_decline', 'task_overlap'";

/// The single namespace a promise input page belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromiseInputScope<'a> {
    Proposal(&'a str),
    Report(&'a str),
}

/// Methods block; async callers use `spawn_blocking`. Every mutation appends
/// its event in the same transaction as the state change; a no-op appends nothing.
pub trait Store: Send + Sync {
    fn backend(&self) -> &'static str;
    fn ping(&self) -> Result<()>;
    /// Trusted administrator/controller API. Never expose directly to an agent token.
    /// The selected connection must own the connected repository.
    fn configure_validation_summary(
        &self,
        config: &super::validation_evidence::ValidationSummaryConfig,
    ) -> Result<()>;
    fn validation_summary_config(
        &self,
        repository: &str,
    ) -> Result<Option<super::validation_evidence::ValidationSummaryConfig>>;
    /// Rechecks the token and repository binding in the receipt transaction.
    fn receive_validation_evidence(
        &self,
        token: &str,
        proposal: i64,
        bundle: &super::validation_evidence::DurableValidationBundle,
        now: i64,
    ) -> Result<super::validation_evidence::ValidationEvidenceReceipt>;
    fn validation_evidence_history(
        &self,
        token: &str,
        repository: &str,
        proposal: i64,
        after: i64,
        limit: usize,
        now: i64,
    ) -> Result<super::validation_evidence::ValidationEvidencePage>;
    /// Trusted internal worker API, with a complete bounded receipt inventory.
    fn validation_snapshot(
        &self,
        repository: &str,
        proposal: i64,
        limit: usize,
    ) -> Result<super::validation_evidence::ValidationSnapshot>;
    /// Config/generation only, for invalidating statuses when the complete inventory fails.
    fn validation_fence(
        &self,
        repository: &str,
        proposal: i64,
    ) -> Result<super::validation_evidence::ValidationFence>;
    fn validation_publication_proposals(
        &self,
        repository: &str,
        after: i64,
        through: Option<i64>,
        limit: usize,
    ) -> Result<super::validation_evidence::ValidationProposalPage>;
    fn validation_publication(
        &self,
        repository: &str,
        proposal: i64,
    ) -> Result<Option<super::validation_evidence::ValidationPublication>>;
    /// Atomic cross-process exclusion. `false` means revision, evidence or config changed.
    /// Recovery of the same Attempting operation is allowed after new evidence or disable.
    fn compare_validation_publication(
        &self,
        repository: &str,
        proposal: i64,
        expected_revision: i64,
        next: &super::validation_evidence::ValidationPublication,
    ) -> Result<bool>;
    /// Insert, or update the cached login of an existing identity. Never changes the id.
    fn upsert_user(&self, identity: &ForgeIdentity, login: &str) -> Result<User>;
    /// Replace the cached profile name (`None` clears it). A display value, not an audited
    /// fact, so no event records it. Unknown user is `Invalid`.
    fn set_user_name(&self, user: &str, name: Option<&str>) -> Result<()>;
    fn set_user_disabled(&self, user: &str, disabled: bool) -> Result<()>;
    /// The owner must exist and be enabled, and `expires_at` must be in the future.
    fn insert_agent_token(&self, token: &NewAgentToken) -> Result<String>;
    /// True if the token was active.
    fn revoke_agent_token(&self, token: &str) -> Result<bool>;
    /// Newest first, at most 1000.
    fn list_agent_tokens(&self, owner: &str) -> Result<Vec<AgentToken>>;
    /// Like `revoke_agent_token`, but only for a token `owner` owns; the event records `by`.
    fn revoke_owned_agent_token(&self, owner: &str, token: &str) -> Result<bool>;
    /// Like `set_user_disabled`; the event records the acting user.
    fn set_user_disabled_by(&self, actor: &str, user: &str, disabled: bool) -> Result<()>;
    /// Oldest first, at most 1000.
    fn list_users(&self) -> Result<Vec<User>>;
    /// Exact identity lookup; authorization must not depend on a bounded user list.
    fn user_by_id(&self, id: &str) -> Result<Option<User>>;
    /// When the person first signed in to this Hub (unix seconds); None for an unknown id.
    fn user_created_at(&self, id: &str) -> Result<Option<i64>>;
    /// Exact numeric forge identity lookup, including disabled users.
    fn user_for_identity(&self, identity: &ForgeIdentity) -> Result<Option<User>>;
    /// `method` is `browser` or `device`. Unknown user is `Invalid`.
    fn record_sign_in(&self, user: &str, method: &str, admin: bool) -> Result<()>;
    /// None if unknown, revoked, expired, or the owner is disabled.
    fn authenticate(&self, digest: &TokenDigest, now: i64) -> Result<Option<Principal>>;
    /// A new computer for an enabled `owner`. `replaces` (a computer `owner` owns) is revoked
    /// with its agent tokens in the same transaction. `app` marks a connected app's credential.
    /// Returns the `dev_` id.
    #[allow(clippy::too_many_arguments)]
    fn insert_device(
        &self,
        owner: &str,
        name: &str,
        digest: &TokenDigest,
        replaces: Option<&str>,
        app: Option<&str>,
        now: i64,
    ) -> Result<String>;
    /// None if unknown, revoked or idle past `DEVICE_IDLE_SECONDS`. A disabled owner is returned
    /// with `owner.disabled` set, so the caller can say so. Records use at most once per
    /// `DEVICE_TOUCH_SECONDS`.
    fn authenticate_device(
        &self,
        digest: &TokenDigest,
        now: i64,
    ) -> Result<Option<DevicePrincipal>>;
    /// `owner`'s computers, or everyone's for None: live ones first, then those revoked within
    /// the last 30 days; newest first, at most `MAX_DEVICES`.
    fn devices(&self, owner: Option<&str>, now: i64) -> Result<Vec<Device>>;
    /// Revokes a live computer (only `owner`'s when Some) and every agent token it minted.
    /// True if it was live.
    fn revoke_device(&self, device: &str, owner: Option<&str>, by: &str, now: i64) -> Result<bool>;
    /// Revokes every live connected-app credential whose app is not in `registered`, with the
    /// agent tokens each minted. Returns how many were revoked.
    fn revoke_unregistered_apps(&self, registered: &[String], by: &str, now: i64) -> Result<usize>;
    /// True if `owner`'s live computer was renamed.
    fn rename_device(&self, device: &str, owner: &str, name: &str) -> Result<bool>;
    /// Authorizes a local CI device (generation 1) for the repository row `repository`
    /// (`hub_repositories.id`) and the enabled Hub user `owner`, and appends an event, in one
    /// transaction. `by` is the administrator who acted; the event names them, not `owner`.
    /// A key id is enrolled once ever. A replayed `request` returns the device.
    fn enroll_local_ci_device(
        &self,
        repository: &str,
        owner: &str,
        by: &str,
        authorization: &DeviceAuthorization,
        request: &LocalCiRequest,
        now: i64,
    ) -> Result<LocalCiDeviceWrite>;
    /// Replaces the authorization with `next` (from `rotate` or `revoke`) if the stored
    /// generation is `expected_generation`, and appends an event naming `by`, in one transaction.
    fn change_local_ci_device(
        &self,
        device_id: &str,
        expected_generation: u64,
        by: &str,
        next: &DeviceAuthorization,
        request: &LocalCiRequest,
        now: i64,
    ) -> Result<LocalCiDeviceWrite>;
    /// A corrupt stored record is an error, never a default.
    fn local_ci_device(&self, device_id: &str) -> Result<Option<LocalCiDevice>>;
    /// The repository row's devices ordered by device id, at most 1000, only those of the Hub
    /// user `owner` when given (filtered before the limit).
    fn local_ci_devices(&self, repository: &str, owner: Option<&str>)
    -> Result<Vec<LocalCiDevice>>;
    /// The device and fingerprint recorded for a request id, if it was applied.
    fn local_ci_request(&self, request_id: &str) -> Result<Option<(String, String)>>;
    /// The repository row by its stable `rep_` id, under its current host and name.
    fn repository_row(&self, id: &str) -> Result<Option<ConnectedRepository>>;
    /// The device's enrolled, rotated and revoked events, oldest first, at most 1000.
    fn local_ci_device_events(&self, device_id: &str) -> Result<Vec<HubEvent>>;
    /// Like `insert_agent_token`, minted through `device` (which must be live and `token.owner`'s).
    /// `auto` (auto-connect) records an `auto_connected` event.
    fn insert_device_token(
        &self,
        token: &NewAgentToken,
        device: &str,
        auto: bool,
    ) -> Result<String>;
    /// Records a refusal: one row per person and reason; a repeat bumps `count` and `at`.
    /// Prunes rows past `REFUSAL_SECONDS` and beyond the newest `MAX_REFUSALS`.
    fn record_refusal(&self, refusal: &Refusal) -> Result<()>;
    /// Newest first, within `REFUSAL_SECONDS` of `now`.
    fn refusals(&self, now: i64) -> Result<Vec<Refusal>>;
    /// Records a capacity refusal: one row per repository and limit; a repeat adds its count
    /// and keeps the newest `at`. Prunes rows past `capacity::RETAIN_SECONDS` and beyond the
    /// newest `capacity::MAX_REFUSALS`.
    fn record_capacity_refusal(&self, refusal: &CapacityRefusal) -> Result<()>;
    /// Newest first, within `capacity::RETAIN_SECONDS` of `now`.
    fn capacity_refusals(&self, now: i64) -> Result<Vec<CapacityRefusal>>;
    /// Connected repositories with the computers and live agent tokens using each.
    fn repository_usage(&self, now: i64) -> Result<Vec<RepositoryUsage>>;
    /// A web-page sign-in for an enabled `owner`, until `expires_at`. Expired rows go, and a
    /// person keeps at most `MAX_BROWSER_SESSIONS` (the oldest go first).
    fn insert_browser_session(
        &self,
        owner: &str,
        digest: &TokenDigest,
        expires_at: i64,
        now: i64,
    ) -> Result<()>;
    /// The owner of a live web-page sign-in (with `disabled` set when they were disabled) and its
    /// expiry, moved to `slide_to` when that is `SLIDE_STEP_SECONDS` later. None when unknown,
    /// expired, or begun `BROWSER_SESSION_MAX_SECONDS` ago.
    fn browser_session(
        &self,
        digest: &TokenDigest,
        now: i64,
        slide_to: i64,
    ) -> Result<Option<(User, i64)>>;
    /// True if the web-page sign-in existed.
    fn delete_browser_session(&self, digest: &TokenDigest) -> Result<bool>;
    /// Upsert intent `intent.id` for the active token `token`; returns it and every live overlapping intent.
    fn announce_intent(&self, token: &str, intent: &NewIntent, now: i64) -> Result<Announced>;
    /// None if unknown, not the caller's, released or expired.
    fn renew_intent(
        &self,
        token: &str,
        intent: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<Option<Intent>>;
    /// True if it was the caller's and not already released.
    fn release_intent(&self, token: &str, intent: &str, now: i64) -> Result<bool>;
    /// Upsert the session, add `call.paths` to its intent (creating a fresh intent when it has none,
    /// or it was released), renew the intent to now + `SESSION_TTL_SECONDS`, and collect new
    /// overlaps and notices.
    fn touch_session(&self, token: &str, call: &SessionCall, now: i64) -> Result<Touched> {
        self.touch_session_named(token, call, call.session.is_some(), now)
    }
    /// `touch_session`, where `named` is false when the caller named no session and
    /// `call.session` is the one the Hub resolved for it. Such a touch renews no lease and carries
    /// no lead credential, since that session is not the call's own.
    fn touch_session_named(
        &self,
        token: &str,
        call: &SessionCall,
        named: bool,
        now: i64,
    ) -> Result<Touched>;
    /// Release the session's intent (if any) and delete the session row. `None` resolves like
    /// `SessionCall.session`. True if a session existed.
    fn end_session(&self, token: &str, session: Option<&str>, now: i64) -> Result<bool>;
    /// `end_session`, and in the same transaction pause the active Work the session leads, with
    /// the pause checkpoint a lead would write (`work_checkpoint`). Blocked Work stays blocked;
    /// Work with no session row is still paused. If the pause fails nothing is written, so the
    /// intent stays held and the call can be repeated. `via` (`mcp` or `hook:<agent>`) names the
    /// pause's operation, which is fixed by the Work's generation and sequence. A hook's end may
    /// be only the gap between two turns of one session, so it leaves Work whose hold is live
    /// active, renews its lease and writes no Work event; `mcp` always pauses.
    fn end_session_pausing(
        &self,
        token: &str,
        session: Option<&str>,
        via: &str,
        now: i64,
    ) -> Result<SessionEnd>;
    /// Asks the live session intent holding exactly the file `path` to hand it over to the
    /// requesting session (`session` resolves like `SessionCall.session`). `note` is filtered and
    /// cut to 200 chars. One request per file and session, at most 3 open per session.
    fn ask_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        note: Option<&str>,
        now: i64,
    ) -> Result<Asked> {
        self.ask_handoff_via(token, session, path, note, Via::Agent, now)
    }
    /// [`Store::ask_handoff`] with the record saying who asked: `Via::Hub` is the token owner
    /// acting for the agent on the Hub's web pages. The caller has checked that they own `token`.
    fn ask_handoff_via(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        note: Option<&str>,
        via: Via,
        now: i64,
    ) -> Result<Asked>;
    /// The holder's answer to the oldest open request for `path` addressed to the token (to its
    /// session `session`, if given). Accepting moves the file to the requester's session; a
    /// declining `reason` is filtered like a note and ignored on accept.
    fn answer_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        accept: bool,
        reason: Option<&str>,
        now: i64,
    ) -> Result<Answered> {
        self.answer_handoff_via(token, session, path, (accept, reason), Via::Agent, now)
    }
    /// [`Store::answer_handoff`] with the record saying who answered, as for `ask_handoff_via`.
    fn answer_handoff_via(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        answer: (bool, Option<&str>),
        via: Via,
        now: i64,
    ) -> Result<Answered>;
    /// The newest handoffs in `repository` (most recent first, at most `limit`) in which a
    /// token owned by `owner` is the requester or the holder, in any state.
    fn handoffs_of_owner(
        &self,
        owner: &str,
        repository: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<HandoffView>>;
    /// Records a person's hand-off in `repository`, then prunes that person's records older than
    /// `pickup::PICKUP_RETAIN_SECONDS` and beyond the newest `pickup::MAX_PICKUPS_PER_OWNER`.
    /// Repository names in pickup reads and matches compare without regard to case.
    fn pickup_create(
        &self,
        owner: &str,
        repository: &str,
        work_id: Option<&str>,
        title: Option<&str>,
        now: i64,
    ) -> Result<Pickup>;
    /// Abandons `owner`'s waiting hand-off `id`; a picked-up, expired or abandoned one is
    /// returned unchanged. `None` when `owner` has no such record in `repository`.
    fn pickup_abandon(
        &self,
        owner: &str,
        repository: &str,
        id: &str,
        now: i64,
    ) -> Result<Option<Pickup>>;
    /// `owner`'s newest hand-offs in `repository`, most recent first, at most `limit`.
    fn pickups_of_owner(
        &self,
        owner: &str,
        repository: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<Pickup>>;
    /// Live intents in `repository`, ordered by id, at most `limit`.
    fn live_intents(&self, repository: &str, now: i64, limit: usize) -> Result<Vec<Intent>>;
    /// Live intents in the principal's repository and forge scope, ordered by id.
    fn live_intents_scoped(&self, who: &Principal, now: i64, limit: usize) -> Result<Vec<Intent>>;
    /// `intent_notice` and `pr_notice` events addressed to the token's intents with seq > `after`, oldest first, at most `limit`.
    fn notices_after(&self, token: &str, after: i64, limit: usize) -> Result<Vec<HubEvent>>;
    /// Sessions of the active tokens in `who`'s scope (same repository name and forge id, same
    /// owner host and provider) updated within `SESSION_TTL_SECONDS` of `now`, newest first, at most `limit`.
    fn live_sessions(&self, who: &Principal, now: i64, limit: usize) -> Result<Vec<SessionView>> {
        self.sessions_with(who, now, limit, &[])
    }
    /// [`Store::live_sessions`], and also those of `also` that are sessions of `who`'s own agent
    /// tokens in the same scope, however long ago they last called.
    fn sessions_with(
        &self,
        who: &Principal,
        now: i64,
        limit: usize,
        also: &[String],
    ) -> Result<Vec<SessionView>>;
    /// Records that the session `started_by` started `session`, both of `owner`, in whichever
    /// repositories they work. Refused unless `started_by` has an owner-wide name
    /// ([`owner_session`]) and is a session of one of `owner`'s agent tokens that the Hub has
    /// seen or that leads Work, and when the link would close a loop. A later call replaces it.
    /// Records older than `SESSION_START_RETAIN_SECONDS`, and the oldest beyond
    /// `SESSION_STARTS_MAX`, are dropped.
    fn session_started_by(
        &self,
        owner: &str,
        session: &str,
        started_by: &str,
        now: i64,
    ) -> Result<()>;
    /// Each of `owner`'s sessions that another session started, with that session, the newest
    /// record first.
    fn session_starts(&self, owner: &str) -> Result<Vec<(String, String)>>;
    /// The sessions that lead `owner`'s unfinished Work (active, blocked or paused), in any
    /// repository.
    fn owner_lead_sessions(&self, owner: &str) -> Result<Vec<String>>;
    /// The status page's event kinds for the tokens and intents in `who`'s scope (as for
    /// `live_sessions`, revoked tokens included), newest first, with `seq` below `before` when
    /// given, at most `limit`.
    fn repository_events(
        &self,
        who: &Principal,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<ActivityEvent>>;
    /// Fails with `Invalid` on a bad shape or a taken id; the event records `actor`.
    fn insert_connection(&self, new: &NewConnection, actor: &str) -> Result<Connection>;
    /// Oldest first, at most 1000.
    fn connections(&self) -> Result<Vec<Connection>>;
    fn connection(&self, id: &str) -> Result<Option<Connection>>;
    /// The `pat` connection for `host`, if any.
    fn pat_connection(&self, host: &str) -> Result<Option<Connection>>;
    /// `enabled` or `auth_failed`; an event only when status or detail changes.
    fn set_connection_status(&self, id: &str, status: &str, detail: Option<&str>) -> Result<()>;
    /// An event only when the flag changes; unknown connection is `Invalid`.
    fn set_connection_outbound(&self, id: &str, enabled: bool, actor: &str) -> Result<()>;
    /// A hub-wide setting's raw text; the owning module validates it.
    fn get_setting(&self, key: &str) -> Result<Option<String>>;
    /// Upserts a hub-wide setting; no event, `updated_by` records the actor.
    fn set_setting(&self, key: &str, value: &str, actor: &str) -> Result<()>;
    /// Insert a hub-wide setting once across replicas and return its retained value.
    fn get_or_create_setting(&self, key: &str, value: &str, actor: &str) -> Result<String>;
    /// Unix seconds; no event.
    fn set_next_poll(&self, id: &str, at: i64) -> Result<()>;
    /// Keyed by (host, repository_id): a rename updates the row. A repository already held by
    /// another connection is returned unchanged.
    fn upsert_repository(
        &self,
        connection: &str,
        host: &str,
        repository_id: i64,
        full_name: &str,
        node_id: Option<&str>,
        installation_id: Option<i64>,
    ) -> Result<ConnectedRepository>;
    /// Oldest first, at most 1000.
    fn repositories(&self, connection: &str) -> Result<Vec<ConnectedRepository>>;
    /// `full_name` compares ASCII-case-insensitively.
    fn repository_named(&self, host: &str, full_name: &str) -> Result<Option<ConnectedRepository>>;
    fn repository_by_id(
        &self,
        host: &str,
        repository_id: i64,
    ) -> Result<Option<ConnectedRepository>>;
    fn repositories_on(&self, host: &str, limit: usize) -> Result<Vec<ConnectedRepository>>;
    /// No event.
    fn set_pulls_etag(&self, repo: &str, etag: Option<&str>) -> Result<()>;
    /// True when the delivery id is new for the connection. Ids older than a week are pruned.
    fn record_delivery(&self, connection: &str, delivery: &str, now: i64) -> Result<bool>;
    /// Greatest forge observation timestamp retained for this repository, including tombstones.
    /// Sync reads this once per repository and process to seed its monotonic read clock.
    fn observation_floor(&self, repo: &str) -> Result<i64>;
    /// Record what a read of the forge (started at `observed_at`, unix ms) showed.
    /// Conflicting pull-request reads at one timestamp retain an unknown tombstone until
    /// a strictly newer observation. Exact equal replay is unchanged.
    fn observe(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        data: &str,
        observed_at: i64,
    ) -> Result<Observed>;
    /// Record that the thing is gone, with `event` (`pull_request_closed`, `branch_deleted`
    /// or `branch_unobserved`). True when an observation was forgotten. A key never seen
    /// gets a marker and no event; so does an already-forgotten one, and a newer stored read wins.
    fn forget(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        observed_at: i64,
        data: &str,
        event: &str,
    ) -> Result<bool>;
    /// Read one current observation by its exact key, independent of list pagination.
    fn observation(&self, repo: &str, kind: &str, key: &str) -> Result<Option<Observation>>;
    /// Exact retained observation including a tombstone. The boolean is true when forgotten;
    /// a tombstone does not by itself establish that a pull request closed unmerged.
    fn observation_retained(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
    ) -> Result<Option<(Observation, bool)>>;
    /// Present (not forgotten) observations of `kind`, ordered by key, at most 1000.
    fn observations(&self, repo: &str, kind: &str) -> Result<Vec<Observation>>;
    /// Drops forgotten markers last read before `before` (unix ms); no event. Returns how many.
    fn prune_tombstones(&self, before: i64) -> Result<usize>;
    fn outbound(&self, repo: &str, number: i64) -> Result<Option<Outbound>>;
    fn set_outbound_comment(
        &self,
        repo: &str,
        number: i64,
        comment_id: i64,
        body_sha256: &str,
    ) -> Result<()>;
    fn set_outbound_status(
        &self,
        repo: &str,
        number: i64,
        sha: &str,
        description: &str,
    ) -> Result<()>;
    fn forge_check(&self, repo: &str, number: i64, context: &str) -> Result<Option<ForgeCheck>>;
    /// Rows of `repo` that may show a success (state `success` or an uncertain post) as
    /// `(number, context, row)`, by number then context, at most `limit`. Rows on a context in
    /// `enabled`, and rows of a pull request whose newest publication in `full_name` is merged,
    /// are left out: neither is a retire candidate.
    fn forge_checks_showing_success(
        &self,
        repo: &str,
        full_name: &str,
        enabled: &[String],
        limit: usize,
    ) -> Result<Vec<(i64, String, ForgeCheck)>>;
    /// Compare-and-set on `expected` (`None` for an absent row): records the desired post as
    /// `attempting` and returns the new generation, or `Conflict` when another writer won.
    fn reserve_forge_check(
        &self,
        repo: &str,
        number: i64,
        context: &str,
        expected: Option<i64>,
        new: &NewForgeCheck,
        now: i64,
    ) -> Result<ForgeCheckReserve>;
    /// Marks the post `phase` (`posted`, or `refused` for a definite refusal) only if
    /// `generation` is still current; returns whether it did.
    fn complete_forge_check(
        &self,
        repo: &str,
        number: i64,
        context: &str,
        generation: i64,
        phase: &str,
        now: i64,
    ) -> Result<bool>;
    /// Records merge group `group` of `repo` once; identities of an existing row never change.
    /// `ended` ends a current row, or records an unseen group as already ended (a destroy
    /// delivered first). A later observation never revives an ended group. Also deletes ended
    /// groups of `repo` that ended over 30 days before `now`. Returns the stored row.
    fn observe_merge_group(
        &self,
        repo: &str,
        group: &NewMergeGroup,
        ended: Option<&str>,
        now: i64,
    ) -> Result<MergeGroup>;
    fn merge_group(&self, repo: &str, head_sha: &str) -> Result<Option<MergeGroup>>;
    /// Current (not ended) groups of `repo`, oldest observation first, at most `limit`.
    fn active_merge_groups(&self, repo: &str, limit: usize) -> Result<Vec<MergeGroup>>;
    /// Groups of `repo` that queued pull request `pr_number`, current or ended, newest
    /// observation first, at most `limit`.
    fn merge_groups_for_pull(
        &self,
        repo: &str,
        pr_number: i64,
        limit: usize,
    ) -> Result<Vec<MergeGroup>>;
    /// Ends a current group; false when it was already ended or is unknown.
    fn end_merge_group(&self, repo: &str, head_sha: &str, reason: &str, now: i64) -> Result<bool>;
    /// Compare-and-set on `expected` generation of a current group: records the desired post as
    /// `attempting` and returns the new generation, or `Conflict` when another writer won or the
    /// group has ended. `new.head_sha` must be the group's.
    fn reserve_merge_group_check(
        &self,
        repo: &str,
        head_sha: &str,
        expected: i64,
        new: &NewForgeCheck,
        now: i64,
    ) -> Result<ForgeCheckReserve>;
    /// Marks the post `phase` (`posted` or `refused`) only if `generation` is still current.
    fn complete_merge_group_check(
        &self,
        repo: &str,
        head_sha: &str,
        generation: i64,
        phase: &str,
        now: i64,
    ) -> Result<bool>;
    /// Records what the Hub last observed about the required context and the posting account.
    fn note_merge_group_source(
        &self,
        repo: &str,
        head_sha: &str,
        ruleset_requirement: &str,
        status_source: &str,
    ) -> Result<()>;
    /// Records one external CI or deployment run of `repo` (a repository id) unless
    /// [`supersedes`](super::external_runs::supersedes) rejects it; an event only on `Changed`.
    fn observe_external_run(&self, repo: &str, run: &ExternalRun) -> Result<ExternalObserved>;
    /// Runs of `kind` seen for `commit_sha`, by environment, run id, then attempt, at most `limit`.
    fn external_runs(
        &self,
        repo: &str,
        commit_sha: &str,
        kind: ExternalKind,
        limit: usize,
    ) -> Result<Vec<ExternalRun>>;
    /// Notes that the provider was read for `commit_sha` at `at` (unix seconds); `complete` is
    /// false when the read was cut short.
    fn mark_external_reconciled(
        &self,
        repo: &str,
        commit_sha: &str,
        at: i64,
        complete: bool,
    ) -> Result<()>;
    fn external_reconciled(&self, repo: &str, commit_sha: &str) -> Result<Option<(i64, bool)>>;
    /// Publications of `repository` (a full name, matched case-insensitively) with a merge
    /// commit, last updated at or after `oldest` (unix seconds), by id, ids above `after`, at
    /// most `limit`.
    fn merged_publications(
        &self,
        repository: &str,
        after: &str,
        oldest: i64,
        limit: usize,
    ) -> Result<Vec<Publication>>;
    /// `kind` events (`pr_notice`, `task_overlap`) as `(subject, data JSON)`, in one transaction.
    fn append_notices(&self, kind: &'static str, notices: &[(String, String)]) -> Result<()>;
    /// Idempotent by `new.id`: the first call records the publication for the active token's
    /// repository and its `publication_requested` event; a repeat with the same manifest returns the
    /// row. A token may have at most `MAX_UNFINISHED` open publications.
    fn create_publication(&self, token: &str, new: &NewPublication, now: i64) -> Result<Create> {
        self.create_bound_publication(token, new, None, now)
    }
    /// Binding is server-created authorization context, never part of a client manifest.
    fn create_bound_publication(
        &self,
        token: &str,
        new: &NewPublication,
        binding: Option<&str>,
        now: i64,
    ) -> Result<Create>;
    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>>;
    /// Publications of `repository` (matched exactly, as [`Store::publication`]), newest first by
    /// `created_at`, then `id`, strictly older than the `before` publication, at most `limit`.
    fn publications(
        &self,
        repository: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Publication>>;
    /// Like [`Store::publications`] but only matches of `search` (already [`search_text`]-cleaned)
    /// among the newest `SEARCH_SCAN_ROWS` publications below `before`: branch, base, title,
    /// id, owner login or token name containing it, or the pull request number it names. Never
    /// reads more than that window; the result says where it stopped.
    fn publication_search(
        &self,
        repository: &str,
        before: Option<&str>,
        search: &str,
        limit: usize,
    ) -> Result<PublicationSearch>;
    /// The newest publication of each pull request number above `after` in `repository`
    /// (matched case-insensitively), omitting pull requests whose newest publication is merged,
    /// by number, at most `limit`. "Newest" is by `created_at`, then `id`, descending.
    fn pull_publications(
        &self,
        repository: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Publication>>;
    /// Applies one forge fact (see [`Update`]); an event only when the state changes. Unknown id is `Invalid`.
    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication>;
    /// Propose or atomically apply a repair for one exact retained publication-projection failure.
    fn repair_catchup_publication_merge(
        &self,
        repair: &CatchupPublicationRepair,
        now: i64,
    ) -> Result<CatchupPublicationRepairResult>;
    /// Record one immutable provider-confirmed merge observation. Exact repository, PR and merge
    /// commit duplicates return the retained observation without changing its ingestion time.
    fn record_catchup_merge(
        &self,
        scope: &CatchupRepositoryScope,
        merge: &NewCatchupMerge,
        observed_at: i64,
    ) -> Result<CatchupMergeObservation>;
    fn catchup_merge_known(
        &self,
        scope: &CatchupRepositoryScope,
        pull_number: i64,
        merge_commit: Option<&str>,
    ) -> Result<bool>;
    /// Exact retained provider merge facts for this PR, ordered by merge commit. At most 17;
    /// 17 means the caller must treat evidence as incomplete rather than infer a single head.
    fn catchup_merges_for_pull(
        &self,
        scope: &CatchupRepositoryScope,
        pull_number: i64,
    ) -> Result<Vec<CatchupMergeObservation>>;
    fn record_catchup_source_status(
        &self,
        scope: &CatchupRepositoryScope,
        status: &CatchupSourceStatus,
    ) -> Result<()>;
    /// Events with `seq` greater than the argument, oldest first, at most 1000.
    /// Retained events in sequence order; operator-compacted work events can leave gaps.
    fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<HubEvent>>;
    /// The highest event `seq`, or 0 with no events.
    fn latest_event_seq(&self) -> Result<i64>;
    fn work_start(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult> {
        self.work_start_named(token, start, true, now)
    }
    /// `work_start`, where `named` is false when the caller named no session and `start.session`
    /// is the one the Hub resolved for it. That session is not the call's own, so such a start
    /// never holds its lapsed scope again.
    fn work_start_named(
        &self,
        token: &str,
        start: &WorkStart,
        named: bool,
        now: i64,
    ) -> Result<WorkStartResult>;
    /// Atomic compatibility start: reuse this session's unfinished lead work, or create one.
    fn work_start_auto(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult>;
    fn work_checkpoint(
        &self,
        token: &str,
        checkpoint: &WorkCheckpoint,
        now: i64,
    ) -> Result<WorkCheckpointResult>;
    /// Scoped read. Person callers must first check the current repository role; no token is minted.
    fn work_get_scoped(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        now: i64,
    ) -> Result<Option<WorkItem>>;
    fn work_get(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkItem>>;
    /// Read retained intent in one transaction without changing Work or delivery state.
    fn work_intent(
        &self,
        token: &str,
        id: &str,
        now: i64,
    ) -> Result<Option<super::work_intent::WorkIntentV1>>;
    /// Set/remove a caller-declared issue link without changing Work identity.
    fn work_issue_link_set(
        &self,
        token: &str,
        update: &super::work_issues::WorkIssueLinkUpdate,
        now: i64,
    ) -> Result<WorkCheckpointResult>;
    /// Read the single explicit PR relationship; comparison statuses confer no approval.
    fn work_pr_link_get(
        &self,
        token: &str,
        id: &str,
        query: &WorkPrLinkQuery,
        now: i64,
    ) -> Result<Option<WorkPrLinkView>>;
    /// Scoped read of the explicit PR target alone, for person and agent readers alike.
    fn work_pr_target_scoped(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        now: i64,
    ) -> Result<Option<super::work_links::WorkPrLinkTarget>>;
    /// Set or remove a caller-asserted relationship, fenced by current Work ownership.
    fn work_pr_link_set(
        &self,
        token: &str,
        update: &WorkPrLinkUpdate,
        now: i64,
    ) -> Result<WorkPrLinkResult>;
    /// Read retained reconciliation and expose it as current only while its facts still match.
    /// Scoped read. Person callers must first check the current repository role; no token is minted.
    fn work_reconciliation_get_scoped(
        &self,
        reader: &WorkLifecycleReader<'_>,
        id: &str,
        now: i64,
    ) -> Result<Option<super::work_reconciliation::ReconciliationView>>;
    fn work_reconciliation_get(
        &self,
        token: &str,
        id: &str,
        now: i64,
    ) -> Result<Option<super::work_reconciliation::ReconciliationView>>;
    /// Persist server-read evidence with Work/declaration fences and an idempotent receipt.
    fn work_reconciliation_apply(
        &self,
        token: &str,
        request: &super::work_reconciliation::ReconciliationApply,
        now: i64,
    ) -> Result<super::work_reconciliation::ReconciliationResult>;
    /// Read durable declarations without modifying legacy checkpoint relations.
    fn work_declarations_get(
        &self,
        token: &str,
        id: &str,
        now: i64,
    ) -> Result<Option<super::work_reconciliation::WorkDeclarationsView>>;
    /// Replace durable declarations as the fenced lead, with an idempotent receipt.
    fn work_declarations_set(
        &self,
        token: &str,
        update: &super::work_reconciliation::WorkDeclarationsUpdate,
        now: i64,
    ) -> Result<super::work_reconciliation::WorkDeclarationsResult>;
    /// Release, retain, finish or reopen Work as its lead, owner or an administrator. One
    /// transaction, idempotent by `operation_id`, fenced by generation and `updated_seq`.
    fn work_transition(
        &self,
        actor: &WorkTransitionActor<'_>,
        request: &WorkTransitionRequest,
        now: i64,
    ) -> Result<WorkTransitionResult>;
    /// The `WorkOperationalV1` projection of the selected Work, ordered by id and bounded to
    /// 100, in one snapshot. Never writes.
    fn work_operational(
        &self,
        reader: &WorkLifecycleReader<'_>,
        select: &OperationalSelect<'_>,
        now: i64,
    ) -> Result<OperationalOverviewV1>;

    /// What a recovery inspection or preview needs, in one snapshot: the page of the reader
    /// owner's unfinished Work after the cursor (or the one `work_id`), the live holds over the
    /// paths with their exact overlaps, the owner's unfinished Work whose scope overlaps them,
    /// and for each listed Work its open coordination requests and unfinished dependents.
    /// Never writes. A person reader must already have passed the repository role check.
    fn work_recovery_read(
        &self,
        reader: &WorkLifecycleReader<'_>,
        read: &super::work_recovery::RecoveryRead,
        now: i64,
    ) -> Result<super::work_recovery::RecoveryFacts>;

    /// Work of the token's repository, finished work included, newest `created_at` then `id`
    /// first, strictly after the `cursor` position (`<created_at>:<id>` from a previous page; it must name a work item of the token's repository with exactly that creation time), at most `limit` (see `MAX_WORK_SEARCH`).
    /// `search` (already [`search_text`]-cleaned) keeps items whose title, branch, owner login,
    /// id, scope or decision checkpoint summary contains it, whose source issue number it names,
    /// or whose lead agent label contains it, except another owner's generated `client-…` label,
    /// which is masked.
    /// An unknown cursor is `Invalid`.
    fn work_search(
        &self,
        token: &str,
        search: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<WorkSearchPage>;
    /// The Work of the token's repository recorded for a commit or a path, with its decisions
    /// (see [`super::why`]). A commit or path with no recorded Work answers `known: false`.
    fn work_why(
        &self,
        token: &str,
        subject: &super::why::WhySubject,
        now: i64,
    ) -> Result<super::why::WhyAnswer>;
    fn change_ingest(
        &self,
        token: &str,
        batch: &CaptureBatchV1,
        now: i64,
    ) -> Result<IngestReceiptV1>;
    fn change_action_start(
        &self,
        token: &str,
        start: &ActionStartV1,
        now: i64,
    ) -> Result<ActionMutationReceiptV1>;
    fn change_action_finish(
        &self,
        token: &str,
        finish: &ActionFinishV1,
        now: i64,
    ) -> Result<ActionMutationReceiptV1>;
    fn change_record(
        &self,
        token: &str,
        work_id: &str,
        now: i64,
    ) -> Result<Option<ChangeRecordSummaryV1>>;
    /// Every declaration of (work, digest) when `token`'s repository scope contains the Work,
    /// with the caller's human owner. `None` for an unknown Work; an undeclared digest has no rows.
    fn shared_body_lookup(
        &self,
        token: &str,
        work_id: &str,
        digest: &str,
        now: i64,
    ) -> Result<Option<SharedBodyAccess>>;
    /// The caller's own declarations among `digests` (none of anyone else's), when `token`'s
    /// repository scope contains the Work. `None` for an unknown Work.
    fn shared_bodies_own(
        &self,
        token: &str,
        work_id: &str,
        digests: &[String],
        now: i64,
    ) -> Result<Option<Vec<SharedBodyRow>>>;
    /// Up to `MAX_CONVERSATION_PAGE_ENTRIES + 1` shared-conversation entries of the Work after
    /// `cursor`, ordered by (session_id bytewise, sequence, digest), when `token`'s repository
    /// scope contains it: the caller's own declarations, or every author's when `all_authors`
    /// (a Hub admin; see `conversation_readable`). `None` for an unknown Work; a malformed cursor
    /// is `Invalid`.
    fn shared_conversation_page(
        &self,
        token: &str,
        work_id: &str,
        cursor: Option<&str>,
        all_authors: bool,
        now: i64,
    ) -> Result<Option<Vec<ConversationEntryV1>>>;
    /// Moves `author`'s `declared` body to `available` with `expires_at = now + retention_seconds`.
    /// Returns the state afterwards (`None` for an unknown row), so a caller that lost a race
    /// to a delete or expiry sees it and must remove the object it wrote.
    fn shared_body_mark_available(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
        retention_seconds: i64,
    ) -> Result<Option<SharedBodyState>>;
    /// Marks `author`'s body (every author's when `None`) deleted on behalf of user `actor`, and
    /// returns the rows afterwards (empty for an unknown body). An already deleted row keeps its
    /// first tombstone.
    fn shared_body_tombstone(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
        actor: &str,
        now: i64,
    ) -> Result<Vec<SharedBodyRow>>;
    /// Marks up to `limit` available bodies with `expires_at <= now` expired, and declared
    /// bodies still not uploaded `retention_seconds` after `declared_at` (they never had an
    /// object of their own), oldest first, and returns them.
    fn shared_bodies_expire(
        &self,
        now: i64,
        limit: usize,
        retention_seconds: i64,
    ) -> Result<Vec<SharedBodyRow>>;
    /// Moves `available` declarations of (work, digest) back to `declared` (only `author`'s when
    /// given) because their object is gone or was never written, so the author's preflight lists
    /// them missing and the client uploads again. Returns how many rows changed.
    fn shared_body_demote_available(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
    ) -> Result<usize>;
    /// Up to `limit` expired or deleted bodies whose object has not been confirmed removed and
    /// whose retry time has come, oldest tombstone first.
    fn shared_bodies_pending_physical_delete(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<SharedBodyRow>>;
    /// Whether any declaration of (work, digest) is `available`, and so needs the shared object.
    /// A `declared` one does not: its author's upload writes the object again.
    fn shared_body_object_in_use(&self, work_id: &str, digest: &str) -> Result<bool>;
    fn shared_body_mark_physically_deleted(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
    ) -> Result<()>;
    /// Records a failed object removal: the row is pending again (even if it was marked removed)
    /// and is not retried before `retry_at`.
    fn shared_body_delete_failed(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        retry_at: i64,
    ) -> Result<()>;
    fn skill_adoption(&self, repo: &str) -> Result<Option<SkillAdoption>>;
    /// Stores an adoption and returns the one that applies: a `reported_by_checkout` row never
    /// replaces a `default_branch` row, which any `default_branch` row replaces.
    fn skill_adoption_put(&self, adoption: &SkillAdoption) -> Result<SkillAdoption>;
    /// Drops the repository's adoption from `source`, or from every source when `None`.
    fn skill_adoption_clear(&self, repo: &str, source: Option<&str>) -> Result<()>;
    fn skill_evidence_add(&self, row: &EvidenceRow) -> Result<()>;
    /// A run's evidence in recorded order.
    fn skill_evidence(&self, repo: &str, run_id: &str) -> Result<Vec<EvidenceRow>>;
    /// Runs of `repo`, most recently active first, optionally one branch or one run id.
    fn skill_run_heads(
        &self,
        repo: &str,
        branch: Option<&str>,
        run_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunHead>>;
    fn receipt_mode_get(&self, token: &str, session: &str, now: i64) -> Result<Option<String>>;
    fn receipt_mode_set(&self, token: &str, session: &str, mode: &str, now: i64) -> Result<()>;
    fn receipt_work_remember(&self, token: &str, session: &str, work: &str, now: i64)
    -> Result<()>;
    fn receipt_work_ids(&self, token: &str, session: &str, now: i64) -> Result<Vec<String>>;
    fn receipt_announce(
        &self,
        token: &str,
        session: &str,
        work: &str,
        generation: i64,
        version: &str,
        now: i64,
    ) -> Result<bool>;
    /// Repository token reads always recheck the live credential, including cached callers.
    fn value_scope(&self, token: &str, now: i64) -> Result<super::value_metrics::ValueScope>;
    fn value_metrics(
        &self,
        token: &str,
        query: &super::value_metrics::ValueMetricsQuery,
        now: i64,
    ) -> Result<super::value_metrics::ValueMetrics>;
    /// Caller must have just checked the person's current repository role, as for repository_work_overview.
    fn repository_value_facts(
        &self,
        who: &Principal,
        query: &super::value_metrics::ValueMetricsQuery,
        now: i64,
    ) -> Result<super::value_metrics::ValueFacts>;
    fn value_observe(
        &self,
        token: &str,
        observation: &super::value_metrics::ValueObservation,
        now: i64,
    ) -> Result<()>;
    /// Adds one hook call to its session's overhead row, for an authenticated token of
    /// `repository`; rows are kept 30 days and at most 1,000 per token (see `hook_overhead`).
    fn hook_overhead(
        &self,
        token: &str,
        repository: &str,
        sample: &super::hook_overhead::Sample,
        now: i64,
    ) -> Result<()>;
    fn work_usage(&self, token: &str, usage: &UsageRecord, now: i64) -> Result<UsageReceipt>;
    fn work_insight(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkInsight>>;
    fn work_insights(&self, token: &str, query: &InsightQuery, now: i64) -> Result<InsightPage>;
    /// Resolve a recorded holder identity within the caller's authorized repository. Never impersonates the holder.
    fn work_id_for_holder(
        &self,
        token: &str,
        holder_token: &str,
        holder_session: &str,
        now: i64,
    ) -> Result<Option<String>>;
    /// The token/session's one unfinished lead claim. Multiple matches are invalid.
    fn work_for_session(&self, token: &str, session: &str, now: i64) -> Result<Option<WorkItem>>;
    /// The token/session's unique current-generation participant Work for native change capture.
    /// Active, Paused and Blocked Work qualify (session end pauses it; resumed sessions keep
    /// capturing); Finished and Handover do not. Never changes the Work state.
    fn capture_work_for_session(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<WorkItem>>;
    /// Resolve the explicit or most recently touched session without touching it.
    fn session_name_for(&self, token: &str, session: Option<&str>, now: i64) -> Result<String>;
    /// Whether this token's unnamed `default` session is bound to one native hook session, so
    /// edit hooks link Work led by `default` to that one conversation's edits only.
    fn default_session_bound(&self, token: &str) -> Result<bool>;
    /// A tool call a hook reported that is not an edit check: a touch of the session, which
    /// renews its leases, and for a shell call the start or end that keeps them live while it
    /// runs, up to the in-flight cap.
    fn session_activity(
        &self,
        token: &str,
        call: &SessionCall,
        phase: super::work_lifecycle::CallPhase,
        now: i64,
    ) -> Result<()>;
    /// Records that the Hub's clock reads `now`. After a gap longer than a lease since the last
    /// observation, every reservation that was live then gets one lease of grace.
    fn observe_lease_clock(&self, now: i64) -> Result<()>;
    fn work_briefing(&self, token: &str, query: &BriefingQuery, now: i64) -> Result<Briefing>;
    /// Side-effect-free current page for hooks and implicit contexts; never ackable, with an
    /// isolated explicit subscriber/session cursor.
    fn work_briefing_preview_for(
        &self,
        token: &str,
        subscriber: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<Briefing>;
    /// Read-only current work, recent decisions/completions and declared-scope overlaps.
    fn work_overview(
        &self,
        token: &str,
        query: &WorkOverviewQuery,
        now: i64,
    ) -> Result<WorkOverview>;
    /// `work_overview` of one connected repository for `owner`, with no token: the caller has
    /// checked `owner`'s current role on it, as a mint would.
    fn repository_work_overview(
        &self,
        owner: &User,
        repository: &str,
        repository_id: i64,
        query: &WorkOverviewQuery,
        now: i64,
    ) -> Result<WorkOverview>;
    /// Check whether one lead session's durable claim covers requested paths. Never writes.
    fn work_scope_check(
        &self,
        token: &str,
        session: &str,
        scope: &[String],
        now: i64,
    ) -> Result<WorkScopeCheck>;
    fn coordination_inbox(
        &self,
        token: &str,
        query: &CoordinationInboxQuery,
        now: i64,
    ) -> Result<CoordinationInbox>;
    /// Scoped read. Person callers must first check the current repository role; no token is minted.
    fn coordination_list_scoped(
        &self,
        reader: &WorkLifecycleReader<'_>,
        query: &CoordinationListQuery,
        now: i64,
    ) -> Result<CoordinationInbox>;
    fn coordination_list(
        &self,
        token: &str,
        query: &CoordinationListQuery,
        now: i64,
    ) -> Result<CoordinationInbox>;
    fn coordination_ack(
        &self,
        token: &str,
        ack: &CoordinationAck,
        now: i64,
    ) -> Result<CoordinationRequest>;
    fn work_overlap_start(
        &self,
        token: &str,
        request: &super::work_overlap::WorkOverlapStart,
        now: i64,
    ) -> Result<WorkStartResult>;
    fn work_overlap_preflight(
        &self,
        token: &str,
        request: &super::work_overlap::WorkOverlapStart,
        now: i64,
    ) -> Result<super::work_preflight::WorkPreflightResult>;
    fn work_overlap_checkpoint(
        &self,
        token: &str,
        request: &super::work_overlap::WorkOverlapCheckpoint,
        now: i64,
    ) -> Result<WorkCheckpointResult>;
    /// Target lead consent alone grants no permission; requester acceptance is separately fenced.
    /// Latest durable prerequisite notices for this session's current lead Work items.
    /// Atomically reserves one bounded advisory summary attempt for this existing native session.
    /// Does not acknowledge the retained notices; explicit reads remain independent and pure.
    fn work_dependency_notice_delivery(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<super::work_dependencies::WorkDependencyNoticePage>>;
    fn work_dependency_notices(
        &self,
        token: &str,
        query: &super::work_dependencies::WorkDependencyNoticeQuery,
        now: i64,
    ) -> Result<super::work_dependencies::WorkDependencyNoticePage>;
    fn work_overlap_consent(
        &self,
        token: &str,
        request: &super::work_overlap::WorkOverlapConsentRequest,
        now: i64,
    ) -> Result<super::work_overlap::WorkOverlapConsentView>;
    /// Bounded current consent state, visible to the requester and affected current leads.
    fn work_overlap_consents(
        &self,
        token: &str,
        request_id: &str,
        session: &str,
        now: i64,
    ) -> Result<Vec<super::work_overlap::WorkOverlapConsentView>>;
    fn coordination_reply(
        &self,
        token: &str,
        reply: &CoordinationReply,
        now: i64,
    ) -> Result<CoordinationRequest>;
    fn coordination_resolve(
        &self,
        token: &str,
        resolve: &CoordinationResolve,
        now: i64,
    ) -> Result<CoordinationRequest>;
    fn coordination_withdraw(
        &self,
        token: &str,
        withdraw: &CoordinationWithdraw,
        now: i64,
    ) -> Result<CoordinationRequest>;
    fn work_ack(&self, token: &str, ack: &BriefingAck, now: i64) -> Result<()>;
    fn work_catchup(&self, token: &str, query: &CatchupQuery, now: i64) -> Result<CatchupPage>;
    fn work_catchup_preview(
        &self,
        token: &str,
        query: &CatchupPreviewQuery,
        now: i64,
    ) -> Result<CatchupPreview>;
    fn work_catchup_ack(&self, token: &str, ack: &CatchupAck, now: i64)
    -> Result<CatchupAckResult>;
    fn work_preflight(
        &self,
        token: &str,
        start: &WorkStart,
        now: i64,
    ) -> Result<WorkPreflightResult>;
    /// Local operator maintenance. `apply = false` is a side-effect-free preview.
    fn work_compact(
        &self,
        scope: &WorkRepositoryScope,
        before: i64,
        apply: bool,
        now: i64,
    ) -> Result<WorkCompaction>;
    /// Operator-only, transactional repair of a verified repository rename.
    fn work_repair_repository(
        &self,
        repair: &super::work::WorkRepositoryRename,
        now: i64,
    ) -> Result<super::work::WorkRepositoryRenameResult>;
    /// Every repository scope that retains Work items or coordination requests.
    fn work_compaction_scopes(&self) -> Result<Vec<WorkRepositoryScope>>;
    /// Bytes of stand-up payload the repository retains now, against `WORK_PAYLOAD_CAPACITY_BYTES`.
    /// A repository that never wrote reports 0.
    fn work_payload_used(&self, scope: &WorkRepositoryScope) -> Result<i64>;
    /// Trusted workspace calls only: authorization is held across this transaction.
    fn promise_create(
        &self,
        workspace_id: &str,
        actor_user: &str,
        request: &PromiseCreateV1,
        admissions: &[super::promise::PromiseRepositoryAdmissionV1],
        now: i64,
    ) -> Result<PromiseReceiptV1>;
    fn promise_adopt(
        &self,
        workspace_id: &str,
        actor_user: &str,
        request: &PromiseAdoptV1,
        admissions: &[super::promise::PromiseRepositoryAdmissionV1],
        now: i64,
    ) -> Result<PromiseReceiptV1>;
    #[allow(
        clippy::too_many_arguments,
        reason = "the trusted write carries independent actor, authority, expected state and source admission fencing"
    )]
    fn promise_change(
        &self,
        workspace_id: &str,
        actor_user: &str,
        promise_id: &str,
        authority: PromiseChangeAuthority,
        request: &PromiseChangeV1,
        admissions: &[super::promise::PromiseRepositoryAdmissionV1],
        current: &super::promise::PromiseCurrentAdmissionsV1,
        now: i64,
    ) -> Result<PromiseReceiptV1>;
    /// Recovery of an immutable, exact actor/target/body-bound receipt. The caller must hold
    /// live workspace read authorization; this grants no authority for a new transition.
    fn promise_change_replay(
        &self,
        workspace_id: &str,
        actor_user: &str,
        promise_id: &str,
        request: &PromiseChangeV1,
        current: &super::promise::PromiseCurrentAdmissionsV1,
        now: i64,
    ) -> Result<Option<PromiseReceiptV1>>;
    fn promise_input(
        &self,
        workspace_id: &str,
        actor_user: &str,
        actor_token: Option<&str>,
        request: &PromiseInputV1,
        admission: &super::promise::PromiseRepositoryAdmissionV1,
        now: i64,
    ) -> Result<PromiseInputReceiptV1>;
    fn promise_get(
        &self,
        workspace_id: &str,
        promise_id: &str,
        permitted_repositories: Option<&[String]>,
        current: &super::promise::PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseViewV1>>;
    fn promise_input_get(
        &self,
        workspace_id: &str,
        input_id: &str,
        permitted_repositories: Option<&[String]>,
        current: &super::promise::PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseInputViewV1>>;
    /// Proposal pages use a repository key; report pages use a promise id.
    fn promise_inputs(
        &self,
        workspace_id: &str,
        scope: PromiseInputScope<'_>,
        permitted_repositories: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
        current: &super::promise::PromiseCurrentAdmissionsV1,
    ) -> Result<PromiseInputPageV1>;
    fn promise_history(
        &self,
        workspace_id: &str,
        promise_id: &str,
        permitted_repositories: Option<&[String]>,
        before_version: Option<u64>,
        limit: usize,
        current: &super::promise::PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseHistoryPageV1>>;
    /// `None` is an authorized wildcard; `Some` is the complete current grant set.
    fn promise_list(
        &self,
        workspace_id: &str,
        permitted_repositories: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
        current: &super::promise::PromiseCurrentAdmissionsV1,
    ) -> Result<PromisePageV1>;
}

/// On Windows the database path may be a drive path: `C:\x`, `C:/x`, or `/C:/x` in URL form.
#[cfg(windows)]
fn windows_drive_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let b = path.as_bytes();
    (b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
        .then_some(path)
}

#[cfg(not(windows))]
fn windows_drive_path(_path: &str) -> Option<&str> {
    None
}

pub fn open(url: &str) -> Result<Arc<dyn Store>> {
    open_with(url, false)
}

/// Opens a Hub database that `hub serve` already created and migrated, for an operator command
/// working beside a running Hub. It creates nothing and migrates nothing: a missing database, or
/// one this binary would migrate, is refused, so a newer CLI never upgrades a running Hub's schema
/// and a mistyped URL never yields a new, empty database.
pub fn open_existing(url: &str) -> Result<Arc<dyn Store>> {
    open_with(url, true)
}

fn open_with(url: &str, existing: bool) -> Result<Arc<dyn Store>> {
    // Errors never echo the input: it may hold a password.
    if let Some(path) = url.strip_prefix("sqlite://") {
        let path = windows_drive_path(path).unwrap_or(path);
        let plain = (path.starts_with('/') || windows_drive_path(path).is_some())
            && path.len() > 1
            && !path.ends_with(['/', std::path::MAIN_SEPARATOR])
            && !path.contains(['?', '#', '%']);
        if !plain {
            return Err(invalid(
                "sqlite database path must be absolute, without ?, # or %, and not end in /",
            ));
        }
        Ok(if existing {
            Arc::new(SqliteStore::open_existing(Path::new(path))?)
        } else {
            Arc::new(SqliteStore::open(Path::new(path))?)
        })
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(if existing {
            Arc::new(PgStore::open_existing(url)?)
        } else {
            Arc::new(PgStore::open(url)?)
        })
    } else {
        Err(invalid(
            "AGENTSCM_DATABASE_URL must start with sqlite:// or postgres://",
        ))
    }
}

/// Refuses a database that this binary would migrate, given its applied migrations.
pub(super) fn ensure_current(versions: &[i64], names: &[String]) -> Result<()> {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    if pending(versions)?.is_empty() && plan_named(NAMED_MIGRATIONS, &names)?.is_empty() {
        Ok(())
    } else {
        Err(invalid(NOT_CURRENT))
    }
}

/// Said when an existing database is missing or would need migrating.
pub(super) const NOT_CURRENT: &str = "no Hub database of this version there: start the Hub with `agentscm hub serve` from the same release first";

/// The identity host of a person made on this computer with `agentscm hub local-token`. No forge
/// sign-in uses it, so such a person never matches a forge account or a forge admin entry.
pub const LOCAL_HOST: &str = "localhost";

pub fn agent_token_digest(raw: &str) -> Result<TokenDigest> {
    token_digest(AGENT_TOKEN_DIGEST_DOMAIN, raw)
        .map(TokenDigest)
        .map_err(|_| HubError::Unauthorized)
}

/// Web-page read tokens use a separate credential domain. Agent and Workspace
/// authentication cannot resolve them as agent credentials.
pub fn web_read_token_digest(raw: &str) -> Result<TokenDigest> {
    token_digest(WEB_READ_TOKEN_DIGEST_DOMAIN, raw)
        .map(TokenDigest)
        .map_err(|_| HubError::Unauthorized)
}

const BROWSER_SESSION_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.browser-session.v1\0";

/// A person keeps at most this many web-page sign-ins.
pub const MAX_BROWSER_SESSIONS: i64 = 8;
/// A web-page sign-in's expiry is moved forward only by at least this much, so every request
/// does not write.
pub const SLIDE_STEP_SECONDS: i64 = 5 * 60;
/// A web-page sign-in ends this long after it began, however often it is used.
pub const BROWSER_SESSION_MAX_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Digest of a web-page sign-in's cookie secret, domain-separated from every other credential.
pub fn browser_session_digest(raw: &str) -> Result<TokenDigest> {
    token_digest(BROWSER_SESSION_DIGEST_DOMAIN, raw)
        .map(TokenDigest)
        .map_err(|_| HubError::Unauthorized)
}

/// Digest of a computer credential, domain-separated from agent tokens.
pub fn device_digest(raw: &str) -> Result<TokenDigest> {
    token_digest(DEVICE_DIGEST_DOMAIN, raw)
        .map(TokenDigest)
        .map_err(|_| HubError::Unauthorized)
}

/// Create an agent token for `owner` that lives `ttl_seconds`; the secret is
/// returned once and only its digest is stored.
pub fn mint_agent_token(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    ttl_seconds: i64,
) -> Result<(String, Secret)> {
    mint(
        store,
        owner,
        name,
        repository,
        None,
        ttl_seconds,
        false,
        agent_token_digest,
    )
    .map(|(id, secret, _)| (id, secret))
}

/// Like [`mint_agent_token`], recording the forge's repository id; also
/// returns the stored expiry (unix seconds).
pub fn mint_bound_agent_token(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    repository_id: i64,
    ttl_seconds: i64,
    read_only: bool,
) -> Result<(String, Secret, i64)> {
    mint(
        store,
        owner,
        name,
        repository,
        Some(repository_id),
        ttl_seconds,
        read_only,
        agent_token_digest,
    )
}

/// Mint a Hub web-page token that cannot authenticate as a Workspace agent.
pub fn mint_web_read_token(
    store: &dyn Store,
    owner: &str,
    repository: &str,
    repository_id: i64,
    ttl_seconds: i64,
) -> Result<(String, Secret, i64)> {
    mint(
        store,
        owner,
        "hub-web",
        repository,
        Some(repository_id),
        ttl_seconds,
        true,
        web_read_token_digest,
    )
}

#[allow(clippy::too_many_arguments)] // private helper with three named wrappers; a parameter struct would only rename them
fn mint(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    repository_id: Option<i64>,
    ttl_seconds: i64,
    read_only: bool,
    digest: fn(&str) -> Result<TokenDigest>,
) -> Result<(String, Secret, i64)> {
    if !(1..=MAX_AGENT_TOKEN_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(invalid("token lifetime must be 1 second to 30 days"));
    }
    let secret = generate_secret().map_err(|_| HubError::Unavailable("random source".into()))?;
    let expires_at = unix_now() + ttl_seconds;
    let id = store.insert_agent_token(&NewAgentToken {
        owner: owner.to_owned(),
        name: name.to_owned(),
        repository: repository.to_owned(),
        digest: digest(secret.expose())?,
        expires_at,
        repository_id,
        read_only,
    })?;
    Ok((id, secret, expires_at))
}

/// 16 hex characters naming one session of a token on the status page, without exposing its id.
pub(super) fn session_key(token: &str, session: &str) -> String {
    let input = format!("agentscm.hub.session-view.v1\0{token}\0{session}");
    lower_hex_of(input.as_bytes())[..16].to_owned()
}

/// 16 hex characters naming one native session of an owner, the same in every repository on
/// this Hub, without exposing its id. `None` unless the id has the form of a UUID, as the session
/// IDs that native hooks report have: a name a client chose for itself (`default`, `main`) can
/// be two unrelated sessions in two repositories.
pub(super) fn owner_session(owner: &str, session: &str) -> Option<String> {
    let bytes = session.as_bytes();
    let uuid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    let input = format!("agentscm.hub.session-name.v1\0{owner}\0{session}");
    uuid.then(|| lower_hex_of(input.as_bytes())[..16].to_owned())
}

/// How long a record of which session started a session is kept.
pub(super) const SESSION_START_RETAIN_SECONDS: i64 = 30 * 86_400;
/// The most such records kept for one owner; the oldest give way. Every read of them loads them
/// all, and a caller may name any session ID as its own.
pub(super) const SESSION_STARTS_MAX: i64 = 4096;

/// The sessions that started `session`, nearest first, from an owner's start records
/// (`Store::session_starts`). Stops at a session it has already passed.
pub(super) fn session_ancestors<'a>(starts: &'a [(String, String)], session: &str) -> Vec<&'a str> {
    let mut found: Vec<&str> = Vec::new();
    let mut at = session;
    while let Some((_, parent)) = starts.iter().find(|(child, _)| child == at) {
        if parent == session || found.contains(&parent.as_str()) {
            break;
        }
        found.push(parent);
        at = parent;
    }
    found
}

/// Checks a new start record for `owner` against the records it already has.
pub(super) fn check_session_start(
    owner: &str,
    starts: &[(String, String)],
    session: &str,
    started_by: &str,
) -> Result<()> {
    if owner_session(owner, session).is_none() || owner_session(owner, started_by).is_none() {
        return Err(invalid(
            "started_by needs the native session IDs of both sessions",
        ));
    }
    if session == started_by || session_ancestors(starts, started_by).contains(&session) {
        return Err(invalid(
            "started_by names a session that this session started",
        ));
    }
    Ok(())
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn lower_hex_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    lower_hex(&Sha256::digest(bytes))
}

/// A new `con_` id, for a connection that needs its id before it is stored.
pub fn new_connection_id() -> Result<String> {
    new_id("con")
}

/// `<prefix>_` + 32 lowercase hex from the OS random source.
pub(super) fn new_id(prefix: &str) -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| HubError::Unavailable("random source".into()))?;
    Ok(format!("{prefix}_{}", lower_hex(&bytes)))
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

impl ForgeIdentity {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "github" | "gitlab") {
            return Err(invalid("provider must be github or gitlab"));
        }
        let host_ok = !self.host.is_empty()
            && self.host.len() <= MAX_HOST_BYTES
            && self
                .host
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':'));
        if !host_ok {
            return Err(invalid("host must be a lowercase hostname"));
        }
        if self.user_id <= 0 {
            return Err(invalid("forge user id must be positive"));
        }
        Ok(())
    }
}

impl NewAgentToken {
    pub(super) fn validate(&self, now: i64) -> Result<()> {
        validate_agent_name(&self.name)?;
        validate_repository(&self.repository)?;
        if self.expires_at <= now {
            return Err(invalid("token expiry must be in the future"));
        }
        if self.repository_id.is_some_and(|id| id <= 0) {
            return Err(invalid("repository id must be positive"));
        }
        Ok(())
    }
}

/// A state change recorded in `hub_events`, appended in the change's transaction.
pub(super) struct Event {
    pub id: String,
    pub at: i64,
    pub kind: &'static str,
    pub subject: String,
    /// Canonical JSON: serde_json orders object keys. Never holds secrets or credential digests.
    pub data: String,
}

impl Event {
    pub(super) fn new(kind: &'static str, subject: &str, data: Value) -> Result<Self> {
        Ok(Self {
            id: new_id("evt")?,
            at: unix_now(),
            kind,
            subject: subject.to_owned(),
            data: data.to_string(),
        })
    }

    pub(super) fn user_created(user: &str, identity: &ForgeIdentity, login: &str) -> Result<Self> {
        let data = json!({"provider": identity.provider, "host": identity.host,
            "user_id": identity.user_id, "login": login});
        Self::new("user_created", user, data)
    }

    pub(super) fn user_login_changed(user: &str, previous: &str, login: &str) -> Result<Self> {
        Self::new(
            "user_login_changed",
            user,
            json!({"login": login, "previous": previous}),
        )
    }

    /// `actor` is None for the library call and the acting user for an admin action.
    pub(super) fn user_toggled(user: &str, disabled: bool, actor: Option<&str>) -> Result<Self> {
        let kind = if disabled {
            "user_disabled"
        } else {
            "user_enabled"
        };
        Self::new(
            kind,
            user,
            actor.map_or_else(|| json!({}), |by| json!({"by": by})),
        )
    }

    pub(super) fn user_signed_in(user: &str, method: &str, admin: bool) -> Result<Self> {
        Self::new(
            "user_signed_in",
            user,
            json!({"method": method, "admin": admin}),
        )
    }

    pub(super) fn token_created(token: &str, new: &NewAgentToken) -> Result<Self> {
        let data = json!({"owner": new.owner, "name": new.name,
            "repository": new.repository, "repository_id": new.repository_id,
            "expires_at": new.expires_at, "read_only": new.read_only});
        Self::new("agent_token_created", token, data)
    }

    /// `by` is None for the library call and the owner for a self-service revoke.
    pub(super) fn token_revoked(token: &str, by: Option<&str>) -> Result<Self> {
        let data = by.map_or_else(|| json!({}), |by| json!({"by": by}));
        Self::new("agent_token_revoked", token, data)
    }

    pub(super) fn publication_merge_corrected(
        repository: &str,
        publication: &str,
        previous_merge_commit: Option<&str>,
        merge_commit: &str,
    ) -> Result<Self> {
        let previous_merge_commit_sha256 =
            previous_merge_commit.map(|value| lower_hex_of(value.as_bytes()));
        Self::new(
            "publication_merge_corrected",
            publication,
            json!({
                "repository": repository,
                "publication": publication,
                "previous_merge_commit_sha256": previous_merge_commit_sha256,
                "merge_commit": merge_commit,
            }),
        )
    }
}

impl Event {
    /// Ids, kind and generation only: public keys appear as key ids.
    fn local_ci_device(
        kind: &'static str,
        authorization: &DeviceAuthorization,
        repository: &str,
        by: &str,
        key_ids: &[&str],
    ) -> Result<Self> {
        Self::new(
            kind,
            &authorization.device_id,
            json!({"repository": repository, "generation": authorization.generation,
                "key_ids": key_ids, "by": by}),
        )
    }
}

/// Caller-chosen identity of one enroll or change, for idempotent replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCiRequest {
    /// 1 to 128 bytes of identifier characters.
    pub request_id: String,
    /// Lowercase SHA-256 hex of what the request asked for.
    pub fingerprint: String,
}

impl LocalCiRequest {
    pub(super) fn validate(&self) -> Result<()> {
        crate::evidence::validate_identifier("request id", &self.request_id)?;
        crate::evidence::validate_lower_hex("request fingerprint", &self.fingerprint, 32)?;
        Ok(())
    }
}

/// A stored authorization, verified against its row on every read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCiDevice {
    pub authorization: DeviceAuthorization,
    pub digest: String,
    /// The `hub_repositories.id` the device is bound to.
    pub repository: String,
    /// The Hub user id of the owner.
    pub owner: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalCiRefusal {
    UnknownDevice,
    DeviceExists,
    /// The stored generation is not the expected one.
    StaleGeneration {
        current: u64,
    },
    /// The request id was used for a different request.
    RequestConflict,
    /// A key id is, or once was, enrolled on a device.
    DuplicateKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalCiDeviceWrite {
    Applied(LocalCiDevice),
    /// Same request id and fingerprint as an applied write: the device as stored now.
    Replayed(LocalCiDevice),
    Refused(LocalCiRefusal),
}

/// A stored row as read, before verification.
pub(super) struct LocalCiRow {
    pub device_id: String,
    pub repository: String,
    pub owner: String,
    pub generation: i64,
    pub document: String,
    pub digest: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub(super) const LOCAL_CI_SELECT: &str = "SELECT device_id, repository, owner, generation, document, digest, created_at, updated_at FROM local_ci_devices";

fn corrupt() -> HubError {
    HubError::Unavailable("stored local CI device record is corrupt".into())
}

impl LocalCiRow {
    pub(super) fn verify(self) -> Result<LocalCiDevice> {
        let authorization =
            DeviceAuthorization::from_slice(self.document.as_bytes()).map_err(|_| corrupt())?;
        let digest = authorization.digest().map_err(|_| corrupt())?;
        if authorization.device_id != self.device_id
            || i64::try_from(authorization.generation).ok() != Some(self.generation)
            || digest != self.digest
        {
            return Err(corrupt());
        }
        Ok(LocalCiDevice {
            authorization,
            digest,
            repository: self.repository,
            owner: self.owner,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// What a backend writes for an accepted enroll or change.
pub(super) struct LocalCiPlan {
    pub document: String,
    pub digest: String,
    pub generation: i64,
    pub event: Event,
    /// Keys in the new document that the previous one lacked.
    pub new_keys: Vec<String>,
}

fn plan_local_ci(
    kind: &'static str,
    previous: Option<&DeviceAuthorization>,
    next: &DeviceAuthorization,
    repository: &str,
    by: &str,
    request: &LocalCiRequest,
) -> Result<LocalCiPlan> {
    request.validate()?;
    let document = String::from_utf8(next.canonical_bytes()?)
        .map_err(|_| HubError::Invalid("device authorization is not UTF-8".into()))?;
    let new_keys: Vec<String> = next
        .keys
        .iter()
        .filter(|key| previous.is_none_or(|p| !p.keys.iter().any(|k| k.key_id == key.key_id)))
        .map(|key| key.key_id.clone())
        .collect();
    let key_ids: Vec<&str> = new_keys.iter().map(String::as_str).collect();
    Ok(LocalCiPlan {
        digest: next.digest()?,
        generation: i64::try_from(next.generation)
            .map_err(|_| HubError::Invalid("generation out of range".into()))?,
        event: Event::local_ci_device(kind, next, repository, by, &key_ids)?,
        document,
        new_keys,
    })
}

pub(super) fn plan_local_ci_enroll(
    repository: &str,
    by: &str,
    authorization: &DeviceAuthorization,
    request: &LocalCiRequest,
) -> Result<LocalCiPlan> {
    authorization.validate()?;
    if authorization.generation != 1 || authorization.revoked_at_ms.is_some() {
        return Err(HubError::Invalid(
            "a new device authorization must be generation 1 and not revoked".into(),
        ));
    }
    plan_local_ci(
        "local_ci_device_enrolled",
        None,
        authorization,
        repository,
        by,
        request,
    )
}

pub(super) fn plan_local_ci_change(
    current: &LocalCiDevice,
    by: &str,
    next: &DeviceAuthorization,
    request: &LocalCiRequest,
) -> Result<LocalCiPlan> {
    let old = &current.authorization;
    next.validate()?;
    if next.device_id != old.device_id
        || next.repository != old.repository
        || next.owner != old.owner
        || next.generation != old.generation.saturating_add(1)
    {
        return Err(HubError::Invalid(
            "next authorization must be the same device one generation later".into(),
        ));
    }
    let kind = if next.revoked_at_ms.is_some() {
        "local_ci_device_revoked"
    } else {
        "local_ci_device_rotated"
    };
    plan_local_ci(kind, Some(old), next, &current.repository, by, request)
}

/// The hub repository's label as `Authorization.repository` spells it.
pub(super) fn local_ci_label(host: &str, full_name: &str) -> String {
    format!("{host}/{full_name}")
}

pub(super) fn event_limit(limit: usize) -> i64 {
    limit.min(MAX_EVENTS_PER_READ) as i64
}

pub(super) const LIST_LIMIT: i64 = 1000;

/// Longest list-search text, in bytes.
pub const MAX_SEARCH_BYTES: usize = 200;

/// Trimmed search text; empty means no filter. Over-long text or control characters are invalid.
pub fn search_text(q: Option<&str>) -> Result<Option<String>> {
    let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    if q.len() > MAX_SEARCH_BYTES || q.chars().any(char::is_control) {
        return Err(invalid(
            "q must be at most 200 bytes without control characters",
        ));
    }
    Ok(Some(q.to_owned()))
}

/// ASCII-case-insensitive (as SQLite `lower`) substring pattern for `lower(col) LIKE ? ESCAPE '\'`.
pub fn like_pattern(q: &str) -> String {
    let mut pattern = String::with_capacity(q.len() + 2);
    pattern.push('%');
    for c in q.to_ascii_lowercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// The record number named by `123` or `#123`, if the search text is one.
pub fn search_number(q: &str) -> Option<i64> {
    let digits = q.strip_prefix('#').unwrap_or(q);
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub fn validate_login(login: &str) -> Result<()> {
    if login.is_empty() || login.len() > MAX_LOGIN_BYTES || login.chars().any(char::is_control) {
        return Err(invalid(
            "login must be 1-255 bytes without control characters",
        ));
    }
    Ok(())
}

/// A forge profile name for display: trimmed, control characters removed, at most
/// `MAX_DISPLAY_NAME_CHARS` characters. `None` when nothing is left.
pub fn clean_display_name(raw: &str) -> Option<String> {
    let name: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name: String = name.trim().chars().take(MAX_DISPLAY_NAME_CHARS).collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

pub(super) fn validate_display_name(name: Option<&str>) -> Result<()> {
    match name {
        Some(name) if clean_display_name(name).as_deref() != Some(name) => {
            Err(invalid("name must be cleaned with clean_display_name"))
        }
        _ => Ok(()),
    }
}

pub fn validate_agent_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_AGENT_NAME_BYTES || !name.bytes().all(name_byte) {
        return Err(invalid("agent name must be 1-64 bytes of [A-Za-z0-9._-]"));
    }
    Ok(())
}

/// `owner/name`, or deeper for GitLab groups.
pub fn validate_repository(repository: &str) -> Result<()> {
    let segments_ok = repository.split('/').count() >= 2
        && repository.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(name_byte)
        });
    if repository.len() > MAX_REPOSITORY_BYTES || !segments_ok {
        return Err(invalid(
            "repository must be owner/name using [A-Za-z0-9._-]",
        ));
    }
    Ok(())
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

pub(super) struct Migration {
    pub version: i64,
    pub sqlite: &'static str,
    pub postgres: &'static str,
}

/// Integer hub migrations. This list is frozen.
///
/// A later schema change is a pair of files, `migrations/named/<name>.sqlite.sql`
/// and `migrations/named/<name>.pg.sql`. A `-- requires: <name>` comment names
/// another named migration that must already be applied. Applied names are a
/// set, so two branches can each add a file without taking the next integer.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sqlite: include_str!("migrations/0001_core.sqlite.sql"),
        postgres: include_str!("migrations/0001_core.pg.sql"),
    },
    Migration {
        version: 2,
        sqlite: include_str!("migrations/0002_login.sqlite.sql"),
        postgres: include_str!("migrations/0002_login.pg.sql"),
    },
    Migration {
        version: 3,
        sqlite: include_str!("migrations/0003_intents.sqlite.sql"),
        postgres: include_str!("migrations/0003_intents.pg.sql"),
    },
    Migration {
        version: 4,
        sqlite: include_str!("migrations/0004_github.sqlite.sql"),
        postgres: include_str!("migrations/0004_github.pg.sql"),
    },
    Migration {
        version: 5,
        sqlite: include_str!("migrations/0005_publish.sqlite.sql"),
        postgres: include_str!("migrations/0005_publish.pg.sql"),
    },
    Migration {
        version: 6,
        sqlite: include_str!("migrations/0006_sessions.sqlite.sql"),
        postgres: include_str!("migrations/0006_sessions.pg.sql"),
    },
    Migration {
        version: 7,
        sqlite: include_str!("migrations/0007_warn_once.sqlite.sql"),
        postgres: include_str!("migrations/0007_warn_once.pg.sql"),
    },
    Migration {
        version: 8,
        sqlite: include_str!("migrations/0008_read_only_tokens.sqlite.sql"),
        postgres: include_str!("migrations/0008_read_only_tokens.pg.sql"),
    },
    Migration {
        version: 9,
        sqlite: include_str!("migrations/0009_handoffs.sqlite.sql"),
        postgres: include_str!("migrations/0009_handoffs.pg.sql"),
    },
    Migration {
        version: 10,
        sqlite: include_str!("migrations/0010_work.sqlite.sql"),
        postgres: include_str!("migrations/0010_work.pg.sql"),
    },
    Migration {
        version: 11,
        sqlite: include_str!("migrations/0011_work_subscribers.sqlite.sql"),
        postgres: include_str!("migrations/0011_work_subscribers.pg.sql"),
    },
    Migration {
        version: 12,
        sqlite: include_str!("migrations/0012_work_subscriber_sessions.sqlite.sql"),
        postgres: include_str!("migrations/0012_work_subscriber_sessions.pg.sql"),
    },
    Migration {
        version: 13,
        sqlite: include_str!("migrations/0013_coordination_requests.sqlite.sql"),
        postgres: include_str!("migrations/0013_coordination_requests.pg.sql"),
    },
    Migration {
        version: 14,
        sqlite: include_str!("migrations/0014_coordination_retention.sqlite.sql"),
        postgres: include_str!("migrations/0014_coordination_retention.pg.sql"),
    },
    Migration {
        version: 15,
        sqlite: include_str!("migrations/0015_candidate_publications.sqlite.sql"),
        postgres: include_str!("migrations/0015_candidate_publications.pg.sql"),
    },
    Migration {
        version: 16,
        sqlite: include_str!("migrations/0016_forge_checks.sqlite.sql"),
        postgres: include_str!("migrations/0016_forge_checks.pg.sql"),
    },
    Migration {
        version: 17,
        sqlite: include_str!("migrations/0017_external_runs.sqlite.sql"),
        postgres: include_str!("migrations/0017_external_runs.pg.sql"),
    },
    Migration {
        version: 18,
        sqlite: include_str!("migrations/0018_hub_settings.sqlite.sql"),
        postgres: include_str!("migrations/0018_hub_settings.pg.sql"),
    },
    Migration {
        version: 19,
        sqlite: include_str!("migrations/0019_work_insights.sqlite.sql"),
        postgres: include_str!("migrations/0019_work_insights.pg.sql"),
    },
    Migration {
        version: 20,
        sqlite: include_str!("migrations/0020_unchecked_holds.sqlite.sql"),
        postgres: include_str!("migrations/0020_unchecked_holds.pg.sql"),
    },
];

#[derive(Debug)]
pub(super) struct NamedMigration {
    pub name: &'static str,
    pub requires: &'static [&'static str],
    pub sqlite: &'static str,
    pub postgres: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/hub_named_migrations.rs"));

/// Integer migration versions compiled into this binary, in apply order.
pub fn integer_migration_versions() -> Vec<i64> {
    MIGRATIONS
        .iter()
        .map(|migration| migration.version)
        .collect()
}

/// Named migration ids compiled into this binary, in name order.
pub fn named_migration_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = NAMED_MIGRATIONS
        .iter()
        .map(|migration| migration.name)
        .collect();
    names.sort_unstable();
    names
}

/// Named migrations still to apply. Already-applied names stay applied in any
/// order. A name this binary does not know means a newer database.
pub(super) fn plan_named<'a>(
    registry: &'a [NamedMigration],
    applied: &[&str],
) -> Result<Vec<&'a NamedMigration>> {
    for migration in registry {
        for requirement in migration.requires {
            if !registry.iter().any(|known| known.name == *requirement) {
                return Err(invalid("named migration requires an unknown migration"));
            }
        }
    }
    if applied
        .iter()
        .any(|name| !registry.iter().any(|known| known.name == *name))
    {
        return Err(invalid("database schema is newer than this binary"));
    }
    let mut pending: Vec<&NamedMigration> = registry
        .iter()
        .filter(|migration| !applied.contains(&migration.name))
        .collect();
    let mut ordered: Vec<&NamedMigration> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending.iter().position(|migration| {
            migration.requires.iter().all(|requirement| {
                applied.contains(requirement)
                    || ordered.iter().any(|done| done.name == *requirement)
            })
        });
        let Some(index) = ready else {
            return Err(invalid("named migrations have a cycle"));
        };
        ordered.push(pending.remove(index));
    }
    Ok(ordered)
}

/// Integer migrations not yet applied. The registry must be numbered 1..n and
/// the applied set must be a prefix of it; a newer database is refused.
/// Named migrations are not part of this prefix.
pub(super) fn pending(applied: &[i64]) -> Result<&'static [Migration]> {
    if MIGRATIONS
        .iter()
        .zip(1..)
        .any(|(m, version)| m.version != version)
    {
        return Err(invalid("migration registry must be numbered 1..n"));
    }
    if applied
        .iter()
        .any(|version| *version > MIGRATIONS.len() as i64)
    {
        return Err(invalid("database schema is newer than this binary"));
    }
    let mut applied = applied.to_vec();
    applied.sort_unstable();
    if applied
        .iter()
        .zip(1..)
        .any(|(version, expected)| *version != expected)
    {
        return Err(invalid(
            "applied migrations are not a prefix of this binary's",
        ));
    }
    Ok(&MIGRATIONS[applied.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_requires_an_applied_prefix() {
        let count = MIGRATIONS.len();
        assert_eq!(pending(&[]).unwrap().len(), count);
        assert_eq!(pending(&[1]).unwrap().len(), count - 1);
        assert_eq!(pending(&[1, 2, 3]).unwrap().len(), count - 3);
        assert_eq!(pending(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap().len(), count - 8);
        assert_eq!(
            pending(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap().len(),
            count - 9
        );
        assert_eq!(
            pending(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14])
                .unwrap()
                .len(),
            count - 14
        );
        let after_hub_settings = pending(&(1..=18).collect::<Vec<_>>()).unwrap();
        assert_eq!(after_hub_settings.len(), 2);
        assert_eq!(after_hub_settings[0].version, 19);
        assert!(
            after_hub_settings[0]
                .sqlite
                .contains("hub_work_model_usage")
        );
        assert!(
            after_hub_settings[0]
                .postgres
                .contains("hub_work_model_usage")
        );
        let after_work_insights = pending(&(1..=19).collect::<Vec<_>>()).unwrap();
        assert_eq!(after_work_insights.len(), 1);
        assert_eq!(after_work_insights[0].version, 20);
        assert!(after_work_insights[0].sqlite.contains("unchecked"));
        assert!(after_work_insights[0].postgres.contains("unchecked"));
        assert!(
            pending(&(1..=i64::try_from(count).unwrap()).collect::<Vec<_>>())
                .unwrap()
                .is_empty()
        );
        assert!(pending(&[2]).is_err());
        let newer = pending(&[1, 999]).err().unwrap().to_string();
        assert!(newer.contains("newer than this binary"), "{newer}");
    }

    fn named(name: &'static str, requires: &'static [&'static str]) -> NamedMigration {
        NamedMigration {
            name,
            requires,
            sqlite: "",
            postgres: "",
        }
    }

    #[test]
    fn named_migrations_apply_as_a_set() {
        let registry = [named("aaa", &[]), named("bbb", &["zzz"]), named("zzz", &[])];
        let names = |pending: Vec<&NamedMigration>| {
            pending
                .iter()
                .map(|migration| migration.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(plan_named(&registry, &[]).unwrap()),
            ["aaa", "zzz", "bbb"]
        );
        assert_eq!(
            names(plan_named(&registry, &["zzz"]).unwrap()),
            ["aaa", "bbb"]
        );
        assert!(
            plan_named(&registry, &["aaa", "bbb", "zzz"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn named_migrations_reject_a_cycle_an_unknown_requirement_and_a_newer_database() {
        let cycle = [named("aaa", &["bbb"]), named("bbb", &["aaa"])];
        let cyclic = plan_named(&cycle, &[]).unwrap_err().to_string();
        assert!(cyclic.contains("cycle"), "{cyclic}");
        let unknown = [named("aaa", &["missing"])];
        let missing = plan_named(&unknown, &[]).unwrap_err().to_string();
        assert!(missing.contains("unknown"), "{missing}");
        let registry = [named("aaa", &[])];
        let newer = plan_named(&registry, &["later"]).unwrap_err().to_string();
        assert!(newer.contains("newer than this binary"), "{newer}");
    }

    #[test]
    fn display_names_are_trimmed_stripped_and_capped() {
        assert_eq!(
            clean_display_name("  Jonny Hawley "),
            Some("Jonny Hawley".into())
        );
        assert_eq!(clean_display_name("A\u{7}\nB\u{1b}"), Some("AB".into()));
        assert_eq!(clean_display_name(" \t\n "), None);
        assert_eq!(clean_display_name(""), None);
        assert_eq!(clean_display_name(&"é".repeat(150)), Some("é".repeat(100)));
        // The cap never leaves trailing whitespace behind.
        assert_eq!(
            clean_display_name(&format!("{} tail", "é".repeat(99))),
            Some("é".repeat(99))
        );
        assert!(validate_display_name(Some(" padded")).is_err());
        assert!(validate_display_name(Some("Jonny")).is_ok());
        assert!(validate_display_name(None).is_ok());
    }

    #[test]
    fn publication_binding_is_a_named_migration() {
        let pending = plan_named(NAMED_MIGRATIONS, &[]).unwrap();
        let binding = pending
            .iter()
            .find(|migration| migration.name == "publication_binding")
            .unwrap();
        assert!(binding.requires.is_empty());
        assert!(binding.sqlite.contains("authorization_binding"));
        assert!(binding.postgres.contains("authorization_binding"));
        assert!(
            MIGRATIONS
                .iter()
                .all(|migration| !migration.sqlite.contains("authorization_binding"))
        );
    }
}
