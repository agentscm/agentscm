//! Narrow in-process authority for human Slack coordination writes.
//!
//! A grant contains no bearer secret, has no wire representation and can only be created by
//! trusted Hub code from an already authenticated read-only principal. Every use revalidates the
//! original token and owner in the store transaction before this grant is considered.

use super::store::Principal;
use super::work::{WorkCheckpoint, WorkItem, WorkRole, WorkStart, WorkState, WorkWho};
use super::{HubError, Result};

const SLACK_SESSION: &str = "slack-human";
const SLACK_ID_PREFIX: &str = "slack-";
const SLACK_ID_HEX_BYTES: usize = 64;

/// Opaque authority for the two structured Slack coordination mutations.
///
/// Fields stay private and the type deliberately implements no serialization traits.
/// External callers cannot mint grants from a bearer principal:
///
/// ```compile_fail,E0624
/// use agentscm::hub::{delegation::CoordinationGrant, store::Principal};
/// fn mint(principal: &Principal) {
///     let _ = CoordinationGrant::for_principal(principal);
/// }
/// ```
pub struct CoordinationGrant {
    token: String,
    owner: String,
    provider: String,
    host: String,
    forge_user_id: i64,
    repository: String,
    repository_id: i64,
}

impl CoordinationGrant {
    /// Creates a grant only for a read-only token with a forge-stable repository identity.
    pub(in crate::hub) fn for_principal(principal: &Principal) -> Result<Self> {
        if !principal.read_only {
            return Err(HubError::Unauthorized);
        }
        let repository_id = principal.repository_id.ok_or(HubError::Unauthorized)?;
        Ok(Self {
            token: principal.token_id.clone(),
            owner: principal.owner.id.clone(),
            provider: principal.owner.identity.provider.clone(),
            host: principal.owner.identity.host.clone(),
            forge_user_id: principal.owner.identity.user_id,
            repository: principal.repository.clone(),
            repository_id,
        })
    }

    pub(in crate::hub) fn token(&self) -> &str {
        &self.token
    }

    /// Matches the grant to freshly loaded database identity and enables only the local call path.
    pub(in crate::hub) fn authorize_who(&self, who: &mut WorkWho) -> Result<()> {
        if !who.read_only
            || who.token != self.token
            || who.owner.id != self.owner
            || who.owner.identity.provider != self.provider
            || who.owner.identity.host != self.host
            || who.owner.identity.user_id != self.forge_user_id
            || who.repository != self.repository
            || who.repository_id != Some(self.repository_id)
        {
            return Err(HubError::Unauthorized);
        }
        who.read_only = false;
        Ok(())
    }

    pub(in crate::hub) fn validate_start(&self, start: &WorkStart) -> Result<()> {
        if start.work_id.is_some()
            || start.source.is_some()
            || start.takeover_generation.is_some()
            || start.branch.is_some()
            || start.worktree.is_some()
            || !start.depends_on.is_empty()
            || !start.related_work.is_empty()
            || start.role != WorkRole::Lead
            || start.session != SLACK_SESSION
            || !start.request_key.as_deref().is_some_and(valid_slack_id)
        {
            return Err(HubError::Invalid(
                "delegated Slack start must create lead-owned native work with its reserved session and request key"
                    .into(),
            ));
        }
        Ok(())
    }

    pub(in crate::hub) fn validate_checkpoint(&self, checkpoint: &WorkCheckpoint) -> Result<()> {
        if checkpoint.session != SLACK_SESSION
            || checkpoint.expected_updated_seq.is_none()
            || checkpoint.scope.is_some()
            || checkpoint.branch.is_some()
            || checkpoint.worktree.is_some()
            || !checkpoint.source_refs.is_empty()
            || !checkpoint.depends_on.is_empty()
            || !checkpoint.related_work.is_empty()
            || !valid_slack_id(&checkpoint.operation_id)
        {
            return Err(HubError::Invalid(
                "delegated Slack checkpoint requires its reserved session, operation key and work receipt"
                    .into(),
            ));
        }
        Ok(())
    }

    pub(in crate::hub) fn validate_checkpoint_item(&self, item: &WorkItem) -> Result<()> {
        if item.lead_token != self.token
            || item.lead_session != SLACK_SESSION
            || !matches!(item.state, WorkState::Active | WorkState::Blocked)
        {
            return Err(HubError::Unauthorized);
        }
        Ok(())
    }
}

fn valid_slack_id(value: &str) -> bool {
    value.strip_prefix(SLACK_ID_PREFIX).is_some_and(|suffix| {
        suffix.len() == SLACK_ID_HEX_BYTES
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::{ForgeIdentity, User};
    use crate::hub::work::{CheckpointKind, ForgeWorkRef, SourceRef};

    fn principal(read_only: bool, repository_id: Option<i64>) -> Principal {
        Principal {
            token_id: "tok_0123456789abcdef0123456789abcdef".into(),
            name: "slack-coordination".into(),
            repository: "acme/widgets".into(),
            repository_id,
            expires_at: 10_000,
            read_only,
            owner: User {
                id: "usr_0123456789abcdef0123456789abcdef".into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 7,
                },
                login: "member".into(),
                disabled: false,
            },
        }
    }

    fn start() -> WorkStart {
        WorkStart {
            work_id: None,
            source: None,
            request_key: Some(format!("slack-{}", "a".repeat(64))),
            title: "Synthetic Slack work".into(),
            session: SLACK_SESSION.into(),
            role: WorkRole::Lead,
            scope: vec!["src/example.rs".into()],
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: vec![],
            related_work: vec![],
            takeover_generation: None,
        }
    }

    fn checkpoint() -> WorkCheckpoint {
        WorkCheckpoint {
            work_id: "wrk_0123456789abcdef0123456789abcdef".into(),
            operation_id: format!("slack-{}", "b".repeat(64)),
            generation: 1,
            session: SLACK_SESSION.into(),
            expected_updated_seq: Some(2),
            kind: CheckpointKind::Progress,
            summary: "Synthetic progress".into(),
            next_action: None,
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: false,
            source_refs: vec![],
            depends_on: vec![],
            related_work: vec![],
        }
    }

    #[test]
    fn only_read_only_stable_repository_principals_create_grants() {
        assert!(CoordinationGrant::for_principal(&principal(true, Some(9))).is_ok());
        assert!(CoordinationGrant::for_principal(&principal(false, Some(9))).is_err());
        assert!(CoordinationGrant::for_principal(&principal(true, None)).is_err());
    }

    #[test]
    fn delegated_start_accepts_only_new_reserved_native_work() {
        let grant = CoordinationGrant::for_principal(&principal(true, Some(9))).unwrap();
        assert!(grant.validate_start(&start()).is_ok());

        let mut invalid = start();
        invalid.request_key = Some(format!("slack-{}", "A".repeat(64)));
        assert!(grant.validate_start(&invalid).is_err());
        let mut invalid = start();
        invalid.work_id = Some("wrk_0123456789abcdef0123456789abcdef".into());
        assert!(grant.validate_start(&invalid).is_err());
        let mut invalid = start();
        invalid.source = Some(ForgeWorkRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "acme/widgets".into(),
            issue: "1".into(),
        });
        invalid.request_key = None;
        assert!(grant.validate_start(&invalid).is_err());
    }

    #[test]
    fn delegated_start_rejects_relationship_fields() {
        let grant = CoordinationGrant::for_principal(&principal(true, Some(9))).unwrap();
        let mut invalid = start();
        invalid.depends_on = vec!["wrk_11111111111111111111111111111111".into()];
        assert!(grant.validate_start(&invalid).is_err());

        let mut invalid = start();
        invalid.related_work = vec!["wrk_22222222222222222222222222222222".into()];
        assert!(grant.validate_start(&invalid).is_err());
    }

    #[test]
    fn delegated_checkpoint_requires_reserved_operation_and_receipt() {
        let grant = CoordinationGrant::for_principal(&principal(true, Some(9))).unwrap();
        let checkpoint = checkpoint();
        assert!(grant.validate_checkpoint(&checkpoint).is_ok());
        let mut invalid = checkpoint.clone();
        invalid.expected_updated_seq = None;
        assert!(grant.validate_checkpoint(&invalid).is_err());
        let mut invalid = checkpoint;
        invalid.session = "agent-session".into();
        assert!(grant.validate_checkpoint(&invalid).is_err());
    }

    #[test]
    fn delegated_checkpoint_rejects_agent_controlled_work_fields() {
        let grant = CoordinationGrant::for_principal(&principal(true, Some(9))).unwrap();

        let mut invalid = checkpoint();
        invalid.scope = Some(vec!["src/other.rs".into()]);
        assert!(grant.validate_checkpoint(&invalid).is_err());

        let mut invalid = checkpoint();
        invalid.branch = Some("feature/other".into());
        assert!(grant.validate_checkpoint(&invalid).is_err());

        let mut invalid = checkpoint();
        invalid.worktree = Some("synthetic-worktree".into());
        assert!(grant.validate_checkpoint(&invalid).is_err());

        let mut invalid = checkpoint();
        invalid.source_refs = vec![SourceRef::RepositoryPath {
            path: "src/other.rs".into(),
            line: Some(7),
        }];
        assert!(grant.validate_checkpoint(&invalid).is_err());

        let mut invalid = checkpoint();
        invalid.depends_on = vec!["wrk_11111111111111111111111111111111".into()];
        assert!(grant.validate_checkpoint(&invalid).is_err());

        let mut invalid = checkpoint();
        invalid.related_work = vec!["wrk_22222222222222222222222222222222".into()];
        assert!(grant.validate_checkpoint(&invalid).is_err());
    }
}
