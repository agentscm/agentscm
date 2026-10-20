//! Trusted-local registry for executors allowed to submit attributed workflow observations.
//!
//! These administrative methods authenticate no remote caller. Their caller is the trusted
//! local operator; future host adapters must enforce operator authorization before invoking them.

use ed25519_dalek::VerifyingKey;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{Authority, read_policy, sql_i64, sql_u64};
use crate::policy::issuer_key_id;
use crate::skill_workflow::{
    CollectorProfile, ExecutorBinding, ExecutorEnrollment, ExecutorRevocation,
};
use crate::{Error, Result};

const MAX_REGISTRATIONS: i64 = 4_096;
const MAX_COLLECTOR_PROFILES: i64 = 4_096;

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct CurrentExecutorRegistration {
    pub enrollment: ExecutorEnrollment,
    pub public_key_hex: String,
    pub enrolled_policy_epoch: u64,
    pub revocation: Option<ExecutorRevocation>,
}

impl Authority {
    /// Hold the native writer reservation through a short journal transition. Checks and
    /// publication use this same connection; this read-only transaction writes no authority
    /// state and does not provide distributed atomicity between the two databases.
    pub(crate) fn with_task_writer_fence<T>(
        &self,
        transition: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        self.require_writable()?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let result = transition(self)?;
        tx.rollback()?;
        Ok(result)
    }

    pub(crate) fn check_task_subject(
        &self,
        context: &crate::context::ReviewContext,
        subject: &crate::skill_workflow::WorkflowTaskSubject,
        binding: &ExecutorBinding,
        historical_cleanup: bool,
    ) -> Result<()> {
        context.validate()?;
        if self.repository_id()? != subject.repository || context.change != subject.change {
            return Err(Error::Conflict(
                "task repository or Change does not match authority".into(),
            ));
        }
        let change = self.change(&subject.change)?;
        if historical_cleanup {
            return Ok(());
        }
        if self.policy_epoch()? != subject.policy_epoch
            || context.policy_epoch != subject.policy_epoch
            || change.version != subject.change_version
            || change.context_digest != subject.context_digest
            || context.digest()? != subject.context_digest
        {
            return Err(Error::Conflict(
                "workflow task subject is stale under current authority".into(),
            ));
        }
        if self.review_context(&subject.context_digest)? != *context {
            return Err(Error::Invalid(
                "current task context bytes are inconsistent".into(),
            ));
        }
        match (&context.base, self.target(&context.target)?) {
            (Some(base), Some(target))
                if base.revision == target.revision && base.target_version == target.version => {}
            (None, None) => {}
            _ => return Err(Error::Conflict("workflow task target base is stale".into())),
        }
        let requirement = self.workflow_requirement(&context.target)?.ok_or_else(|| {
            Error::Conflict("target has no installed workflow requirement".into())
        })?;
        if requirement.digest()? != subject.requirement_digest {
            return Err(Error::Conflict("installed task requirement changed".into()));
        }
        self.require_current_workflow_executor(subject, binding)?;
        Ok(())
    }

    /// Registers operator-supplied public key material for an exact repository and step scope.
    /// No key is generated and no credential or broader source authority is granted.
    pub fn enroll_workflow_executor(
        &mut self,
        enrollment: &ExecutorEnrollment,
        public_key_hex: &str,
    ) -> Result<String> {
        self.require_writable()?;
        let canonical = enrollment.canonical_bytes()?;
        let digest = enrollment.digest()?;
        validate_public_key(enrollment, public_key_hex)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored, stored_key)) = tx
            .query_row(
                "SELECT canonical_json,public_key_hex FROM workflow_executor_enrollments WHERE registration_id=?1",
                [&enrollment.registration_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored == canonical && stored_key == public_key_hex {
                tx.commit()?;
                return Ok(digest);
            }
            return Err(Error::Conflict(format!(
                "executor registration {} is already bound to different bytes",
                enrollment.registration_id
            )));
        }
        let authority_repository: String = tx.query_row(
            "SELECT repository_id FROM authority_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if enrollment.repository != authority_repository {
            return Err(Error::Conflict(
                "executor enrollment repository is not this authority".into(),
            ));
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM workflow_executor_enrollments",
            [],
            |row| row.get(0),
        )?;
        if count >= MAX_REGISTRATIONS {
            return Err(Error::Limit("executor enrollment inventory is full".into()));
        }
        let latest: Option<i64> = tx.query_row(
            "SELECT max(registration_epoch) FROM workflow_executor_enrollments WHERE repository=?1 AND executor_id=?2",
            params![enrollment.repository, enrollment.executor_id],
            |row| row.get(0),
        )?;
        if latest.is_some_and(|value| enrollment.registration_epoch <= value as u64) {
            return Err(Error::Conflict(
                "executor registration epoch must increase monotonically".into(),
            ));
        }
        let current = read_policy(&tx)?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| Error::Limit("policy epoch exhausted".into()))?;
        if tx.execute(
            "UPDATE authority_state SET policy_epoch=?1 WHERE singleton=1 AND policy_epoch=?2",
            params![next, current],
        )? != 1
        {
            return Err(Error::Conflict(
                "policy epoch changed while enrolling executor".into(),
            ));
        }
        tx.execute(
            "INSERT INTO workflow_executor_enrollments(registration_id,executor_id,repository,registration_epoch,verification_key_id,public_key_hex,enrolled_policy_epoch,canonical_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                enrollment.registration_id,
                enrollment.executor_id,
                enrollment.repository,
                sql_i64("registration epoch", enrollment.registration_epoch)?,
                enrollment.verification_key_id,
                public_key_hex,
                next,
                canonical,
            ],
        )?;
        tx.commit()?;
        Ok(digest)
    }

    /// Revokes exactly one immutable registration epoch and advances policy atomically.
    pub fn revoke_workflow_executor(&mut self, revocation: &ExecutorRevocation) -> Result<String> {
        self.require_writable()?;
        let canonical = revocation.canonical_bytes()?;
        let digest = revocation.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(stored) = tx
            .query_row(
                "SELECT canonical_json FROM workflow_executor_revocations WHERE registration_id=?1 AND registration_epoch=?2",
                params![revocation.registration_id, sql_i64("registration epoch", revocation.registration_epoch)?],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        {
            if stored == canonical {
                tx.commit()?;
                return Ok(digest);
            }
            return Err(Error::Conflict(
                "executor revocation is already bound to different bytes".into(),
            ));
        }
        let registration = read_current_registration(&tx, &revocation.registration_id)?
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "executor registration {}",
                    revocation.registration_id
                ))
            })?;
        let authority_repository: String = tx.query_row(
            "SELECT repository_id FROM authority_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if !revocation.matches_binding(
            &registration.enrollment.repository,
            &registration.enrollment.binding()?,
        )? || revocation.repository != authority_repository
        {
            return Err(Error::Conflict(
                "executor revocation does not match the authoritative registration".into(),
            ));
        }
        let current = read_policy(&tx)?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| Error::Limit("policy epoch exhausted".into()))?;
        if revocation.revoked_at_policy_epoch != sql_u64("policy epoch", next)? {
            return Err(Error::Conflict(
                "executor revocation must bind the next policy epoch".into(),
            ));
        }
        if tx.execute(
            "UPDATE authority_state SET policy_epoch=?1 WHERE singleton=1 AND policy_epoch=?2",
            params![next, current],
        )? != 1
        {
            return Err(Error::Conflict(
                "policy epoch changed while revoking executor".into(),
            ));
        }
        tx.execute(
            "INSERT INTO workflow_executor_revocations(registration_id,registration_epoch,revoked_policy_epoch,canonical_json) VALUES(?1,?2,?3,?4)",
            params![
                revocation.registration_id,
                sql_i64("registration epoch", revocation.registration_epoch)?,
                next,
                canonical,
            ],
        )?;
        tx.commit()?;
        Ok(digest)
    }

    pub fn workflow_executor_registration(
        &self,
        registration_id: &str,
    ) -> Result<Option<CurrentExecutorRegistration>> {
        if registration_id.is_empty() || registration_id.len() > 128 {
            return Err(Error::Invalid("invalid executor registration ID".into()));
        }
        read_current_registration(&self.connection, registration_id)
    }

    /// Installs immutable operator-owned collector authority for one already-enrolled step.
    /// Existing enrollments remain observation-only unless this separate profile exists.
    pub fn install_workflow_collector_profile(
        &mut self,
        profile: &CollectorProfile,
    ) -> Result<String> {
        self.require_writable()?;
        let canonical = profile.canonical_bytes()?;
        let digest = profile.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored_digest, stored)) = tx
            .query_row(
                "SELECT digest,canonical_json FROM workflow_collector_profiles WHERE profile_id=?1",
                [&profile.profile_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if stored_digest == digest && stored == canonical {
                tx.commit()?;
                return Ok(digest);
            }
            return Err(Error::Conflict(format!(
                "collector profile {} is already bound to different bytes",
                profile.profile_id
            )));
        }
        let registration = read_current_registration(&tx, &profile.registration_id)?
            .ok_or_else(|| Error::Conflict("collector executor is not enrolled".into()))?;
        let binding = registration.enrollment.binding()?;
        if registration.revocation.is_some()
            || registration.enrollment.repository != profile.repository
            || !registration.enrollment.step_ids.contains(&profile.step_id)
            || binding.registration_epoch != profile.registration_epoch
            || binding.verification_key_id != profile.verification_key_id
        {
            return Err(Error::Conflict(
                "collector profile does not match a current exact-step enrollment".into(),
            ));
        }
        let repository: String = tx.query_row(
            "SELECT repository_id FROM authority_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if profile.repository != repository {
            return Err(Error::Conflict(
                "collector profile repository is not this authority".into(),
            ));
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM workflow_collector_profiles",
            [],
            |row| row.get(0),
        )?;
        if count >= MAX_COLLECTOR_PROFILES {
            return Err(Error::Limit("collector profile inventory is full".into()));
        }
        // The immutable profile is subordinate to this exact registration epoch. Installing it
        // grants no repository authority on its own; enrollment/re-enrollment and revocation are
        // the policy-epoch transitions. This keeps one pinned profile usable across revisions.
        let current = read_policy(&tx)?;
        tx.execute(
            "INSERT INTO workflow_collector_profiles(profile_id,digest,registration_id,registration_epoch,repository,step_id,installed_policy_epoch,canonical_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                profile.profile_id,
                digest,
                profile.registration_id,
                sql_i64("registration epoch", profile.registration_epoch)?,
                profile.repository,
                profile.step_id,
                current,
                canonical,
            ],
        )?;
        tx.commit()?;
        Ok(digest)
    }

    pub fn workflow_collector_profile(&self, profile_id: &str) -> Result<Option<CollectorProfile>> {
        if profile_id.is_empty() || profile_id.len() > 128 {
            return Err(Error::Invalid("invalid collector profile ID".into()));
        }
        read_collector_profile(&self.connection, profile_id)
    }

    pub fn require_current_collector_profile(
        &self,
        profile_id: &str,
        task: &crate::skill_workflow::WorkflowTask,
        binding: &ExecutorBinding,
    ) -> Result<CollectorProfile> {
        let profile = self
            .workflow_collector_profile(profile_id)?
            .ok_or_else(|| Error::Conflict("collector profile is not installed".into()))?;
        let audience = self.repository_id()?;
        profile.validate_for(task, binding, &audience)?;
        self.require_current_workflow_executor(&task.subject, binding)?;
        Ok(profile)
    }

    /// Loads authoritative key and revocation state, then checks exact scope and binding.
    pub fn require_current_workflow_executor(
        &self,
        subject: &crate::skill_workflow::WorkflowTaskSubject,
        binding: &ExecutorBinding,
    ) -> Result<CurrentExecutorRegistration> {
        let registration = self
            .workflow_executor_registration(&binding.registration_id)?
            .ok_or_else(|| Error::Conflict("executor is not enrolled".into()))?;
        registration
            .enrollment
            .validate_scope_and_binding(subject, binding)?;
        if registration.revocation.is_some() {
            return Err(Error::Conflict("executor registration is revoked".into()));
        }
        validate_public_key(&registration.enrollment, &registration.public_key_hex)?;
        Ok(registration)
    }
}

pub(super) fn create_schema_v8(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE workflow_executor_enrollments(
            registration_id TEXT PRIMARY KEY CHECK(length(registration_id) BETWEEN 1 AND 128),
            executor_id TEXT NOT NULL CHECK(length(executor_id) BETWEEN 1 AND 128),
            repository TEXT NOT NULL CHECK(length(repository)=32),
            registration_epoch INTEGER NOT NULL CHECK(registration_epoch>0),
            verification_key_id TEXT NOT NULL CHECK(length(verification_key_id)=64),
            public_key_hex TEXT NOT NULL CHECK(length(public_key_hex)=64),
            enrolled_policy_epoch INTEGER NOT NULL CHECK(enrolled_policy_epoch>0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=262144),
            UNIQUE(repository,executor_id,registration_epoch)
         ) STRICT;
         CREATE TABLE workflow_executor_revocations(
            registration_id TEXT NOT NULL REFERENCES workflow_executor_enrollments(registration_id),
            registration_epoch INTEGER NOT NULL CHECK(registration_epoch>0),
            revoked_policy_epoch INTEGER NOT NULL CHECK(revoked_policy_epoch>0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=262144),
            PRIMARY KEY(registration_id,registration_epoch)
         ) STRICT;",
    )?;
    Ok(())
}

pub(super) fn create_schema_v9(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE workflow_collector_profiles(
            profile_id TEXT PRIMARY KEY CHECK(length(profile_id) BETWEEN 1 AND 128),
            digest TEXT NOT NULL UNIQUE CHECK(length(digest)=64),
            registration_id TEXT NOT NULL REFERENCES workflow_executor_enrollments(registration_id),
            registration_epoch INTEGER NOT NULL CHECK(registration_epoch>0),
            repository TEXT NOT NULL CHECK(length(repository)=32),
            step_id TEXT NOT NULL CHECK(length(step_id) BETWEEN 1 AND 128),
            installed_policy_epoch INTEGER NOT NULL CHECK(installed_policy_epoch>0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=524288),
            UNIQUE(registration_id,registration_epoch,repository,step_id)
         ) STRICT;",
    )?;
    super::workflows::create_schema_v9(tx)?;
    Ok(())
}

pub(super) fn read_collector_profile(
    connection: &rusqlite::Connection,
    profile_id: &str,
) -> Result<Option<CollectorProfile>> {
    let row = connection
        .query_row(
            "SELECT digest,registration_id,registration_epoch,repository,step_id,canonical_json FROM workflow_collector_profiles WHERE profile_id=?1",
            [profile_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(digest, registration_id, registration_epoch, repository, step_id, bytes)| {
            if bytes.len() > 512 * 1024 {
                return Err(Error::Limit(
                    "stored collector profile exceeds 512 KiB".into(),
                ));
            }
            let profile: CollectorProfile = serde_json::from_slice(&bytes)?;
            if profile.profile_id != profile_id
                || profile.registration_id != registration_id
                || profile.registration_epoch != sql_u64("registration epoch", registration_epoch)?
                || profile.repository != repository
                || profile.step_id != step_id
                || profile.canonical_bytes()? != bytes
                || profile.digest()? != digest
            {
                return Err(Error::Invalid(
                    "stored collector profile failed integrity validation".into(),
                ));
            }
            Ok(profile)
        },
    )
    .transpose()
}

pub(super) fn read_current_registration(
    connection: &rusqlite::Connection,
    registration_id: &str,
) -> Result<Option<CurrentExecutorRegistration>> {
    let row = connection
        .query_row(
            "SELECT canonical_json,public_key_hex,enrolled_policy_epoch,executor_id,repository,registration_epoch,verification_key_id FROM workflow_executor_enrollments WHERE registration_id=?1",
            [registration_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?, row.get::<_, String>(6)?)),
        )
        .optional()?;
    row.map(|(canonical, public_key_hex, enrolled_policy_epoch, executor_id, repository, registration_epoch, verification_key_id)| {
        let enrollment = ExecutorEnrollment::from_slice(&canonical)?;
        if enrollment.registration_id != registration_id || enrollment.canonical_bytes()? != canonical
            || enrollment.executor_id != executor_id || enrollment.repository != repository
            || enrollment.registration_epoch != sql_u64("registration epoch", registration_epoch)?
            || enrollment.verification_key_id != verification_key_id {
            return Err(Error::Invalid("stored executor enrollment failed integrity validation".into()));
        }
        validate_public_key(&enrollment, &public_key_hex)?;
        let revocation_count: i64 = connection.query_row("SELECT count(*) FROM workflow_executor_revocations WHERE registration_id=?1", [registration_id], |row| row.get(0))?;
        if revocation_count > 1 { return Err(Error::Invalid("multiple revocations for an immutable registration".into())); }
        let revocation = connection
            .query_row(
                "SELECT canonical_json,revoked_policy_epoch,registration_epoch FROM workflow_executor_revocations WHERE registration_id=?1",
                [registration_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()?
            .map(|(bytes, revoked_epoch, registration_epoch)| {
                let value = ExecutorRevocation::from_slice(&bytes)?;
                if value.canonical_bytes()? != bytes
                    || value.registration_epoch != sql_u64("registration epoch", registration_epoch)?
                    || value.revoked_at_policy_epoch != sql_u64("revoked policy epoch", revoked_epoch)?
                    || !value.matches_binding(&enrollment.repository, &enrollment.binding()?)?
                {
                    return Err(Error::Invalid("stored executor revocation failed integrity validation".into()));
                }
                Ok(value)
            })
            .transpose()?;
        Ok(CurrentExecutorRegistration {
            enrollment,
            public_key_hex,
            enrolled_policy_epoch: sql_u64("enrolled policy epoch", enrolled_policy_epoch)?,
            revocation,
        })
    })
    .transpose()
}

fn validate_public_key(
    enrollment: &ExecutorEnrollment,
    public_key_hex: &str,
) -> Result<VerifyingKey> {
    let bytes = decode_hex_32(public_key_hex)?;
    if issuer_key_id(public_key_hex)? != enrollment.verification_key_id
        || format!("{:x}", Sha256::digest(bytes)) != enrollment.verification_key_sha256
    {
        return Err(Error::Invalid(
            "executor verification key does not match its enrollment identifiers".into(),
        ));
    }
    if !crate::evidence::is_canonical_ed25519_encoding(&bytes) {
        return Err(Error::Invalid(
            "non-canonical Ed25519 executor public key".into(),
        ));
    }
    let key = VerifyingKey::from_bytes(&bytes)
        .map_err(|_| Error::Invalid("invalid Ed25519 executor public key".into()))?;
    if key.is_weak() {
        return Err(Error::Invalid("weak Ed25519 executor public key".into()));
    }
    Ok(key)
}

fn decode_hex_32(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "executor public key must be 32 bytes of lowercase hexadecimal".into(),
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(output)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("hex input was validated"),
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::skill_workflow::ObservationSignatureAlgorithm;

    fn key_material() -> (String, String, String) {
        let public = SigningKey::from_bytes(&[7_u8; 32])
            .verifying_key()
            .to_bytes();
        let public_hex = public
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let key_id = issuer_key_id(&public_hex).unwrap();
        let key_sha256 = format!("{:x}", Sha256::digest(public));
        (public_hex, key_id, key_sha256)
    }

    #[test]
    fn enrollment_is_scoped_epoch_monotonic_and_revocation_is_current() {
        let root = tempfile::tempdir().unwrap();
        let mut authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let (public_key_hex, key_id, key_sha256) = key_material();
        let enrollment = ExecutorEnrollment {
            version: 1,
            registration_id: "registration-1".into(),
            executor_id: "executor-1".into(),
            operator_id: "operator-1".into(),
            repository: repository.clone(),
            step_ids: vec!["tests".into()],
            registration_epoch: 1,
            verification_key_id: key_id,
            verification_key_sha256: key_sha256,
            signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
            issued_at_ms: 1,
        };
        let digest = authority
            .enroll_workflow_executor(&enrollment, &public_key_hex)
            .unwrap();
        assert_eq!(authority.policy_epoch().unwrap(), 1);
        assert_eq!(
            authority
                .enroll_workflow_executor(&enrollment, &public_key_hex)
                .unwrap(),
            digest
        );
        assert_eq!(authority.policy_epoch().unwrap(), 1);

        let mut conflicting = enrollment.clone();
        conflicting.step_ids = vec!["review".into()];
        assert!(matches!(
            authority.enroll_workflow_executor(&conflicting, &public_key_hex),
            Err(Error::Conflict(_))
        ));
        let mut nonmonotonic = enrollment.clone();
        nonmonotonic.registration_id = "registration-2".into();
        assert!(matches!(
            authority.enroll_workflow_executor(&nonmonotonic, &public_key_hex),
            Err(Error::Conflict(_))
        ));

        let revocation = ExecutorRevocation {
            version: 1,
            registration_id: enrollment.registration_id.clone(),
            repository,
            registration_epoch: enrollment.registration_epoch,
            revoked_at_policy_epoch: 2,
            reason: "operator removed executor".into(),
        };
        authority.revoke_workflow_executor(&revocation).unwrap();
        assert_eq!(authority.policy_epoch().unwrap(), 2);
        let current = authority
            .workflow_executor_registration(&enrollment.registration_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.enrollment, enrollment);
        assert_eq!(current.revocation, Some(revocation));
        for sql in [
            "UPDATE workflow_executor_enrollments SET executor_id='different-executor'",
            "UPDATE workflow_executor_enrollments SET registration_epoch=2",
            "UPDATE workflow_executor_enrollments SET verification_key_id=printf('%064d',0)",
            "UPDATE workflow_executor_revocations SET revoked_policy_epoch=3",
            "UPDATE workflow_executor_revocations SET registration_epoch=2",
        ] {
            let tx =
                Transaction::new_unchecked(&authority.connection, TransactionBehavior::Immediate)
                    .unwrap();
            tx.execute(sql, []).unwrap();
            assert!(matches!(
                authority.workflow_executor_registration(&enrollment.registration_id),
                Err(Error::Invalid(_))
            ));
            tx.rollback().unwrap();
        }
    }
    #[test]
    fn native_writer_fence_blocks_policy_writers_until_release() {
        let root = tempfile::tempdir().unwrap();
        let authority = Authority::open(root.path()).unwrap();
        let mut competing = Authority::open(root.path()).unwrap();
        competing
            .connection
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let epoch = authority.policy_epoch().unwrap();
        authority.with_task_writer_fence(|held| {
            assert_eq!(held.policy_epoch()?, epoch);
            let error = competing.advance_policy(epoch).unwrap_err();
            assert!(matches!(error, Error::Sql(rusqlite::Error::SqliteFailure(code, _)) if code.code == rusqlite::ErrorCode::DatabaseBusy));
            // Publication runs while the reservation is held, before any writer can advance.
            assert_eq!(held.policy_epoch()?, epoch);
            Ok(())
        }).unwrap();
        assert_eq!(competing.advance_policy(epoch).unwrap(), epoch + 1);
    }
}
