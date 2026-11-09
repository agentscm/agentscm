//! Synthetic forge, keys and temporary Git; no production transport escape exists.
use super::*;
use crate::backend::LocalBackend;
use crate::hub::connection::NewConnection;
use crate::hub::store::{self, ForgeIdentity, mint_bound_agent_token};
use crate::hub::validation_evidence::{ValidationSummaryConfig, persist_bundle};
use crate::local_validation::bundle::{EvidencePairV1, ValidationBundleV1};
use crate::local_validation::fetch::HostedTestTransport;
use crate::local_validation::*;
use crate::policy::issuers::{CommonControl, IssuerKeyRecord, IssuerRecord, issuer_key_id};
use ed25519_dalek::SigningKey;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;

#[path = "../../../tests/support/fake_github.rs"]
mod fake;

struct Fixture {
    root: TempDir,
    hub: HubState,
    conn: Connection,
    repo: ConnectedRepository,
    forge: fake::FakeGithub,
    agent: String,
    source: std::path::PathBuf,
    remote: std::path::PathBuf,
    base: String,
    head: String,
    registry: IssuerRegistrySnapshot,
    recipe: ValidationRecipe,
    policies: Vec<ValidationPolicy>,
    _transport: HostedTestTransport,
}
fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .current_dir(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let registry = IssuerRegistrySnapshot {
            version: 1,
            repository: "github.com/acme/widgets".into(),
            epoch: 1,
            issuers: vec![IssuerRecord {
                issuer_id: "maintainer".into(),
                trust_domain: "local".into(),
                common_control: CommonControl::Unknown {},
                allowed_check_kinds: vec![CHECK_KIND.into()],
                keys: vec![IssuerKeyRecord {
                    key_id: issuer_key_id(&public).unwrap(),
                    public_key_hex: public,
                    not_before_ms: 0,
                    not_after_ms: i64::MAX as u64,
                    revoked_at_ms: None,
                }],
            }],
        };
        let recipe = ValidationRecipe {
            recipe_id: "gate".into(),
            commands: vec![RecipeCommand {
                id: "test".into(),
                argv: vec!["true".into()],
                timeout_secs: 30,
                test_selection: None,
                paths: None,
            }],
            dependency_files: None,
            toolchain_probes: None,
            constituents: None,
        };
        let policies: Vec<_> = ["unit", "static"]
            .iter()
            .map(|id| ValidationPolicy {
                repository: registry.repository.clone(),
                recipe_sha256: recipe.sha256().unwrap(),
                required_services: Services {
                    postgres: false,
                    s3: false,
                },
                max_age_seconds: 3600,
                epoch: 1,
                exempt_paths: None,
                check_id: Some((*id).into()),
            })
            .collect();
        let config = source.join(CONFIG_DIR);
        fs::create_dir_all(config.join("static")).unwrap();
        fs::write(config.join("checks.json"),r#"{"schema_version":1,"checks":[{"name":"Unit","dir":"."},{"name":"Static","dir":"static"}]}"#).unwrap();
        fs::write(
            config.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        for (index, dir) in [".", "static"].iter().enumerate() {
            fs::write(
                config.join(dir).join("policy.json"),
                serde_json::to_vec(&policies[index]).unwrap(),
            )
            .unwrap();
            fs::write(
                config.join(dir).join("recipe.json"),
                serde_json::to_vec(&recipe).unwrap(),
            )
            .unwrap();
        }
        fs::write(source.join("base"), "base").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        fs::write(source.join("change"), "first").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "head"]);
        let head = git(&source, &["rev-parse", "HEAD"]);
        let remote = fake::bare_clone(root.path(), &source);
        git(&remote, &["update-ref", "refs/heads/main", &base]);
        git(&remote, &["update-ref", "refs/pull/1/head", &head]);
        let transport = HostedTestTransport::enter(&remote);
        let forge = fake::FakeGithub::start().await;
        forge.with(|s| {
            let mut repo = fake::Repo::new(42, "acme/widgets");
            repo.git = Some(remote.clone());
            let mut pull = fake::Pull::new(1, "topic", &["change"]);
            pull.head_sha = head.clone();
            repo.pulls.insert(1, pull);
            repo.branches.push(fake::Branch {
                name: "main".into(),
                sha: base.clone(),
                committed_at: "2026-01-01T00:00:00Z".into(),
            });
            s.repos.insert(42, repo);
            s.pats.insert(
                "synthetic-token".into(),
                ("publisher".into(), 7, [42].into()),
            );
            s.pushers.insert("synthetic-token".into(), [42].into());
        });
        let store = store::open(&format!(
            "sqlite://{}",
            root.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let owner = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 7,
                },
                "publisher",
            )
            .unwrap();
        let conn = store
            .insert_connection(
                &NewConnection {
                    id: store::new_connection_id().unwrap(),
                    provider: "github".into(),
                    host: "github.com".into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("publisher".into()),
                },
                &owner.id,
            )
            .unwrap();
        let repo = store
            .upsert_repository(&conn.id, "github.com", 42, "acme/widgets", None, None)
            .unwrap();
        store
            .configure_validation_summary(&ValidationSummaryConfig {
                repository: repo.id.clone(),
                connection: conn.id.clone(),
                enabled: true,
            })
            .unwrap();
        let (agent, _, _) = mint_bound_agent_token(
            &*store,
            &owner.id,
            "synthetic",
            "acme/widgets",
            42,
            3600,
            false,
        )
        .unwrap();
        let config = crate::hub::sync::SyncConfig::from_lookup(
            "http://127.0.0.1:7332",
            root.path().join("secrets"),
            30,
            |name| match name {
                "AGENTSCM_GITHUB_API_URL" => Some(forge.base.clone()),
                _ => None,
            },
        )
        .unwrap();
        let hub = HubState::new(
            store,
            Arc::new(LocalBackend::open(root.path().join("objects")).unwrap()),
        )
        .with_sync(crate::hub::sync::Sync::new(config).unwrap());
        Self {
            root,
            hub,
            conn,
            repo,
            forge,
            agent,
            source,
            remote,
            base,
            head,
            registry,
            recipe,
            policies,
            _transport: transport,
        }
    }
    fn submit(&self, check: usize, issued: u64, wrong_signature: bool) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let record = ValidationRecord {
            subject: GitValidationSubject {
                repository: self.registry.repository.clone(),
                commit: self.head.clone(),
                tree: git(&self.source, &["rev-parse", "HEAD^{tree}"]),
            },
            context: GitValidationContext {
                base_ref: "refs/heads/main".into(),
                merge_base: self.base.clone(),
            },
            base_tip: self.base.clone(),
            recipe_sha256: self.recipe.sha256().unwrap(),
            results: vec![CommandResult {
                id: "test".into(),
                argv: vec!["true".into()],
                exit_code: Some(0),
                stop: StopReason::Exited,
                duration_ms: 1,
                stdout_sha256: "a".repeat(64),
                stderr_sha256: "b".repeat(64),
                truncated: false,
                incomplete: false,
            }],
            not_required: vec![],
            selections: vec![],
            services: Services {
                postgres: false,
                s3: false,
            },
            platform: "synthetic".into(),
            toolchain: vec!["synthetic".into()],
            tree_unchanged_after: true,
        };
        let registry_digest = self.registry.digest().unwrap();
        let issuance = Issuance {
            issuer_id: "maintainer",
            key_id: &self.registry.issuers[0].keys[0].key_id,
            attempt_id: "synthetic",
            run_generation: 1,
            issued_at_ms: issued,
            expires_at_ms: issued + 86_400_000,
            issuer_registry_sha256: &registry_digest,
            dependencies: &[],
        };
        let (payload, record) =
            evidence_payload(&record, &self.recipe, &self.policies[check], &issuance).unwrap();
        let mut envelope = crate::evidence::sign_evidence(payload, &key).unwrap();
        if wrong_signature {
            envelope.signature_hex = "a".repeat(128);
        }
        let bundle = ValidationBundleV1 {
            version: 1,
            entries: vec![EvidencePairV1::new(
                &serde_json::to_vec(&envelope).unwrap(),
                &record,
            )],
        };
        let durable = persist_bundle(
            &*self.hub.objects,
            &self.repo.id,
            &self.registry.repository,
            &bundle,
        )
        .unwrap();
        self.hub
            .store
            .receive_validation_evidence(&self.agent, 1, &durable, super::super::store::unix_now())
            .unwrap();
    }
    async fn run(&self) -> Gh<()> {
        reconcile_repo(
            &self.hub,
            &self.conn,
            &self.repo,
            "synthetic-token",
            Some(1),
        )
        .await
    }
    fn journal(&self) -> ValidationPublication {
        self.hub
            .store
            .validation_publication(&self.repo.id, 1)
            .unwrap()
            .unwrap()
    }
    fn counts(&self) -> (usize, usize) {
        self.forge.with(|s| {
            (
                s.repos[&42].comments.get(&1).map_or(0, Vec::len),
                s.repos[&42].statuses.len(),
            )
        })
    }
    fn now() -> u64 {
        super::super::store::unix_now() as u64 * 1000
    }
    fn advance(&mut self) {
        fs::write(self.source.join("change"), "second").unwrap();
        git(&self.source, &["add", "."]);
        git(&self.source, &["commit", "-qm", "second"]);
        self.head = git(&self.source, &["rev-parse", "HEAD"]);
        git(
            &self.source,
            &[
                "push",
                self.remote.to_str().unwrap(),
                &format!("{}:refs/pull/1/head", self.head),
            ],
        );
        self.forge.with(|s| {
            s.repos
                .get_mut(&42)
                .unwrap()
                .pulls
                .get_mut(&1)
                .unwrap()
                .head_sha = self.head.clone()
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_requirements_update_noop_and_new_head_keep_one_identity() {
    let mut f = Fixture::new().await;
    let now = Fixture::now();
    f.submit(0, now, false);
    f.run().await.unwrap();
    let first = f.journal().comment_id;
    assert_eq!(f.counts(), (1, 4));
    f.forge.with(|s| {
        let statuses = &s.repos[&42].statuses;
        assert_eq!(statuses[2].1["state"], "success");
        assert_eq!(statuses[3].1["state"], "pending");
    });
    f.submit(1, now + 1, false);
    f.run().await.unwrap();
    assert_eq!(f.journal().comment_id, first);
    let counts = f.counts();
    f.run().await.unwrap();
    assert_eq!(f.counts(), counts);
    f.advance();
    f.run().await.unwrap();
    assert_eq!(f.journal().comment_id, first);
    f.forge.with(|s| {
        let repo = &s.repos[&42];
        assert!(
            repo.statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        );
        assert!(repo.comments[&1][0].body.contains("none for current head"));
    });
    f.submit(0, now + 2, false);
    f.submit(1, now + 3, false);
    f.run().await.unwrap();
    assert_eq!(f.counts().0, 1);
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "success")
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_comment_reply_recovers_exact_marker_without_duplicate() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.forge.with(|s| s.lose_next_comment_reply = true);
    assert!(f.run().await.is_err());
    assert_eq!(f.journal().phase, PublicationPhase::Attempting);
    let counts = f.counts();
    f.run().await.unwrap();
    assert_eq!(f.journal().phase, PublicationPhase::Idle);
    assert_eq!(f.counts(), counts);
    f.run().await.unwrap();
    assert_eq!(f.counts(), counts);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_status_never_retries_or_creates_comment() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.forge.with(|s| s.lose_next_status_reply = true);
    assert!(f.run().await.is_err());
    assert_eq!(f.journal().phase, PublicationPhase::Attempting);
    let counts = f.counts();
    assert_eq!(counts, (0, 1));
    assert!(f.run().await.is_err());
    assert_eq!(f.counts(), counts);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_marker_is_never_adopted_and_wrong_signature_cannot_pass() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), true);
    f.forge.with(|s| {
        s.repos.get_mut(&42).unwrap().comments.insert(
            1,
            vec![fake::Comment {
                id: 99,
                body: format!("{}forged -->", MARKER),
                author: "outsider".into(),
                author_association: "OWNER".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                html_url: None,
            }],
        )
    });
    f.run().await.unwrap();
    assert_eq!(f.counts().0, 2);
    assert_ne!(f.journal().comment_id, Some(99));
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .all(|(_, v)| v["state"] != "success")
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_archive_invalidates_previously_green_contexts() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.submit(1, Fixture::now() + 1, false);
    f.run().await.unwrap();
    fs::remove_dir_all(f.root.path().join("objects")).unwrap();
    f.run().await.unwrap();
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        );
        assert!(
            s.repos[&42].comments[&1][0]
                .body
                .contains("Verification unavailable")
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definite_deleted_comment_recreates_after_complete_discovery() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.run().await.unwrap();
    let first = f.journal().comment_id;
    f.forge.with(|s| {
        s.repos
            .get_mut(&42)
            .unwrap()
            .comments
            .get_mut(&1)
            .unwrap()
            .clear()
    });
    f.run().await.unwrap();
    assert_eq!(f.counts().0, 1);
    assert!(f.journal().comment_id.is_some());
    // The fake may reuse deleted numeric IDs; production's ID is retained in either case.
    assert!(first.is_some());
}

#[test]
fn renderer_bounds_history_and_escapes_requirement_text() {
    let snapshot = ValidationSnapshot {
        config: None,
        generation: 1000,
        entries: (1..=1000)
            .map(
                |generation| super::super::validation_evidence::ValidationEvidenceReceipt {
                    repository: "repo".into(),
                    proposal: 1,
                    generation,
                    bundle_sha256: "a".repeat(64),
                    received_at: 0,
                    received_by: "synthetic".into(),
                    state: "received_unverified".into(),
                },
            )
            .collect(),
    };
    let body = render(
        &[Row {
            requirement: "<script>|".into(),
            state: VerdictState::Pending,
            reason: "missing".into(),
            head: "a".repeat(40),
            envelope: String::new(),
            omissions: vec![],
        }],
        &snapshot,
        None,
    )
    .unwrap();
    assert!(body.len() < 6000);
    assert!(!body.contains("<script>"));
    assert!(body.contains("1000 submitted generations"));
    assert_eq!(body.matches("## Local validation").count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_change_during_status_sequence_invalidates_and_releases_owner() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.submit(1, Fixture::now() + 1, false);
    f.forge
        .with(|s| s.advance_base_after_status_count = Some((3, f.head.clone())));
    assert!(f.run().await.is_err());
    assert_eq!(f.journal().phase, PublicationPhase::Idle);
    assert_eq!(f.counts().0, 0);
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .any(|(_, v)| v["state"] == "success")
        );
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_workers_cannot_send_duplicate_summaries() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    let (a, b) = tokio::join!(f.run(), f.run());
    // A worker observing the other owner's in-flight Attempting state reports
    // blocked. At least one worker completes or safely fences a preparation.
    assert!(a.is_ok() || b.is_ok());
    // A preparation may be fenced by the other worker; a later poll completes it.
    for _ in 0..3 {
        f.run().await.unwrap();
    }
    assert_eq!(f.counts().0, 1);
    assert_eq!(f.journal().phase, PublicationPhase::Idle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_head_across_bases_cannot_pass() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.submit(1, Fixture::now() + 1, false);
    f.forge.with(|s| {
        let repo = s.repos.get_mut(&42).unwrap();
        let mut pull = repo.pulls[&1].clone();
        pull.number = 2;
        pull.base_ref = "release".into();
        repo.pulls.insert(2, pull);
    });
    f.run().await.unwrap();
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .all(|(_, v)| v["state"] == "pending")
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_receipt_inventory_uses_fence_only_to_invalidate() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.run().await.unwrap();
    let db = rusqlite::Connection::open(f.root.path().join("hub.sqlite")).unwrap();
    db.execute("DELETE FROM hub_validation_evidence", [])
        .unwrap();
    assert!(
        f.hub
            .store
            .validation_snapshot(&f.repo.id, 1, MAX_RECEIPTS)
            .is_err()
    );
    assert_eq!(
        f.hub
            .store
            .validation_fence(&f.repo.id, 1)
            .unwrap()
            .generation,
        1
    );
    f.run().await.unwrap();
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_discovery_cannot_preserve_old_green_statuses() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.run().await.unwrap();
    f.forge
        .with(|s| s.arrayless_path = Some("/comments".into()));
    assert!(f.run().await.is_err());
    assert_eq!(f.journal().phase, PublicationPhase::Idle);
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_pull_does_not_block_later_repository_pulls() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.forge.with(|s| s.lose_next_status_reply = true);
    assert!(f.run().await.is_err());
    git(&f.remote, &["update-ref", "refs/pull/2/head", &f.base]);
    f.forge.with(|s| {
        let repo = s.repos.get_mut(&42).unwrap();
        let mut pull = fake::Pull::new(2, "other", &[]);
        pull.head_sha = f.base.clone();
        repo.pulls.insert(2, pull);
    });
    assert!(
        reconcile_repo(&f.hub, &f.conn, &f.repo, "synthetic-token", None)
            .await
            .is_err()
    );
    assert_eq!(f.journal().phase, PublicationPhase::Attempting);
    assert!(
        f.hub
            .store
            .validation_publication(&f.repo.id, 2)
            .unwrap()
            .unwrap()
            .comment_id
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_outer_pull_inventory_invalidates_known_idle_journals() {
    let f = Fixture::new().await;
    f.submit(0, Fixture::now(), false);
    f.run().await.unwrap();
    let first = store_call(|| {
        f.hub
            .store
            .validation_publication_proposals(&f.repo.id, 0, None, 1)
    })
    .unwrap();
    assert_eq!(first.proposals, vec![1]);
    assert_eq!(first.through, 1);
    assert!(
        store_call(|| f.hub.store.validation_publication_proposals(
            &f.repo.id,
            1,
            Some(first.through),
            1
        ))
        .unwrap()
        .proposals
        .is_empty()
    );
    f.forge.with(|s| {
        let repo = s.repos.get_mut(&42).unwrap();
        for number in 2..=301 {
            repo.pulls
                .insert(number, fake::Pull::new(number, "synthetic", &[]));
        }
    });
    assert!(
        reconcile_repo(&f.hub, &f.conn, &f.repo, "synthetic-token", None)
            .await
            .is_err()
    );
    assert_eq!(f.journal().phase, PublicationPhase::Idle);
    f.forge.with(|s| {
        assert!(
            s.repos[&42]
                .statuses
                .iter()
                .rev()
                .take(2)
                .all(|(_, v)| v["state"] == "pending")
        )
    });
}
