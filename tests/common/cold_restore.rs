//! Test-only stopped-service filesystem qualification, not a native backup API.
use super::*;
use agentscm::attached_worktree::AttachmentState;
use agentscm::hub::workspace::{MemberChange, Workspace, WorkspaceActor, WorkspaceConfig};
use agentscm::skill_workflow::CollectorTransfer;
use std::collections::BTreeMap;
use std::time::Duration;
use workspace_hub::EnvStorage;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct Entry {
    directory: bool,
    mode: u32,
    length: u64,
    sha256: String,
}
type Manifest = BTreeMap<PathBuf, Entry>;

fn inventory(root: &Path) -> Result<Manifest, &'static str> {
    fn visit(root: &Path, path: &Path, result: &mut Manifest) -> Result<(), &'static str> {
        let metadata = fs::symlink_metadata(path).map_err(|_| "restore_inventory_unavailable")?;
        let directory = metadata.is_dir();
        if !directory && !metadata.is_file() {
            return Err("restore_inventory_file_type");
        }
        let bytes = if directory {
            vec![]
        } else {
            fs::read(path).map_err(|_| "restore_inventory_unavailable")?
        };
        result.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            Entry {
                directory,
                mode: metadata.permissions().mode(),
                length: bytes.len() as u64,
                sha256: digest(&bytes),
            },
        );
        if directory {
            for child in fs::read_dir(path).map_err(|_| "restore_inventory_unavailable")? {
                visit(
                    root,
                    &child.map_err(|_| "restore_inventory_unavailable")?.path(),
                    result,
                )?;
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}

fn checkpoint(root: &Path) {
    use std::io::Read;
    for (relative, entry) in inventory(root).unwrap() {
        if entry.directory {
            continue;
        }
        let path = root.join(relative);
        // A preceding database checkpoint can remove its own WAL/SHM files.
        if !path.exists()
            && ["-wal", "-shm"].iter().any(|suffix| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with(suffix)
            })
        {
            continue;
        }
        let mut header = [0_u8; 16];
        if fs::File::open(&path)
            .unwrap()
            .read_exact(&mut header)
            .is_ok()
            && &header == b"SQLite format 3\0"
        {
            let db = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            db.busy_timeout(Duration::from_secs(2)).unwrap();
            let (busy, log, done): (i64, i64, i64) = db
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .unwrap();
            assert_eq!(busy, 0);
            assert_eq!(log, done);
            db.close().unwrap();
        }
    }
}

fn copy(root: &Path, destination: &Path, manifest: &Manifest) {
    for (relative, entry) in manifest {
        let to = destination.join(relative);
        if entry.directory {
            fs::create_dir_all(&to).unwrap();
        } else {
            fs::copy(root.join(relative), &to).unwrap();
        }
        fs::set_permissions(&to, fs::Permissions::from_mode(entry.mode)).unwrap();
    }
    preflight(destination, manifest).unwrap();
}

fn preflight(root: &Path, expected: &Manifest) -> Result<(), &'static str> {
    if &inventory(root)? != expected {
        return Err("restore_inventory_mismatch");
    }
    Ok(())
}

pub(super) async fn browser_hold(base: &str) {
    let seconds = std::env::var("AGENTSCM_TEST_BROWSER_HOLD_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().expect("browser hold must be seconds"))
        .unwrap_or(0)
        .min(600);
    if seconds == 0 {
        return;
    }
    println!("{base}/ui/#workspace");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let release = std::env::var_os("AGENTSCM_TEST_BROWSER_RELEASE_FILE").map(PathBuf::from);
    while tokio::time::Instant::now() < deadline {
        if release.as_ref().is_some_and(|path| path.is_file()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn offline(storage: &EnvStorage) -> (Arc<dyn agentscm::hub::store::Store>, Arc<Workspace>) {
    let store = agentscm::hub::store::open(&format!(
        "sqlite://{}",
        storage.dir.path().join("hub.sqlite").display()
    ))
    .unwrap();
    let workspace = Arc::new(
        Workspace::open(
            WorkspaceConfig::from_file(&storage.dir.path().join("workspace.json")).unwrap(),
            store.clone(),
        )
        .unwrap(),
    );
    (store, workspace)
}

fn deny(
    storage: &EnvStorage,
    native: &NativeSourceProfile,
    actor: &WorkspaceActor,
    ledger: &Path,
    request: &str,
) {
    // This independent, nonsecret record is read in full, never inferred from backup B.
    let value: Value = serde_json::from_slice(&fs::read(ledger).unwrap()).unwrap();
    let (store, workspace) = offline(storage);
    let member: MemberChange = serde_json::from_value(json!({
        "identity":value["member"], "enabled":false, "roles":["reader","contributor"],
        "repository_ids":["synthetic/project"], "request_id":request,
        "expected_generation":workspace.settings().unwrap().generation
    }))
    .unwrap();
    workspace.put_member(actor, member).unwrap();
    assert!(
        store
            .revoke_agent_token(value["token_id"].as_str().unwrap())
            .unwrap()
    );
    let mut authority = Authority::open(&native.profile.data_dir).unwrap();
    let epoch = authority.policy_epoch().unwrap() + 1;
    authority
        .revoke_workflow_executor(&agentscm::skill_workflow::ExecutorRevocation {
            version: 1,
            registration_id: value["registration_id"].as_str().unwrap().into(),
            repository: native.profile.native_repository.clone(),
            registration_epoch: 1,
            revoked_at_policy_epoch: epoch,
            reason: "synthetic post-backup denial".into(),
        })
        .unwrap();
}

pub(super) fn sync_config(root: &Path) -> agentscm::hub::sync::SyncConfig {
    let values: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("sync-config.json")).unwrap()).unwrap();
    agentscm::hub::sync::SyncConfig::from_lookup(
        "http://127.0.0.1:1",
        root.join("publication-secrets"),
        14,
        |name| values.get(name).cloned(),
    )
    .unwrap()
}

struct Restored {
    env: Env,
    publication: agentscm::hub::HubState,
    task: tokio::task::JoinHandle<()>,
    base: String,
}
impl Restored {
    async fn stop(self) -> EnvStorage {
        self.task.abort();
        let result = tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap();
        assert!(result.is_ok() || result.unwrap_err().is_cancelled());
        drop(self.publication);
        self.env.stop().await
    }
}
async fn reopen(storage: EnvStorage, root: &Path) -> Restored {
    let data = root.to_owned();
    let env = Env::open_existing(storage, move |state, workspace| {
        let mut config = ExecutionConfig::from_file(&data.join("publication-config.json")).unwrap();
        config.attached_publication_profiles.clear();
        config.agent_task_profiles[0].publish_forge_status = false;
        let service = Arc::new(
            ExecutionService::open(&data.join("agent-only.sqlite"), config, workspace).unwrap(),
        );
        state.with_workspace_execution(service)
    })
    .await;
    let service = Arc::new(
        ExecutionService::open(
            &root.join("publication.sqlite"),
            ExecutionConfig::from_file(&root.join("publication-config.json")).unwrap(),
            env.workspace.clone(),
        )
        .unwrap(),
    );
    let publication = agentscm::hub::HubState::new(
        env.store.clone(),
        Arc::new(agentscm::backend::LocalBackend::open(root.join("publication-objects")).unwrap()),
    )
    .with_workspace(env.workspace.clone())
    .with_workspace_execution(service)
    .with_sync(agentscm::hub::sync::Sync::new(sync_config(root)).unwrap());
    let address: String =
        serde_json::from_slice(&fs::read(root.join("publication-listen.json")).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
    let app = agentscm::hub::router(publication.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Restored {
        env,
        publication,
        task,
        base: format!("http://{address}"),
    }
}
async fn publication_request(
    env: &Env,
    base: &str,
    path: &str,
    token: &str,
    body: &Value,
) -> (u16, Value) {
    let response = json_request(
        env.client.post(format!("{base}{path}")).bearer_auth(token),
        body,
    )
    .send()
    .await
    .unwrap();
    let status = response.status().as_u16();
    let value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, value)
}

#[allow(
    clippy::too_many_arguments,
    reason = "One existing joined fixture hands its real retained state to the cold checkpoint."
)]
pub(super) async fn qualify(
    root: &tempfile::TempDir,
    mut storage: EnvStorage,
    native: &NativeSourceProfile,
    profile: &CollectorProfile,
    fake: &mut fake_github::FakeGithub,
    actor: WorkspaceActor,
    token_id: String,
    old_token: &str,
    old_cookie: &str,
    transfer: &CollectorTransfer,
    attachments: &[AttachmentState; 2],
    candidate: &Value,
    publication_id: &str,
    forge_request: &Value,
    recovery: &Value,
    docker_home: &Path,
) {
    let external = tempfile::tempdir().unwrap();
    fs::set_permissions(external.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ledger = external.path().join("denials.json");
    let identity = json!({"provider":"github","host":storage_host(&storage),"user_id":202});
    fs::write(
        &ledger,
        serde_json::to_vec(
            &json!({"member":identity,"token_id":token_id,"registration_id":"registration-http-1"}),
        )
        .unwrap(),
    )
    .unwrap();
    let ledger_bytes = fs::read(&ledger).unwrap();
    assert!(!String::from_utf8_lossy(&ledger_bytes).contains(old_token));
    let effects = fake_docker_effect_counts(&docker_home.join("calls.jsonl"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&fake.state) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake forge connections must release their state before copying");
    let requests = fake.with(|s| s.requests.len());
    let provider = fake.with(|s| s.repos[&100].git.clone().unwrap());
    let push_log = provider.with_extension("push-log");
    let pushes = fs::read(&push_log).unwrap();
    let publication_before = {
        let (store, _workspace) = offline(&storage);
        store
            .publication("synthetic/project", publication_id)
            .unwrap()
            .unwrap()
            .branch_state
    };
    let roots = [root.path().to_owned(), storage.dir.path().to_owned()];
    for path in &roots {
        checkpoint(path);
    }
    let manifests = roots.each_ref().map(|path| inventory(path).unwrap());
    // Every actual file (including journals and source objects) participates, not a placeholder list.
    let object = manifests[0]
        .iter()
        .find(|(p, e)| {
            !e.directory
                && p.starts_with(
                    native
                        .profile
                        .data_dir
                        .strip_prefix(root.path())
                        .unwrap()
                        .join("objects"),
                )
        })
        .unwrap()
        .0
        .clone();
    let required = [
        object,
        PathBuf::from("hub-candidate.git/config"),
        PathBuf::from("collector-state/collector.sqlite"),
        PathBuf::from("collector.key"),
        PathBuf::from("agent.token"),
        PathBuf::from("publication-config.json"),
        PathBuf::from("sync-config.json"),
        PathBuf::from("publication.pat"),
        PathBuf::from("synthetic-git-credential.json"),
    ];
    for path in &required {
        assert!(
            manifests[0]
                .get(path)
                .is_some_and(|e| !e.directory && e.length > 0)
        );
    }
    let backup = external.path().join("backup-b");
    fs::create_dir(&backup).unwrap();
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o700)).unwrap();
    for i in 0..2 {
        copy(&roots[i], &backup.join(i.to_string()), &manifests[i]);
    }
    fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec(&json!({"roots":roots,"entries":manifests})).unwrap(),
    )
    .unwrap();
    // Missing state must fail before a constructor can silently initialize it. Error text is fixed.
    for path in required {
        let missing = backup.join("0").join(&path);
        let hidden = external.path().join("omitted");
        fs::rename(&missing, &hidden).unwrap();
        let error = preflight(&backup.join("0"), &manifests[0]).unwrap_err();
        for private in [
            old_token,
            "synthetic-fixture-only",
            root.path().to_str().unwrap(),
            path.to_str().unwrap(),
        ] {
            assert!(!error.contains(private));
        }
        fs::rename(hidden, missing).unwrap();
    }
    for path in ["workspace.json", "login.json"] {
        let missing = backup.join("1").join(path);
        let hidden = external.path().join("omitted");
        fs::rename(&missing, &hidden).unwrap();
        assert_eq!(
            preflight(&backup.join("1"), &manifests[1]),
            Err("restore_inventory_mismatch")
        );
        fs::rename(hidden, missing).unwrap();
    }
    let manifest_text = fs::read_to_string(backup.join("manifest.json")).unwrap();
    for secret in [
        old_token,
        "synthetic-fixture-only",
        "synthetic-publication-pat",
        "synthetic-secret",
    ] {
        assert!(!manifest_text.contains(secret));
    }
    // Original installation receives all three later denials while listeners remain stopped.
    deny(&storage, native, &actor, &ledger, "original-after-backup");
    for path in &roots {
        checkpoint(path);
    }
    // Move only our own synthetic roots; old roots cannot be selected by restored constructors.
    for i in 0..2 {
        preflight(&backup.join(i.to_string()), &manifests[i]).unwrap();
        fs::rename(&roots[i], external.path().join(format!("isolated-old-{i}"))).unwrap();
        copy(&backup.join(i.to_string()), &roots[i], &manifests[i]);
    }
    assert_eq!(fs::read(&ledger).unwrap(), ledger_bytes);
    assert_eq!(fake.with(|s| s.requests.len()), requests);
    assert_eq!(fs::read(&push_log).unwrap(), pushes);
    assert_eq!(
        fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
        effects
    );
    // Maintenance-only open: no listeners, then complete denial reconciliation at restored generation.
    deny(
        &storage,
        native,
        &actor,
        &ledger,
        "restored-denial-reconciliation",
    );
    fake.restart().await;
    let _socket = std::os::unix::net::UnixListener::bind(root.path().join("docker.sock")).unwrap();
    for restart in 0..2 {
        let restored = reopen(storage, root.path()).await;
        let env = &restored.env;
        let path = format!(
            "{REPO}/profiles/checks/attachments/{}/publication",
            attachments[0].id
        );
        assert!(matches!(
            env.request(
                Method::GET,
                "/v1/hub/workspace/session",
                Some(old_cookie),
                None,
                None,
                None
            )
            .await
            .0,
            401 | 403
        ));
        assert!(
            env.store
                .authenticate(
                    &agentscm::hub::store::agent_token_digest(old_token).unwrap(),
                    agentscm::hub::store::unix_now()
                )
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            publication_request(
                env,
                &restored.base,
                &format!("{path}/forge"),
                old_token,
                forge_request
            )
            .await
            .0,
            401 | 403
        ));
        // Regrant makes current member auth valid while the old session, token and executor remain denied.
        assert_ne!(env.signin(202).await.0, 302);
        {
            let change: MemberChange = serde_json::from_value(json!({"identity":identity,"enabled":true,
                "roles":["reader","contributor"],"repository_ids":["synthetic/project"],"request_id":format!("restore-regrant-{restart}"),
                "expected_generation":env.workspace.settings().unwrap().generation})).unwrap();
            env.workspace.put_member(&actor, change).unwrap();
        }
        assert!(matches!(
            env.request(
                Method::GET,
                "/v1/hub/workspace/session",
                Some(old_cookie),
                None,
                None,
                None
            )
            .await
            .0,
            401 | 403
        ));
        let (_, fresh_cookie) = env.signin(202).await;
        assert_eq!(
            env.request(
                Method::GET,
                "/v1/hub/workspace/session",
                Some(&fresh_cookie),
                None,
                None,
                None
            )
            .await
            .0,
            200
        );
        let user = env
            .store
            .list_users()
            .unwrap()
            .into_iter()
            .find(|u| u.identity.user_id == 202)
            .unwrap();
        let (_, observer) = mint_agent_token(
            &*env.store,
            &user.id,
            "restore-observer",
            "synthetic/project",
            3600,
        )
        .unwrap();
        let report = qualification(
            env,
            observer.expose(),
            &native.profile.data_dir,
            &native.subject,
        )
        .await;
        assert_eq!(report["qualified"], false);
        let descriptor: Value =
            serde_json::from_slice(&fs::read(root.path().join("fresh-descriptor.json")).unwrap())
                .unwrap();
        let task: WorkflowTask = serde_json::from_value(descriptor["task"].clone()).unwrap();
        assert_eq!(task.task_id, transfer.header.task_id);
        assert_eq!(task.digest().unwrap(), transfer.header.task_digest);
        // Direct authority check isolates executor denial from the separately revoked origin grant.
        let authority = Authority::open_read_only(&native.profile.data_dir).unwrap();
        let error = authority
            .require_current_workflow_executor(&task.subject, &transfer.header.executor)
            .expect_err("executor must remain revoked")
            .to_string();
        assert!(error.contains("executor registration is revoked"));
        drop(authority);
        assert_eq!(
            env.request(
                Method::POST,
                "/synthetic/execute?repository=synthetic%2Fproject",
                None,
                None,
                Some(observer.expose()),
                None
            )
            .await
            .0,
            204
        );
        // Current actor is authorized above; revoked origin/executor evidence cannot publish.
        let refused = publication_request(
            env,
            &restored.base,
            &format!("{path}/forge"),
            observer.expose(),
            forge_request,
        )
        .await;
        assert!(matches!(refused.0, 401 | 409), "{refused:?}");
        let status = env.request(Method::POST, MCP_PATH, None, None, Some(observer.expose()), Some(mcp_call("agentscm_task_status", json!({
            "subject":native.subject, "status":{
                "version":1,"task_id":transfer.header.task_id,"task_digest":transfer.header.task_digest,
                "attempt_id":transfer.header.attempt_id,"attempt_digest":transfer.header.attempt_digest,
                "fence":transfer.header.fence,"executor":transfer.header.executor
            }
        })))).await;
        assert_eq!(status.0, 200, "{status:?}");
        let upload = env
            .request(
                Method::POST,
                &format!("{REPO}/profiles/checks/agent-tasks/collector-upload"),
                None,
                None,
                Some(observer.expose()),
                Some(json!({"subject":native.subject,"transfer":transfer})),
            )
            .await;
        assert!(matches!(upload.0, 401 | 403 | 409), "{upload:?}");
        let observed = publication_request(
            env,
            &restored.base,
            &format!("{path}/forge/recover"),
            observer.expose(),
            recovery,
        )
        .await;
        assert_eq!(observed.0, 200, "{observed:?}");
        assert_eq!(observed.1["id"], publication_id);
        let retained = env
            .store
            .publication("synthetic/project", publication_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.branch_state, publication_before);
        let collector = TrustedLocalCollector::new(
            &root.path().join("collector-state"),
            profile.clone(),
            SigningKey::from_bytes(
                &fs::read(root.path().join("collector.key"))
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            collector
                .retained_transfer("transfer-success")
                .unwrap()
                .digest()
                .unwrap(),
            transfer.digest().unwrap()
        );
        let journal =
            AttachmentJournal::open(&native.profile.data_dir, &native.profile.native_repository)
                .unwrap();
        let engine = Engine::open(&native.profile.data_dir).unwrap();
        for expected in attachments {
            assert_eq!(
                serde_json::to_value(journal.state(&expected.id).unwrap()).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert_eq!(
                git(
                    Path::new(&expected.worktree),
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"]
                ),
                expected.git_common_dir
            );
        }
        assert_eq!(
            engine
                .read_file(
                    &attachments[1].candidate_revision,
                    "notes/contributor-b.txt"
                )
                .unwrap(),
            b"independent concurrent edit\n"
        );
        assert_eq!(
            fs::read(root.path().join("contributor-b/notes/contributor-b.txt")).unwrap(),
            b"independent concurrent edit\n"
        );
        assert_eq!(
            git(
                &root.path().join("hub-candidate.git"),
                &["rev-parse", "refs/heads/agentscm"]
            ),
            candidate["candidate"]["commit"].as_str().unwrap()
        );
        assert_eq!(
            fake_docker_effect_counts(&docker_home.join("calls.jsonl")),
            effects
        );
        assert_eq!(fs::read(&push_log).unwrap(), pushes);
        assert_eq!(
            fake.with(|s| s
                .requests
                .iter()
                .filter(|r| r.method == "POST" && r.path_and_query.ends_with("/pulls"))
                .count()),
            1
        );
        drop(engine);
        drop(journal);
        drop(collector);
        let disable: MemberChange = serde_json::from_value(json!({"identity":identity,"enabled":false,
            "roles":["reader","contributor"],"repository_ids":["synthetic/project"],"request_id":format!("restore-disable-{restart}"),
            "expected_generation":env.workspace.settings().unwrap().generation})).unwrap();
        env.workspace.put_member(&actor, disable).unwrap();
        storage = restored.stop().await;
    }
    fake.stop().await;
}

fn storage_host(storage: &EnvStorage) -> String {
    let config = WorkspaceConfig::from_file(&storage.dir.path().join("workspace.json")).unwrap();
    config.initial_owner.host
}
