//! Fail-closed Work access for explicitly managed local execution.
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::change_record::{ChangeRecordResponseV1, SCHEMA_VERSION_V1};
use agentscm::hub::store::unix_now;
use agentscm::hub::work::{
    WorkCheckpoint, WorkCheckpointResult, WorkItem, WorkRepositoryScope, WorkStart,
    WorkStartResult, WorkState,
};
use agentscm::{Error, Result};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::intent_cli::Hub;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Identity {
    token: Token,
    owner: Owner,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Token {
    id: String,
    repository: String,
    repository_id: Option<i64>,
    expires_at: i64,
    read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Owner {
    provider: String,
    host: String,
    user_id: i64,
}

impl Identity {
    pub(crate) fn token_id(&self) -> &str {
        &self.token.id
    }

    pub(crate) fn actor(&self) -> String {
        format!(
            "{}:{}:{}",
            self.owner.provider, self.owner.host, self.owner.user_id
        )
    }

    fn validate(&self, scope: &WorkRepositoryScope) -> Result<()> {
        if self.token.id.is_empty()
            || self.owner.user_id <= 0
            || self.token.read_only
            || self.token.expires_at <= unix_now()
            || self.token.repository_id.is_none_or(|id| id <= 0)
            || self.token.repository != scope.repository
            || self.token.repository_id != scope.repository_id
            || self.owner.provider != scope.provider
            || self.owner.host != scope.host
        {
            return Err(invalid(
                "current Hub identity does not authorize this repository",
            ));
        }
        Ok(())
    }
}

pub(crate) struct ManagedWork {
    root: PathBuf,
    scope: WorkRepositoryScope,
    hub: Hub,
    pub(crate) identity: Identity,
}

pub(crate) enum CheckpointAttempt {
    Recorded(Box<WorkCheckpointResult>),
    /// The Hub proved this operation absent before refusing its stale sequence.
    RejectedSequence(i64),
}

impl ManagedWork {
    pub(crate) async fn connect(root: &Path, scope: WorkRepositoryScope) -> Result<Self> {
        let (managed, _) = Self::discover(root).await?;
        managed.identity.validate(&scope)?;
        Ok(managed)
    }

    pub(crate) async fn discover(root: &Path) -> Result<(Self, WorkRepositoryScope)> {
        let root = root.canonicalize()?;
        let hub = Hub::connect_in(Duration::from_secs(10), &root)?;
        let identity: Identity =
            serde_json::from_value(hub.call(Method::GET, "/v1/hub/whoami", None).await?)?;
        let scope = WorkRepositoryScope {
            provider: identity.owner.provider.clone(),
            host: identity.owner.host.clone(),
            repository: identity.token.repository.clone(),
            repository_id: identity.token.repository_id,
        };
        identity.validate(&scope)?;
        validate_origin(&root, &scope)?;
        Ok((
            Self {
                root,
                scope: scope.clone(),
                hub,
                identity,
            },
            scope,
        ))
    }

    pub(crate) fn identity_commitment(&self) -> Result<String> {
        Ok(agentscm::store::digest(&serde_json::to_vec(
            &serde_json::json!({
                "domain": "agentscm/managed-work-identity/v1",
                "hub": self.hub.base,
                "scope": self.scope,
                "token_id": self.identity.token.id,
                "owner": self.identity.owner,
            }),
        )?))
    }

    pub(crate) async fn authenticate(&self) -> Result<()> {
        validate_origin(&self.root, &self.scope)?;
        let current = Hub::connect_in(Duration::from_secs(10), &self.root)?;
        if current.base != self.hub.base || current.token != self.hub.token {
            return Err(invalid(
                "checkout Hub connection changed; inspect before resuming",
            ));
        }
        let identity: Identity =
            serde_json::from_value(current.call(Method::GET, "/v1/hub/whoami", None).await?)?;
        identity.validate(&self.scope)?;
        if identity.token.id != self.identity.token.id || identity.owner != self.identity.owner {
            return Err(invalid("Hub execution identity changed"));
        }
        Ok(())
    }

    pub(crate) async fn read(&self, id: &str) -> Result<WorkItem> {
        if id.is_empty()
            || id.len() > 255
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("invalid Work identity"));
        }
        let item: WorkItem = serde_json::from_value(
            self.hub
                .call(Method::GET, &format!("/v1/hub/standup/work/{id}"), None)
                .await?,
        )?;
        if item.id != id || item.repository != self.scope.repository {
            return Err(invalid(
                "Hub returned Work for another identity or repository",
            ));
        }
        Ok(item)
    }

    pub(crate) async fn start(&self, request: &WorkStart) -> Result<WorkItem> {
        self.authenticate().await?;
        let result: WorkStartResult = serde_json::from_value(
            self.hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/start",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
        )?;
        if result.conflict.is_some()
            || result.coordination_request.is_some()
            || result.item.repository != self.scope.repository
            || result.item.lead_token != self.identity.token.id
            || result.item.lead_session != request.session
            || result.item.state != WorkState::Active
        {
            return Err(invalid(
                "Work was not acquired; inspect its current owner and blockers",
            ));
        }
        Ok(result.item)
    }

    pub(crate) async fn read_change_record(
        &self,
        work: &WorkItem,
    ) -> Result<ChangeRecordResponseV1> {
        self.authenticate().await?;
        let current = self.read(&work.id).await?;
        if current.generation != work.generation || current.updated_seq != work.updated_seq {
            return Err(invalid("Work changed while reading its evidence"));
        }
        let path = format!(
            "{}/change-records/{}",
            Hub::repo_path(&self.scope.repository),
            work.id
        );
        let record: ChangeRecordResponseV1 =
            serde_json::from_value(self.hub.call(Method::GET, &path, None).await?)?;
        if record.version != SCHEMA_VERSION_V1
            || record.summary.schema_version != SCHEMA_VERSION_V1
            || record.summary.work_id != work.id
            || record.summary.generation != work.generation
        {
            return Err(invalid("change record does not describe the current Work"));
        }
        record
            .validate_encoded_size()
            .map_err(crate::hub_cli::map_hub_error)?;
        Ok(record)
    }

    pub(crate) async fn check_current(&self, expected: &WorkItem) -> Result<WorkItem> {
        self.authenticate().await?;
        let current = self.read(&expected.id).await?;
        if current.generation != expected.generation
            || current.updated_seq != expected.updated_seq
            || current.lead_token != self.identity.token.id
            || current.lead_session != expected.lead_session
            || current.scope != expected.scope
            || current.state != WorkState::Active
        {
            return Err(invalid(
                "Work changed or is no longer active for this executor",
            ));
        }
        Ok(current)
    }

    pub(crate) async fn checkpoint(
        &self,
        request: &WorkCheckpoint,
    ) -> Result<WorkCheckpointResult> {
        self.authenticate().await?;
        let result: WorkCheckpointResult = serde_json::from_value(
            self.hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/checkpoint",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
        )?;
        if result.receipt.work_id != request.work_id
            || result.receipt.operation_id != request.operation_id
            || result.receipt.generation != request.generation
            || result.item.repository != self.scope.repository
        {
            return Err(invalid(
                "Hub checkpoint receipt did not match the requested operation",
            ));
        }
        Ok(result)
    }

    /// Only the dedicated server response proves rejection without commit. Other
    /// errors preserve the retained request, including malformed/oversized responses.
    pub(crate) async fn checkpoint_for_recovery(
        &self,
        request: &WorkCheckpoint,
    ) -> Result<CheckpointAttempt> {
        self.authenticate().await?;
        let call = self
            .hub
            .client
            .post(format!("{}/v1/hub/standup/checkpoint", self.hub.base))
            .bearer_auth(&self.hub.token)
            .timeout(self.hub.limit)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(request)?);
        let (status, body) = agentscm::hub::forge_http::send(call, 1024 * 1024)
            .await
            .map_err(|_| {
                invalid("recovery checkpoint outcome unknown; retained request preserved")
            })?;
        if status == 409
            && body.get("error").and_then(serde_json::Value::as_str)
                == Some("work_checkpoint_sequence_conflict")
        {
            let sequence = body
                .get("current_seq")
                .and_then(serde_json::Value::as_i64)
                .filter(|s| *s > 0 && Some(*s) != request.expected_updated_seq)
                .ok_or_else(|| invalid("invalid sequence rejection; retained request preserved"))?;
            return Ok(CheckpointAttempt::RejectedSequence(sequence));
        }
        if !(200..=299).contains(&status) {
            return Err(invalid(
                "recovery checkpoint unconfirmed; retained request preserved",
            ));
        }
        let result: WorkCheckpointResult = serde_json::from_value(body)?;
        if result.receipt.work_id != request.work_id
            || result.receipt.operation_id != request.operation_id
            || result.receipt.generation != request.generation
            || result.item.repository != self.scope.repository
        {
            return Err(invalid(
                "Hub recovery checkpoint receipt differs; retained request preserved",
            ));
        }
        Ok(CheckpointAttempt::Recorded(Box::new(result)))
    }
}

fn validate_origin(root: &Path, scope: &WorkRepositoryScope) -> Result<()> {
    let (host, repository) = crate::client_config::origin_identity(root)?;
    if host != scope.host || !repository.eq_ignore_ascii_case(&scope.repository) {
        return Err(invalid(
            "checkout origin does not match the current Hub repository",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_identity_requires_current_write_authority_and_exact_source() {
        let scope = WorkRepositoryScope {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "synthetic/project".into(),
            repository_id: Some(42),
        };
        let valid = Identity {
            token: Token {
                id: "token-1".into(),
                repository: scope.repository.clone(),
                repository_id: scope.repository_id,
                expires_at: unix_now() + 60,
                read_only: false,
            },
            owner: Owner {
                provider: scope.provider.clone(),
                host: scope.host.clone(),
                user_id: 7,
            },
        };
        assert!(valid.validate(&scope).is_ok());
        let from_api: Identity = serde_json::from_value(serde_json::json!({
            "token": {"id":"token-1", "repository":"synthetic/project", "repository_id":42,
                "expires_at":unix_now() + 60, "read_only":false},
            "owner": {"provider":"github", "host":"github.com", "user_id":7}
        }))
        .unwrap();
        assert_eq!(from_api.actor(), "github:github.com:7");
        assert!(from_api.validate(&scope).is_ok());
        let mut wrong = valid.clone();
        wrong.token.repository_id = None;
        assert!(wrong.validate(&scope).is_err());
        wrong = valid.clone();
        wrong.token.repository_id = Some(43);
        assert!(wrong.validate(&scope).is_err());
        wrong = valid.clone();
        wrong.token.read_only = true;
        assert!(wrong.validate(&scope).is_err());
        wrong = valid.clone();
        wrong.token.expires_at = unix_now();
        assert!(wrong.validate(&scope).is_err());
        wrong = valid;
        wrong.owner.host = "other.example".into();
        assert!(wrong.validate(&scope).is_err());
    }
}
