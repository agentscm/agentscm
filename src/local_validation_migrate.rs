//! Explicit legacy transport migration. Provenance is local audit metadata, never issuer authority.
use super::*;
use agentscm::evidence::{
    ArtifactInput, CurrentIssuerTrust, IntegrityStatus, SuppliedArtifact, verify_evidence,
};
use agentscm::hub::{forge_http, github::Api};
use agentscm::local_validation::bundle::{EvidencePairV1, MAX_BUNDLE_BYTES, ValidationBundleV1};

#[derive(Debug, Args)]
pub struct MigrateArgs {
    #[arg(long)]
    state_dir: PathBuf,
    /// Canonical GitHub OWNER/NAME; GitLab and Forgejo are unsupported.
    #[arg(long)]
    github_repo: String,
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    proposal: i64,
    #[arg(long)]
    github_token_file: PathBuf,
    #[arg(long)]
    hub_token_file: PathBuf,
    #[arg(long)]
    hub_url: String,
    #[arg(long)]
    repository_id: String,
    /// Trusted registry relative to the default-branch root, read at its pinned commit.
    #[arg(long, default_value = ".github/local-validation/registry.json")]
    registry_path: String,
    /// Stop all standalone writers before selecting cleanup; GitHub has no body-CAS delete.
    #[arg(long)]
    delete_legacy: bool,
    #[arg(long, default_value = "https://api.github.com", hide = true)]
    api_url: String,
}
fn github_error(_: agentscm::hub::github::GhError) -> Error {
    Error::Invalid("GitHub migration request failed; no automatic mutation retry".into())
}
fn validate_pair(
    envelope: &[u8],
    record: &[u8],
    registry: &IssuerRegistrySnapshot,
    clock: EvaluationClock,
) -> Result<()> {
    EvidencePairV1::new(envelope, record).decode()?;
    let report = verify_evidence(
        envelope,
        registry,
        clock,
        &[ArtifactInput {
            artifact_id: "record",
            supplied: SuppliedArtifact::Full(record),
        }],
    )?;
    let payload = &agentscm::evidence::EvidenceEnvelope::from_slice(envelope)?.payload;
    let requirement = payload.requirement_id.as_str();
    let valid_requirement = requirement == "local-validation"
        || requirement
            .strip_prefix("local-validation/")
            .is_some_and(|id| {
                id.len() <= 40
                    && id
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
    if payload.check_kind != agentscm::local_validation::CHECK_KIND || !valid_requirement {
        return Err(Error::Invalid(
            "legacy envelope is not local-validation evidence; unresolved".into(),
        ));
    }
    if report.integrity() != IntegrityStatus::Verified
        || report.current_issuer_trust() != CurrentIssuerTrust::Trusted
        || !registry.issuers.iter().any(|issuer| {
            issuer.issuer_id == payload.issuer_id
                && issuer.allowed_check_kinds.contains(&payload.check_kind)
        })
    {
        return Err(Error::Invalid(
            "legacy comment has invalid or currently untrusted signed evidence; unresolved".into(),
        ));
    }
    // Expired evidence and old source/registry bindings may be archived. This command
    // never evaluates a current-head policy or writes a passing status.
    Ok(())
}

pub async fn run(args: MigrateArgs) -> Result<i32> {
    private_state_dir(&args.state_dir)?;
    for token in [&args.github_token_file, &args.hub_token_file] {
        if !token.is_absolute() {
            return Err(Error::Invalid("token files must be absolute".into()));
        }
    }
    let parts: Vec<_> = args.github_repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err(Error::Invalid("invalid canonical GitHub repository".into()));
    }
    // Allow only the production API or explicit loopback synthetic fixtures.
    let api_base = forge_http::check_base_url(&args.api_url)
        .map_err(|_| Error::Invalid("invalid GitHub API URL".into()))?;
    if api_base != "https://api.github.com" && !api_base.starts_with("http://127.0.0.1:") {
        return Err(Error::Invalid(
            "migration supports canonical GitHub only".into(),
        ));
    }
    let path = Path::new(&args.registry_path);
    if !path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        || args.registry_path.is_empty()
    {
        return Err(Error::Invalid(
            "registry path must be relative and normal".into(),
        ));
    }
    let token = read_token(&args.github_token_file)?;
    let api = Api::new(&api_base, "https://github.com")
        .map_err(|_| Error::Invalid("could not create GitHub client".into()))?;
    let metadata = api
        .repo_json(&token, &args.github_repo)
        .await
        .map_err(github_error)?
        .ok_or_else(|| Error::Invalid("GitHub repository unavailable".into()))?;
    if metadata["full_name"].as_str() != Some(args.github_repo.as_str()) {
        return Err(Error::Invalid(
            "GitHub canonical repository identity differs".into(),
        ));
    }
    let default = api
        .default_branch(&token, &args.github_repo)
        .await
        .map_err(github_error)?;
    let commit = api
        .ref_sha(&token, &args.github_repo, &default)
        .await
        .map_err(github_error)?
        .ok_or_else(|| Error::Invalid("default branch commit unavailable".into()))?;
    check_git_sha(&commit)?;
    let registry_bytes = api
        .file(
            &token,
            &args.github_repo,
            &args.registry_path,
            &commit,
            MAX_INPUT_BYTES as usize,
        )
        .await
        .map_err(github_error)?
        .ok_or_else(|| Error::Invalid("trusted registry unavailable".into()))?;
    let registry = IssuerRegistrySnapshot::from_slice(&registry_bytes)?;
    if registry.repository != format!("github.com/{}", args.github_repo) {
        return Err(Error::Invalid(
            "default-branch issuer registry repository mismatch".into(),
        ));
    }
    let (_, login) = api.user(&token).await.map_err(github_error)?;
    let comments = api
        .validation_comments(&token, &args.github_repo, args.proposal, 30)
        .await
        .map_err(github_error)?;
    let clock = EvaluationClock::Known {
        unix_ms: now_ms().ok_or_else(|| Error::Invalid("system clock unavailable".into()))?,
    };
    let mut legacy = Vec::new();
    // Finish the entire bounded discovery and signature preflight before any deletion.
    for (id, author, body) in comments {
        if !body.contains(comment::MARKER_PREFIX) {
            continue;
        }
        let (envelope, record) = comment::parse(&body)?;
        validate_pair(&envelope, &record, &registry, clock)?;
        if args.delete_legacy && !author.eq_ignore_ascii_case(&login) {
            return Err(Error::Invalid(format!(
                "legacy comment {id} belongs to another author; unresolved, cleanup requires that author's own migration"
            )));
        }
        legacy.push((id, author, body, envelope, record));
    }
    let base = forge_http::check_base_url(&args.hub_url)
        .map_err(|_| Error::Invalid("invalid Hub URL".into()))?;
    let hub_token = read_token(&args.hub_token_file)?;
    let client =
        forge_http::client().map_err(|_| Error::Invalid("could not create Hub client".into()))?;
    let destination =
        json!({"hub_url": base, "repository_id": args.repository_id, "proposal": args.proposal});
    let destination_digest = agentscm::store::digest(&serde_json::to_vec(&destination)?);
    // Archive ALL eligible comments and confirm every readback before deleting ANY.
    let mut archived = Vec::new();
    for (id, author, body, envelope, record) in &legacy {
        let sha = envelope_sha256(envelope);
        retain_submission(&args.state_dir, &format!("{sha}.record.json"), record)?;
        retain_submission(&args.state_dir, &format!("{sha}.envelope.json"), envelope)?;
        let receipt = submit_retained(SubmitArgs {
            state_dir: args.state_dir.clone(),
            attestation: vec![sha],
            hub_url: base.to_string(),
            repository_id: args.repository_id.clone(),
            proposal: args.proposal,
            token_file: args.hub_token_file.clone(),
        })
        .await?;
        let (status, value) = forge_http::send(
            client
                .get(format!(
                    "{base}/v1/hub/repos/{}/validation-evidence/{}/{}",
                    args.repository_id, args.proposal, receipt.generation
                ))
                .bearer_auth(&hub_token),
            MAX_BUNDLE_BYTES,
        )
        .await
        .map_err(|_| {
            Error::Invalid(
                "archived Hub evidence readback unavailable; legacy comments retained".into(),
            )
        })?;
        if status != 200 {
            return Err(Error::Invalid(
                "archived Hub evidence readback refused; legacy comments retained".into(),
            ));
        }
        let bundle = ValidationBundleV1::from_slice(&serde_json::to_vec(&value)?)?;
        let expected = ValidationBundleV1 {
            version: 1,
            entries: vec![EvidencePairV1::new(envelope, record)],
        }
        .to_bytes()?;
        if bundle.to_bytes()? != expected
            || agentscm::store::digest(&expected) != receipt.bundle_sha256
        {
            return Err(Error::Invalid(
                "archived Hub evidence readback differs; legacy comments retained".into(),
            ));
        }
        let body_digest = agentscm::store::digest(body.as_bytes());
        let journal_name = format!("migration-{destination_digest}-{id}-{body_digest}.json");
        let journal = json!({"version":1, "github_repo": args.github_repo, "proposal":args.proposal,
            "comment_id":id, "author":author, "body_sha256":agentscm::store::digest(body.as_bytes()),
            "registry_commit":commit, "registry_sha256":registry.digest()?, "destination":destination,
            "receipt":receipt, "state":"archived_readback_confirmed"});
        retain_submission(
            &args.state_dir,
            &journal_name,
            &serde_json::to_vec(&journal)?,
        )?;
        archived.push((journal_name, journal));
    }
    if args.delete_legacy {
        // A live edit since discovery refuses the entire cleanup before the first delete.
        for (id, author, body, _, _) in &legacy {
            if let Some((live_author, live_body)) = api
                .validation_comment(&token, &args.github_repo, *id)
                .await
                .map_err(github_error)?
                && (live_author != *author
                    || live_body != *body
                    || !live_author.eq_ignore_ascii_case(&login))
            {
                return Err(Error::Conflict(format!(
                    "legacy comment {id} changed; cleanup refused"
                )));
            }
        }
        for ((id, author, body, _, _), (name, mut journal)) in legacy.iter().zip(archived) {
            // Check immediately before this deletion too. Writers must remain stopped:
            // GitHub DELETE has no atomic condition on the observed body.
            let live = api
                .validation_comment(&token, &args.github_repo, *id)
                .await
                .map_err(github_error)?;
            if let Some((live_author, live_body)) = live {
                if live_author != *author
                    || live_body != *body
                    || !live_author.eq_ignore_ascii_case(&login)
                {
                    return Err(Error::Conflict(format!(
                        "legacy comment {id} changed; cleanup refused"
                    )));
                }
                journal["state"] = json!("delete_attempting");
                retain_submission(&args.state_dir, &name, &serde_json::to_vec(&journal)?)?;
                let deleted = api
                    .delete_validation_comment(&token, &args.github_repo, *id)
                    .await;
                if deleted.is_err()
                    && api
                        .validation_comment(&token, &args.github_repo, *id)
                        .await
                        .map_err(github_error)?
                        .is_some()
                {
                    return Err(Error::Invalid(format!(
                        "legacy comment {id} delete outcome unresolved; rerun migration after inspecting it"
                    )));
                }
            }
            journal["state"] = json!("deleted_or_absent");
            retain_submission(&args.state_dir, &name, &serde_json::to_vec(&journal)?)?;
        }
        let remaining = api
            .validation_comments(&token, &args.github_repo, args.proposal, 30)
            .await
            .map_err(github_error)?;
        if remaining
            .iter()
            .any(|(_, _, body)| body.contains(comment::MARKER_PREFIX))
        {
            return Err(Error::Conflict(
                "legacy comments remain; migration unresolved".into(),
            ));
        }
    }
    println!(
        "{}",
        json!({"archived_comments":legacy.len(), "cleanup_requested":args.delete_legacy,
        "legacy_comments_remaining":if args.delete_legacy {0} else {legacy.len()},
        "current_head_validated":false})
    );
    Ok(0)
}
fn check_git_sha(sha: &str) -> Result<()> {
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("invalid default branch commit".into()));
    }
    Ok(())
}
